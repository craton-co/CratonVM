#[allow(unused_imports)]
use super::super::*;

pub(crate) use super::splice_args::reflective_varargs_box_site;

/// One attempt of [`ir_tier`], planning no splice at the caller pcs in
/// `ir_splice_skip`. When the builder refuses inside a spliced body (and
/// `CRATONVM_JIT_IR_SPLICE_REBUILD` is on), `ir_rebuild_without` names the
/// top-level site to drop for the next attempt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_tier_attempt(
    req: &CompileRequest<'_>,
    admission: &compile_gate::CompileAdmission,
    metrics: &metrics::CompileRecorder,
    code: &[u8],
    code_len: usize,
    scan: &x64::JitScanResult,
    prologue_param_slots: usize,
    optimize: bool,
    precise_exception_frames: bool,
    resolved_string_layout: Option<StringFieldLayout>,
    ir_method_hash: u64,
    ir_refusal_key: u64,
    ir_refusal_ticket: ir_evidence::IrRefusalTicket,
    ir_refused_before: bool,
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): build an OSR-door
    // compile without the door's own every-header opaque merges
    // (`osr_narrow_opaque_door_enabled`). `false` for every other compile.
    osr_narrow_opaque: bool,
    ir_splice_skip: &[usize],
    ir_rebuild_without: &mut Option<usize>,
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): stage the exception
    // table for compiled handler entries, and report whether it was staged.
    catch_arms: bool,
    catch_arms_staged: &mut bool,
    // JIT round 19 wave 6 (lane osr6, OSR5-2): build with
    // `ir::Graph::promote_pending_withheld` (the retry after a refused
    // promoted-loop write-back). `false` for every first attempt.
    promote_pending_withheld: bool,
    // JIT round 21 (lane lambda3, `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`):
    // build with no lambda impl-call plan (the retry after a refused attempt
    // that had one), and report whether this attempt planned any.
    lambda_plan_withheld: bool,
    lambda_plan_built: &mut bool,
    // j27 wave 4 (lane SPLICEFFM2): the top-level splice sites whose
    // `MemorySegment` element calls this attempt's build left as calls; the
    // attempt was refused for them (`ffm_element_inline::spliced_ffm_declined_sites`).
    ffm_splice_declined: &mut Vec<usize>,
) -> Option<CompiledMethod> {
    // JIT round 19 (lane splice19, CH16-4): the chain-snapshot census clock
    // (`CRATONVM_DBG_IR_CHAIN_SNAPSHOTS`), started only when the line is on.
    let chain_census_t0 = false.then(std::time::Instant::now);
    // `CRATONVM_FROZEN_PAGES`: the classes the planner's rules treat as a
    // frozen object's (`frozen_facts::compile_has_class`), for this attempt.
    let _frozen_classes = frozen_facts::CompileClasses::of_facts(req.frozen_facts.map(|f| &**f));
    let CompileRequest {
        branch_counter_resolver: _,
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        cp_field_volatile_resolver,
        cp_invoke_resolver,
        cp_invokespecial_owner_resolver,
        callee_compiler,
        cp_new_resolver,
        cp_ldc_resolver,
        cp_ldc2w_resolver,
        profile,
        helpers,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        ir_emit_calls,
        ir_emit_special_calls,
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
        ir_inline_resolver,
        jdk_only,
        direct_bind_admission,
        cp_invoke_declaring_class_resolver,
        direct_helpers,
        ..
    } = *req;
    // The compiling VM's verdict registry (cycle sets), read by the bind ladders.
    let verdicts = req.verdicts();
    // Every conjunct above passed: the optimizing pipeline is entered. The
    // report records this before anything can decline, so a later
    // `enter_single_pass` is recognisable as a FALL-THROUGH rather than a
    // method that was never a C2 candidate at all.
    metrics.enter_optimizing_pipeline();
    note_ir_pipeline_entered();
    // Includes the implicit `this` slot for instance methods — see
    // `prologue_param_slots` above.
    let num_params = prologue_param_slots;
    // Compact-layout metadata for this method's field reads, for the
    // lowerer's guarded inline `getfield`. Filled alongside the builder's
    // `field_info` below.
    let mut ir_compact_fields: std::collections::HashMap<(usize, bool), (u32, bool, u8)> =
        std::collections::HashMap::new();
    // Arm the per-compile evidence slot. Everything between here and the
    // acceptance check below runs on this thread; see `ir_evidence`.
    //
    // Scoped: every IR bail between here and the acceptance `take()` (a
    // builder refusal, the graph-size cap, a verifier or schedule failure)
    // used to leave this entry armed on the thread's stack. The next outer
    // compile then popped the stale inner entry instead of its own and was
    // judged — and possibly memoised as refused — on the wrong evidence.
    let _evidence_scope = ir_evidence::begin_compile_scope();
    let mut builder = ir::IrBuilder::new(num_params, cached.max_locals as usize);
    builder.set_root_caller_frame(req.caller_mirror_slot, req.caller_security_walk_ignored);
    builder.set_object_class_id(req.object_class_id);
    if let Some(refresher) = req.const_folds_refresher {
        builder.set_const_folds(refresher());
    } else if let Some(cf) = req.const_folds {
        builder.set_const_folds(cf.clone());
    }
    // JIT r22 wave 2, lane inline (W16L3-6 of `jit-r21-nio-proposals-w16-L3.md`):
    // every builder census line (`field-load memo`, `static-load memo`, the
    // trap lines) names its method from the start, not `<method>` when the
    // compile has no invoke labels and no field traps. Diagnostic only, built
    // only under the same flags those lines print under.
    if x64::dbg_jitc_enabled() || false {
        builder.set_method_label(
            format!(
                "{}.{}{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            )
            .into_boxed_str(),
        );
    }
    // Interpreter round i5 wave 102, lane L2: the method's throw-stamp token
    // (`ir::Graph::throw_stamp_token`, `CRATONVM_JIT_THROW_STAMP_OWNER`).
    builder.graph.throw_stamp_token = crate::implicit_null::method_stamp_token(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    );
    // THIS compile's `java/lang/String` field layout — the same one the
    // single-pass backend is handed — for the String accessor expander. A
    // builder has no other route to one (the process-wide seed that could
    // carry another VM's field indices is retired; see
    // `ir::PUBLISHED_STRING_LAYOUT`), and without it every String call site
    // dispatches.
    builder.set_string_layout(resolved_string_layout);
    // t1 wave 10 (lane LCAP2, `CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED`): a
    // context-free published body gets no implicit exception edges, whose
    // pads would give the C2 body a context its baked callers cannot follow.
    builder.set_implicit_throw_edges_withheld(
        r25_w10_2_steady_c1::installed_body_withholds_implicit_edges(
            req.installed_body_needs_context,
        ),
    );
    // JIT round 21 wave 20 (lane sbfuse, STRINTR21-4;
    // `CRATONVM_JIT_IR_SB_CHAIN_FUSE`): the method's explicit
    // `new StringBuilder()...toString()` chains, each built as one concat.
    let vthread_guarded = direct_helpers.vthread_state_offset != 0
        && direct_helpers.vthread_state_mask != 0
        && x64::jit_thread_tls_disp() != 0
        && !cached.is_synchronized
        && !cached.class_name.starts_with("java/")
        && !cached.class_name.starts_with("jdk/")
        && !cached.class_name.starts_with("sun/")
        && !cached.class_name.starts_with("com/sun/");
    let sb_chain_fusions = if vthread_guarded {
        builder.vthread_guard = Some((
            direct_helpers.vthread_state_offset,
            direct_helpers.vthread_state_mask,
        ));
        Default::default()
    } else {
        ir_sb_chain_fusion_plan(req, code, code_len)
    };
    if !sb_chain_fusions.is_empty() {
        if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] sb-chain-fuse {}.{}{}: {} chain(s) planned as one concat",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                sb_chain_fusions.len(),
            );
        }
        builder.set_sb_chain_fusions(sb_chain_fusions);
    }
    // nio21 wave 19 (lane L3): the VM's `ThreadLocal.get()` layout, for the
    // builder's expansion of that site (`IrBuilder::try_thread_local_get_intrinsic`).
    let nio_charset_layout = ir::NioCharsetIrLayout::from_table(direct_helpers);
    builder.set_thread_local_layout(ir::ThreadLocalIrLayout::from_table(direct_helpers));
    builder.set_nio_charset_layout(nio_charset_layout);
    // JIT round 21 (lane refs2; re-landed t1 wave 9, lane REFS2): the
    // `refersTo` expansion's referent index, which does not wait for the
    // `ThreadLocal` classes (`CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT`).
    builder.set_refers_to_referent_index(ir::refers_to_referent_index_from_table(direct_helpers));
    // t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`): the
    // referent's compact row, so a compact receiver's read is inline.
    builder.set_refers_to_referent_row(ir::refers_to_referent_row_from_table(direct_helpers));
    // j27 wave 1 (lane TYPECHK, `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW`):
    // the same row for the spliced `Reference.get` referent reads.
    builder.set_spliced_referent_row(ir::spliced_referent_row_from_table(direct_helpers));
    // JIT round 21 (lane tlsub, TL321-4 / TL321-1; re-landed t1 wave 8, lane
    // RL-TLWALK): the receiver profiles, the VM's one-class query and the
    // de-spec registry that expansion reads.
    builder.set_thread_local_site_plan(ir::ThreadLocalSitePlan::for_compile(
        profile,
        Some(*direct_helpers),
        req.despec.cloned(),
        format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        ),
    ));
    // JIT round 23 wave 9 (lane W9-L2, CF-W9-3): the helper table the
    // constant-`VarHandle` fresh-receiver build asks its plans through
    // (`IrBuilder::try_vh_const_fresh_field_access`).
    builder.set_vh_fresh_helpers(Some(*direct_helpers));

    // JIT round 21 (lane refs2): the `refersTo` expansion's referent index,
    // which does not wait for the `ThreadLocal` classes.
    builder.set_refers_to_referent_index(ir::refers_to_referent_index_from_table(direct_helpers));
    // JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
    // the `WeakReference.get()` / `SoftReference.get()` expansion's layout.
    builder.set_reference_get_layout(ir::ReferenceGetIrLayout::from_table(direct_helpers));
    // JIT round 21 (lane tlsub, TL321-4 / TL321-1): the receiver profiles, the
    // VM's one-class query and the de-spec registry that expansion reads.
    builder.set_thread_local_site_plan(ir::ThreadLocalSitePlan::for_compile(
        profile,
        Some(*direct_helpers),
        req.despec.cloned(),
        format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        ),
    ));
    // JIT round 21 (lane strintr2, STRINTR21-1,
    // `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW`): the String accessor
    // sites whose deopting guard the VM has withdrawn are built as calls.
    if req.despec.is_some() && ir_string_access_trap_withdraw_enabled() {
        let withdrawn = string_access_withdrawn_pcs(
            req.despec.map(|r| &**r),
            &format!(
                "{}.{}:{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            ),
            &scan.invoke_ops,
        );
        if !withdrawn.is_empty() {
            builder.set_string_access_withdrawn_pcs(withdrawn);
        }
    }
    // The call sites the compile door found unresolved (interpreter round i1
    // wave 50, lane L1; `JitFirstCallCell`): each is planned and built as a
    // plain call -- no arithmetic or unbox lowering, no splice, no elision --
    // so the lowerer's first-execution guard has a call to ride on.
    let ir_first_call: std::collections::HashMap<usize, usize> =
        first_call_cells_of(req, &scan.invoke_ops)
            .into_iter()
            .collect();
    if !ir_first_call.is_empty() {
        builder.set_first_call_sites(ir_first_call.clone());
    }
    // The one fact the optimizing tier cannot derive for itself: whether
    // parameter 0 is a receiver. `num_params` above already counts the
    // implicit `this`, so the flag is the only missing half, and it is
    // right here.
    builder.graph.receiver_param = if cached.is_static { None } else { Some(0) };
    // JIT round 25 item 2: the access flag itself, for the shadow drop rule's
    // receiver admission (`ir::Graph::method_synchronized`).
    builder.graph.method_synchronized = Some(cached.is_synchronized);
    // Round 14 wave 5 (lane sync5, SS8-3): an `ACC_SYNCHRONIZED` instance
    // method's optimizing body runs only with its receiver's monitor held (the
    // door's wrapped entry -- every publication stamps
    // `requires_wrapped_entry` from `is_synchronized` -- or the interpreter
    // frame an OSR transfer leaves); see `ir::Graph::method_monitor_param`.
    if cached.is_synchronized && !cached.is_static && ir::ir_sync_method_monitor_facts_enabled() {
        builder.graph.method_monitor_param = Some(0);
    }
    // Round 15 wave 1 (lane sync, SS8-2): under
    // `CRATONVM_JIT_IR_SELF_LOCKING_SYNC`, the body takes its receiver's
    // monitor itself (`ir::MethodSelfLock`); every entry into it then holds the
    // monitor by the body's own enter instead of the door's, which is the
    // hold `method_monitor_param` describes. Asked with the predicate
    // `single_pass_self_lock_preferred` stepped aside for.
    //
    // Round 15 wave 3 (lane sync2, S15-3): a `static synchronized` candidate
    // locks its class mirror, read from the request's GC-maintained mirror
    // slot (`ir::IrBuilder::set_method_self_lock_mirror`).
    //
    // Round 15 wave 3 (lane sync2, page
    // `r15w3-chain2-sync-method-chains-refuse-self-locking-bodies`): bound
    // once, because a self-locking body keeps FLAT frames -- the only shape
    // whose hand-over the VM recognises (`deopt::handed_method_monitor_index`
    // refuses a chain) -- so the three sync-method chain conditions below
    // (`CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`) step aside for it instead of
    // planning chain points `ir_lower::Lowerer::self_lock_refusal` would then
    // refuse the whole body for.
    let ir_self_lock_requested = ir_self_lock_candidate(
        verdicts,
        cached,
        code,
        code_len,
        admission.door(),
        req.self_lock_mirror_slot,
    );
    if ir_self_lock_requested {
        builder.set_method_self_lock(true);
        if cached.is_static {
            builder.set_method_self_lock_mirror(
                cached.declaring_class_id.as_u32(),
                req.self_lock_mirror_slot,
            );
        }
    }
    // j27 wave 3 (lane BACKLOG, CN16-3): the three sync-method chain
    // conditions below, decided once (`deopt::ir_method_admits_chains`).
    let ir_sync_chains_ok =
        deopt::ir_method_admits_chains(cached.is_synchronized, ir_self_lock_requested);
    // Round 14 wave 7 (lane sync7, RV6-1): the `static synchronized` twin. The
    // body runs holding its declaring class's mirror (JVMS 2.11.10) on every
    // entry the instance fact names; see `ir::Graph::method_monitor_class`.
    // The method's own `ldc` entries the VM resolves (loaded, no loading run)
    // to that very class are the ones whose `holdsLock` folds.
    if cached.is_synchronized
        && cached.is_static
        && ir::ir_sync_static_method_monitor_facts_enabled()
    {
        let own_class = cached.declaring_class_id.as_u32();
        if own_class != 0 {
            builder.graph.method_monitor_class = Some(own_class);
            let mut own_cps: std::collections::HashSet<(u32, u16)> =
                std::collections::HashSet::new();
            if let (Some(ldc), Some(new_site)) = (cp_ldc_resolver, cp_new_resolver) {
                for &(_, cp_idx) in &scan.ldc_ops {
                    let holder = match ldc(cp_idx) {
                        Some(JitLdcConstant::ClassMirror {
                            holder_class_id,
                            cp_idx: c,
                        })
                        | Some(JitLdcConstant::ClassMirrorSlot {
                            holder_class_id,
                            cp_idx: c,
                            ..
                        }) if c == cp_idx => holder_class_id,
                        _ => continue,
                    };
                    let names_own_class = matches!(
                        new_site(cp_idx),
                        Some(JitNewSite::Resolved { class_id, .. }) if class_id == own_class
                    );
                    if holder == own_class && names_own_class {
                        own_cps.insert((holder, cp_idx));
                    }
                }
            }
            builder.set_method_monitor_class_cps(own_cps);
        }
    }
    // The adapter fold is armed by the constant pool, not by the method's name
    // (interpreter round i1 wave 10): see `lambda_int_to_double_runs_proven`.
    builder.tdigest_scalar_kernel = match (cp_invoke_resolver, cp_class_name_resolver) {
        (Some(invoke), Some(class_name)) => {
            lambda_int_to_double_runs_proven(code, code_len, invoke, class_name)
        }
        _ => false,
    };
    // Type each `Param` node from the descriptor. Two consumers depend on
    // this:
    //   * inc 25 (long gate): re-lay-out the parameter locals with the JVM
    //     two-slot category-2 convention (a `long`/`double` param occupies
    //     two slots) so `lload`/`lstore` of a later long param reads the
    //     right slot.
    //   * real-frame-deopt type source: a ref-typed param (an instance
    //     method's `this`, an object/array argument) must be `IrType::Ref`
    //     so the deopt snapshot tags its slot `StackSlotRef` → `Value::Object`
    //     on resume, instead of a truncated `Value::Int`. Without this the
    //     receiver of an instance method that deopts at, e.g., a div-by-zero
    //     guard would resume with a garbage `this`.
    // For an all-category-1 signature this is layout-identical to `new`'s
    // one-slot-per-param placement — only the node *type* changes, which is
    // codegen-neutral (spill/reload are always 64-bit REX.W; ref operands
    // are never width-sensitive arithmetic), so it is now applied
    // unconditionally rather than only under the long gate.
    let ptypes = ir_param_types(&cached.method_descriptor, cached.is_static);
    builder.set_param_types(&ptypes);
    // JVMS §6.5 `ireturn` narrowing for a `Z`/`B`/`C`/`S` return, the same
    // four tags the interpreter bridge narrows. A compiled caller reads the
    // return register raw, so the body has to narrow before it returns.
    builder.set_return_descriptor(&cached.method_descriptor);
    // COV-03: admit `J` / `F`+`D` INSTANCE FIELD accesses from the same two
    // flags this admission chain evaluates. A wide field is the one way a
    // category-2 or FP value can enter the graph with no category-2/FP
    // OPCODE in the body (`getfield J; invokestatic (J)V` has neither), so
    // `method_uses_category2` / `method_uses_fp` would admit such a method
    // with the long/FP tier off and the builder would then produce
    // `IrType::Long`/`Double` nodes it does not support. Telling the builder
    // directly keeps the premise those gates rest on true rather than
    // assuming it.
    builder.set_wide_field_gates(ir_emit_long, ir_emit_fp);
    if ir_emit_long || ir_emit_fp {
        // inc 26 (long) / inc 35 (double): resolve `ldc2_w` constants to
        // `(pc → (bits, is_double))` so the builder lowers a `long` to
        // `Op::Const(Long)` (`lconst`) and a `double` to `Op::ConstF`
        // (`dconst`). inc 35 lifts the inc-30 FP-gate `ldc2_w` exclusion now
        // that the builder disambiguates by `is_double` (a `double` constant
        // is admitted under `ir_emit_fp`). An unresolved `ldc2_w` is omitted →
        // that opcode bails to single-pass.
        if !scan.ldc2w_ops.is_empty() {
            if let Some(resolver) = cp_ldc2w_resolver {
                let mut lm = std::collections::HashMap::with_capacity(scan.ldc2w_ops.len());
                for &(pc, cp_idx) in &scan.ldc2w_ops {
                    if let Some(v) = resolver(cp_idx) {
                        lm.insert(pc, v);
                    }
                }
                builder.set_ldc2w_info(lm);
            }
        }
    }
    // cov-01 increment 1: resolve `ldc`/`ldc_w` (0x12/0x13) constants for
    // the IR builder. `getstatic` + `ldc`/`ldc_w` was 189 of the 273
    // opcode-gap events measured on 2026-08-03 — 69% of every opcode the
    // optimizing builder had no arm for
    // (`ir-coverage-survey-20260803.md`).
    //
    // Three site kinds, three tables, all fed from the SAME resolver the
    // single-pass backend uses — this lane consumes a table the caller
    // already computes, it does not resolve a constant pool:
    //
    //   * `Immediate` → `(bits, is_float)`. A constant node and nothing
    //     else: no memory edge, no safepoint, no GC interaction. An `int`
    //     constant is admitted unconditionally; a `float` constant only
    //     under `ir_emit_fp`, mirroring the `ldc2_w` gate directly above —
    //     the builder would otherwise emit an `Op::ConstF`/`Float` into a
    //     graph the FP tier is switched off for.
    //   * `String` → the CP-indexed SITE, served by
    //     `helpers.ldc_string_cp`: the value is materialised at run time
    //     because an `ObjectRef` baked at compile time can relocate between
    //     two runs of the body, and it is keyed by the SITE rather than by
    //     the literal's bytes because that is the key JVMS §5.4.3's
    //     recorded resolution is filed under.
    //   * `ClassMirror` → the CP-indexed site, served by
    //     `helpers.ldc_class_cp` for the same reason plus one more:
    //     resolution can load a class, which runs arbitrary Java.
    //
    // A pc absent from all three (no resolver, a `MethodHandle` /
    // `MethodType` / condy entry, an unwired class or string helper, or a
    // float with the FP gate off) makes the builder's 0x12/0x13 arm bail
    // that method to single-pass — the pre-existing behaviour, only
    // narrower.
    //
    // The `ir_ldc_strings` keep-alive that stood here is GONE with the
    // bytes it kept alive: an `ldc <String>` site no longer bakes an
    // address into the body, so there is nothing for the artifact to
    // outlive.
    // cov-05: `instanceof`/`checkcast` target class-name bytes, whose
    // ADDRESS the lowered body bakes as an imm64 argument to
    // `helpers.instanceof_check`/`helpers.checkcast`. These are NOT kept
    // alive on the
    // `CompiledMethod`: they are interned process-wide by
    // `intern_typecheck_class_name`, because the type-check helpers
    // memoize on the `(ptr, len)` pair in thread-locals that outlive any
    // one compiled method. Populated below, near the `new_info` /
    // `anewarray_info` construction they share a resolver with.
    // cov-05: `jit_checkcast`'s definitive-refusal path constructs a
    // `ClassCastException` through `jit_thread_mut()` (the JIT_THREAD
    // TLS), same requirement `jit_getstatic`'s `<clinit>` has — the
    // `!has_dispatch` fast entry never sets that TLS. Set below, near
    // where the `getstatic` lane sets the same flag for the same reason.
    let mut ir_needs_dispatch_for_checkcast = false;
    // Round 14 wave 3 (lane calls, I7-4): each `ldc <String>` site's hash, for
    // `ir_fold_null_checks_on_fresh_allocations_with` below.
    let mut ldc_string_hashes: std::collections::HashMap<(u32, u16), i32> =
        std::collections::HashMap::new();
    // Round 14 wave 4 (lane calls2, C14W3-4): each `ldc <String>` site's text,
    // for `ir_fold_string_literal_queries` below.
    let mut ldc_string_texts: std::collections::HashMap<(u32, u16), Vec<u16>> =
        std::collections::HashMap::new();
    // t1 wave 9 (lane LCAP): a string or class `ldc` row, which gives the
    // single-pass body the context form (`x64::driver`, `ldc_string_info` /
    // `ldc_class_info`) although the bytecode scan's `needs_heap` stays false.
    // See `r25_w10_2_steady_c1::ldc_context_abi_floor`.
    let mut ir_ldc_takes_context = false;
    if !scan.ldc_ops.is_empty() {
        if let Some(resolver) = cp_ldc_resolver {
            let mut imm: std::collections::HashMap<usize, (i64, bool)> =
                std::collections::HashMap::new();
            let mut strs: std::collections::HashMap<usize, (u32, u16)> =
                std::collections::HashMap::new();
            let mut classes: std::collections::HashMap<usize, (u32, u16)> =
                std::collections::HashMap::new();
            // Round 9 wave 8b (irl8 request 1): the GC-maintained constant
            // slots the IR tier's `ldc` probe reads before its helper call,
            // collected exactly as `build_single_pass_tables` does. The
            // builder only consults a slot for a site also in
            // `strs`/`classes`, so a slot whose helper is unwired is ignored.
            let mut ldc_slots: std::collections::HashMap<(u32, u16), usize> =
                std::collections::HashMap::new();
            // JIT r23 wave 4b (lane L2, `CRATONVM_JIT_IR_LDC_CONDY_SLOT`):
            // `pc -> slot` of each resolved reference condy the builder reads
            // as an `Op::IndyRootRef`.
            let mut condy_slots: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            let ir_condy = ir::ir_ldc_condy_slot_enabled();
            for &(pc, cp_idx) in &scan.ldc_ops {
                let resolved = resolver(cp_idx);
                if let Some(
                    JitLdcConstant::StringSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    }
                    | JitLdcConstant::ClassMirrorSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    },
                ) = &resolved
                {
                    if *slot_addr != 0 {
                        ldc_slots.insert((*holder_class_id, *cp_idx), *slot_addr);
                    }
                }
                match resolved {
                    Some(JitLdcConstant::Immediate { bits, is_float }) => {
                        if !is_float || ir_emit_fp {
                            imm.insert(pc, (bits, is_float));
                        }
                    }
                    Some(JitLdcConstant::String {
                        holder_class_id,
                        cp_idx,
                    })
                    | Some(JitLdcConstant::StringSlot {
                        holder_class_id,
                        cp_idx,
                        ..
                    }) => {
                        // `ldc_string_cp` is an OptionalPtr, exactly like
                        // `ldc_class_cp` below: omit the site rather than
                        // plan a CALL to address 0, and let the builder
                        // bail the method.
                        if helpers.ldc_string_cp != 0 {
                            strs.insert(pc, (holder_class_id, cp_idx));
                            // Round 14 wave 3 (lane calls, I7-4): the site's
                            // `String.hashCode()`, for the literal hash fold.
                            if let Some(hash) =
                                req.cp_ldc_string_hash_resolver.and_then(|r| r(cp_idx))
                            {
                                ldc_string_hashes.insert((holder_class_id, cp_idx), hash);
                            }
                            if let Some(text) =
                                req.cp_ldc_string_utf16_resolver.and_then(|r| r(cp_idx))
                            {
                                ldc_string_texts.insert((holder_class_id, cp_idx), text);
                            }
                        }
                    }
                    Some(JitLdcConstant::ClassMirror {
                        holder_class_id,
                        cp_idx,
                    })
                    | Some(JitLdcConstant::ClassMirrorSlot {
                        holder_class_id,
                        cp_idx,
                        ..
                    }) => {
                        // `ldc_class_cp` is an OptionalPtr — a hand-built
                        // test helper table leaves it 0. Omit the site
                        // rather than plan a CALL to address 0; the builder
                        // then bails the method, exactly as the single-pass
                        // arm does for the same condition.
                        if helpers.ldc_class_cp != 0 {
                            classes.insert(pc, (holder_class_id, cp_idx));
                        }
                    }
                    // JIT r23 wave 4b (lane L2): a resolved reference condy is
                    // an IR row only under `CRATONVM_JIT_IR_LDC_CONDY_SLOT`
                    // (default OFF): the read of its never-cleared slot. Off,
                    // or unslotted, it has no row and the builder bails the
                    // method to single-pass, which emits the slotted site
                    // (`JitLdcConstant::DynamicSlot`).
                    Some(JitLdcConstant::DynamicSlot { slot_addr, .. })
                        if ir_condy && slot_addr != 0 =>
                    {
                        condy_slots.insert(pc, slot_addr);
                    }
                    Some(JitLdcConstant::DynamicSlot { .. } | JitLdcConstant::Dynamic { .. })
                    | None => {}
                }
            }
            builder.set_ldc_info(imm);
            ir_ldc_takes_context = !strs.is_empty() || !classes.is_empty();
            builder.set_ldc_string_info(strs);
            builder.set_ldc_class_info(classes);
            builder.set_ldc_slot_info(ldc_slots);
            builder.set_ldc_condy_slot_info(condy_slots);
            // JIT r20 wave 12 (lane qMisc, NIO20-L3-1): the
            // `x.getClass() == C.class` pairs the builder may fold.
            if ir_getclass_exact_fold_enabled() {
                if let (
                    Some(fold),
                    Some(new_site),
                    Some(invoke),
                    Some(class_name),
                    Some(declaring),
                ) = (
                    req.getclass_exact_fold_resolver,
                    cp_new_resolver,
                    cp_invoke_resolver,
                    cp_class_name_resolver,
                    cp_invoke_declaring_class_resolver,
                ) {
                    builder.set_getclass_exact_compares(ir_getclass_exact_compares(
                        code,
                        code_len,
                        &scan.ldc_ops,
                        resolver,
                        new_site,
                        invoke,
                        declaring,
                        class_name,
                        fold,
                    ));
                }
            }
        }
    }
    // cov-01 increment 4: resolve `getstatic` (0xb2) sites for the IR
    // builder — the largest single opcode in the survey (92 events). Same
    // resolver and same `(class_id, field_index, type_tag, is_volatile)`
    // tuple the single-pass backend's 0xb2 arm consumes; an unresolvable
    // site is omitted and the builder bails that method.
    //
    // Both `getstatic` and `putstatic` (0xb3) sites are fed: the scanner
    // records both in `static_field_ops` (`x64/bytecode_compat.rs`), and the
    // builder's `0xb3` arm lowers a write to its `jit_putstatic_*` helper
    // (`Op::StoreStatic`, round 11 wave 10) -- the helper, never an inline
    // store, because a static reference WRITE owes an SATB pre-barrier that
    // no collector `set_field` barrier covers (statics live in a Rust-side
    // table), and only those helpers pay it. The value-tier gate below
    // applies to a write's width exactly as to a read's.
    //
    // The class ids this collects are ALSO recorded on the finished
    // artifact (`static_init_classes`, below), because compiled code reads
    // static storage directly and the interpreter's compiled-entry path is
    // what ensure-initializes the declaring classes once per artifact.
    //
    // The VALUE TIER is gated here and not in the builder, because this is
    // the only place that knows the width. `getstatic` is polymorphic: it
    // is listed by neither `is_category2_opcode` nor `is_float_opcode`, so
    // a method whose only wide or floating-point content is a static read
    // has `method_uses_category2() == false` and `fp_in_body() == false`,
    // and is admitted to the optimizing pipeline through the INT clause.
    // Feeding it a `J` site with `ir_emit_long` off would then put a `Long`
    // node in a graph the long tier is switched off for, and a `D`/`F` site
    // an FP node with the FP tier off.
    //
    // Same shape as the float half of the `ldc` feed above, and as the
    // `ldc2_w` feed's `if ir_emit_long || ir_emit_fp`: the party that
    // resolved the width is the party that decides. A gated-off site is
    // simply absent, so the builder's 0xb2 arm bails that method to
    // single-pass, which compiles every width.
    let mut ir_static_init_classes: Vec<u32> = Vec::new();
    // How many of this method's own `getstatic` sites read a constant
    // `VarHandle` over an instance field (`static_varhandle_resolver`). A
    // `VarHandle.get*` site is only a fold candidate in a method that has one.
    let mut ir_vh_plan_count: usize = 0;
    // nio21 wave 12 L2: see `ir_vh_const_site_admitted`.
    let mut ir_vh_const_getstatics: Vec<(usize, u8, u32, usize)> = Vec::new();
    // JIT r23 wave 4 (lane L8, `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`): the
    // layout facts of this method's own `getstatic`s of a filed `static final`
    // `ValueLayout`, keyed `(class id, static field index)`.
    let mut ir_ffm_layout_facts: std::collections::HashMap<
        (u32, u32),
        ffm_element_inline::FfmLayoutFacts,
    > = std::collections::HashMap::new();
    // ... and the element sites planned for the inline expansion, by pc.
    let mut ir_ffm_inline_sites: std::collections::HashMap<
        usize,
        ffm_element_inline::FfmElementInlineSite,
    > = std::collections::HashMap::new();
    // JIT round 24 (`CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC`): the second class
    // of each site planned bimorphic (`ir_ffm_element_bimorphic_plan`).
    let mut ir_ffm_bimorphic_seconds: std::collections::HashMap<
        usize,
        ffm_element_inline::FfmSegmentShape,
    > = std::collections::HashMap::new();
    // JIT r23 wave 11 (lane L4, `CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP`):
    // the element sites no execution has reached yet, built as an unstable-if
    // trap instead of refusing the method (`ir_ffm_element_unreached_site`).
    let mut ir_ffm_unreached_sites: std::collections::HashMap<
        usize,
        ffm_element_inline::FfmElementAccessor,
    > = std::collections::HashMap::new();
    // JIT r23 wave 13 (lane L4, `CRATONVM_JIT_IR_FFM_UNREACHED_SEED_CALL`): the
    // element sites whose unreached trap already fired but whose receiver row is
    // still empty, kept an inline-cache CALL with its profile seed armed
    // (`ir_ffm_element_seed_call_site`) instead of refusing the method.
    let mut ir_ffm_seed_call_sites: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    // JIT r23 wave 12 (lane L7, `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP`): every
    // other never-reached call-site intrinsic, by pc: `(operand entries,
    // return tag)` (`ir_unreached_call_site_shape`).
    let mut ir_unreached_call_sites: std::collections::HashMap<usize, (usize, u8)> =
        std::collections::HashMap::new();
    // Round 16 wave 16 (lane monitor4, MON3-1): `Graph::direct_static_fields`.
    let mut ir_direct_static_fields: Vec<(u32, u32)> = Vec::new();
    // Round 17 wave 2 (lane irfeed): `Graph::trusted_final_static_fields`.
    let mut ir_trusted_final_static_fields: Vec<(u32, u32)> = Vec::new();
    if !scan.static_field_ops.is_empty() {
        if let Some(resolver) = cp_static_field_resolver {
            let mut sm = std::collections::HashMap::with_capacity(scan.static_field_ops.len());
            // Round 17 wave 2 (lane irfeed): the VM's trusted-final answer,
            // asked only when the block coarsening could read it (its switch,
            // or `CRATONVM_DBG_JITC`'s census) AND the trust switch is on.
            // JIT round 23 close: and the static-final loop hoist, which
            // moves a trusted final's read out of a loop that writes memory
            // (`loop_static_hoist::static_final_hoist_enabled`). The
            // coarsening still asks its own trust switch before it reads one.
            let trust_final_of = req.cp_static_field_final_resolver.filter(|_| {
                (ir_optimize::ir_loop_lock_trust_static_final_enabled()
                    && (ir_optimize::ir_loop_lock_coarsen_blocks_enabled() || false))
                    || ir_optimize::static_final_hoist_enabled()
            });
            let mut final_rows: Vec<(u32, usize)> = Vec::new();
            for &(pc, cp_idx) in &scan.static_field_ops {
                if let Some((class_id, field_index, type_tag, is_volatile)) = resolver(cp_idx) {
                    if matches!(type_tag, b'L' | b'[')
                        && !is_volatile
                        && trust_final_of.is_some_and(|is_final| is_final(cp_idx))
                    {
                        final_rows.push((class_id, field_index));
                    }
                    let admitted_by_value_tier = match type_tag {
                        b'J' => ir_emit_long,
                        b'D' | b'F' => ir_emit_fp,
                        _ => true,
                    };
                    if admitted_by_value_tier {
                        sm.insert(pc, (class_id, field_index, type_tag, is_volatile));
                    }
                    // Recorded regardless of the value tier: this list is
                    // the ensure-init obligation, which the declaring class
                    // owes whether or not the IR lowers the read. It also
                    // matches the single-pass artifact, which records every
                    // static site including every `putstatic`.
                    ir_static_init_classes.push(class_id);
                }
            }
            // Round 16 wave 14 (lane osrir, MON3-2): the classes whose statics
            // the lowering will read directly -- initialised now, so a
            // `getstatic` of theirs runs no `<clinit>` -- for the builder's
            // static-load memo. Asked only with that memo's switch on (JIT
            // round 21, lane staticfwd: or its store half's, OI-4).
            if ir::ir_static_load_forward_enabled() || ir::ir_static_store_forward_enabled() {
                builder.set_static_initialized_classes(
                    sm.values()
                        .filter(|&&(class_id, field_index, _, _)| {
                            direct_helpers
                                .resolve_static_base(class_id, field_index)
                                .is_some()
                        })
                        .map(|&(class_id, _, _, _)| class_id)
                        .collect(),
                );
            }
            // Round 16 wave 16 (lane monitor4, MON3-1): the fields whose read
            // is the direct load, for the loop block coarsening's repeated
            // monitor read. Asked only with that switch on, or under
            // `CRATONVM_DBG_JITC`, whose census plans the loop either way.
            // JIT round 19 (lane irops19, IRLOWER17-1): also under the
            // direct-static LICM switch, whose hoist reads the same list.
            // JIT round 19 wave 2 (lane irops2, IROPS19-1): and under the
            // static alias-class switch, whose effect model reads it too.
            // nio20 wave 6 (lane L4): and for the loop static-read hoist
            // (`ir_optimize::loop_static_hoist`), which moves only a read the
            // lowering takes directly.
            if ir_optimize::ir_loop_lock_coarsen_blocks_enabled()
                || ir_optimize::ir_licm_direct_static_enabled()
                || ir::ir_static_alias_class_enabled()
                || ir_optimize::loop_static_hoist_enabled()
                // JIT round 20 wave 11 (lane pLoop, OSR4-2): and for the
                // loop static promotion, which promotes only direct cells.
                || ir_optimize::loop_static_promote_enabled()
                // JIT r20 wave 11 review (lane rev11): and for LICM's
                // direct-read barrier exemption, which asks the same list
                // (`Graph::direct_static_access`); without this row its
                // A/B arm was silently tied to the promotion's switch.
                || ir_optimize::licm_static_read_not_barrier_enabled()
                // JIT round 21 wave 1 (lane loops, LOOPS21-3; consistency,
                // default ON, `CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED=0`
                // restores the list without it): and for the fresh-object
                // loop promotion, which admits a body `getstatic` only when it
                // is on this list. Without the row, that pass's A/B arm was
                // silently tied to three unrelated switches.
                || ir_optimize::fresh_promote_static_feed_enabled()
                // And for the overwritten-`putstatic` removal, which drops a
                // store only to a cell whose class is initialised.
                || ir_optimize::overwritten_static_store_enabled()
                || false
            {
                ir_direct_static_fields = sm
                    .values()
                    .filter_map(|&(class_id, field_index, _, _)| {
                        direct_helpers.resolve_static_base(class_id, field_index)?;
                        Some((class_id, u32::try_from(field_index).ok()?))
                    })
                    .collect();
            }
            // Round 17 wave 2 (lane irfeed;
            // `r17-monitor2-static-final-feed-needs-a-final-bit-FIXED-20261005.md`):
            // the `static final` reference fields the VM trusts, kept only
            // where the direct read is admitted too -- `resolve_static_base`
            // answers for an initialised class only, and never for
            // `java/lang/System` (`System.setOut` rewrites its finals).
            // `ir_optimize::static_final_monitor_is_trusted` asks both lists.
            if !final_rows.is_empty() {
                let mut rows: Vec<(u32, u32)> = final_rows
                    .into_iter()
                    .filter_map(|(class_id, field_index)| {
                        direct_helpers.resolve_static_base(class_id, field_index)?;
                        Some((class_id, u32::try_from(field_index).ok()?))
                    })
                    .collect();
                rows.sort_unstable();
                rows.dedup();
                ir_trusted_final_static_fields = rows;
            }
            // Constant `VarHandle` fields among this method's own `getstatic`
            // sites, for `IrBuilder::try_emit_vh_field_read`. Asked of the VM
            // here, per static reference read, and never of the heap: the
            // resolver answers from a table filled on a mutator thread when
            // the declaring class finished initialising.
            if ir_vh_field_fold_enabled() {
                if let Some(resolve_vh) = req.static_varhandle_resolver {
                    let plans: Vec<((u32, u32), StaticVarHandleField)> = sm
                        .values()
                        .filter(|&&(_, _, type_tag, _)| type_tag == b'L')
                        .filter_map(|&(class_id, field_index, _, _)| {
                            let field_index = u32::try_from(field_index).ok()?;
                            resolve_vh(class_id, field_index)
                                .map(|plan| ((class_id, field_index), plan))
                        })
                        .collect();
                    ir_vh_plan_count += plans.len();
                    builder.add_vh_field_plans(plans);
                }
            }
            // nio21 wave 12 L2: this method's own `getstatic`s of a filed
            // constant-`VarHandle` field, `(pc, kind, class id, static field
            // index)`, for `ir_vh_const_site_admitted`.
            if ir_vh_const_admit_enabled() {
                ir_vh_const_getstatics = sm
                    .iter()
                    .filter(|&(&pc, &(_, _, type_tag, _))| {
                        type_tag == b'L' && code.get(pc) == Some(&0xb2)
                    })
                    .filter_map(|(&pc, &(class_id, field_index, _, _))| {
                        ir_lower::vh_const_plan(direct_helpers, class_id, field_index)
                            .map(|plan| (pc, plan.kind, class_id, field_index))
                    })
                    .collect();
            }
            // JIT r23 wave 4 (lane L8): the filed `ValueLayout` facts of this
            // method's own reference `getstatic`s, for the FFM element
            // inline expansion (planner and builder).
            if ir::ir_ffm_element_inline_enabled() {
                for &(class_id, field_index, type_tag, _) in sm.values() {
                    if type_tag != b'L' {
                        continue;
                    }
                    let Ok(fi) = u32::try_from(field_index) else {
                        continue;
                    };
                    if let Some(facts) =
                        ffm_element_inline::ffm_layout_facts(direct_helpers, class_id, field_index)
                    {
                        ir_ffm_layout_facts.insert((class_id, fi), facts);
                    }
                }
                builder.add_ffm_layout_facts(ir_ffm_layout_facts.iter().map(|(&k, &v)| (k, v)));
            }
            // Round nio20 wave 6 (lane L1): the direct-route reads among this
            // method's own `getstatic`s (`Graph::plain_static_reads`).
            builder.add_plain_static_reads(ir_plain_static_read_rows(direct_helpers, sm.values()));
            ir_plan_root_static_final_prims(req, &sm, &mut builder);
            builder.set_static_field_info(sm);
        }
        ir_static_init_classes.sort_unstable();
        ir_static_init_classes.dedup();
    }
    // Thread the resolved instance-field layout (pc → (field_index,
    // type_tag)) into the builder so it can lower an int-category
    // `getfield` into `Op::Load`. A field the resolver can't resolve is
    // simply omitted; the builder then bails that getfield to single-pass.
    if !scan.field_ops.is_empty() {
        if let Some(resolver) = cp_field_resolver {
            let mut fm = std::collections::HashMap::with_capacity(scan.field_ops.len());
            // Round 16 wave 10 (lane fieldsite2): this method's own resolved
            // sites with and without a compact row, for the line below.
            let (mut sites_with_row, mut sites_rowless) = (0usize, 0usize);
            for &(pc, cp_idx) in &scan.field_ops {
                if let Some((field_index, type_tag, compact_slot)) = resolver(cp_idx) {
                    // Marker tag `0`: a `getfield` that must stay on the checked
                    // helper (SBR4 satb, `Reference.referent`). No row, so the
                    // builder bails the method to single-pass.
                    if type_tag == 0 {
                        continue;
                    }
                    if compact_slot.is_some() {
                        sites_with_row += 1;
                    } else {
                        sites_rowless += 1;
                    }
                    // The builder needs (field_index, type_tag); the LOWERER
                    // additionally takes the packed compact slot, so a field
                    // read can be emitted inline instead of paying the
                    // checked helper on every access. Only a genuinely
                    // resolved slot enters the map — a fabricated `(0,
                    // false)` steered the single-pass inline arm at a
                    // garbage offset once already (the WildFly Host
                    // Controller SIGSEGV), and `None` here simply keeps the
                    // helper.
                    fm.insert(pc, (field_index, type_tag));
                    if cratonvm_types::compact_ref_fields_enabled() {
                        if let Some((c_off, c_ref)) = compact_slot {
                            ir_compact_fields.insert((pc, c_ref), (c_off, c_ref, type_tag));
                        }
                    }
                }
            }
            // Round 15 wave 3 (lane chain3, CHN15-2 producer): each resolved
            // site's declaring class, for the alias model's field identity.
            if let Some(decl_of) = req.cp_field_declaring_class_resolver {
                builder.set_field_decl_classes(ir_field_decl_rows(&scan.field_ops, &fm, decl_of));
            }
            builder.set_field_info(fm);
            // The engagement control for a class-layout change: a site whose
            // declaring class has no compact layout (a JDK class padded to a
            // synthetic-stub floor, `ArrayList$Itr` before
            // `CRATONVM_ARRAYLIST_ITR_COMPACT_LAYOUT`) is `rowless`. Since
            // FG16-7 (`CRATONVM_JIT_IR_LEGACY_FIELD_INLINE`, default ON) such a
            // site reads inline through the legacy arm and no longer counts as
            // a `no-compact-slot-for-pc` decline, so that census cannot tell
            // the two layouts apart; this line can.
            if (sites_with_row + sites_rowless) > 0 && false {
                eprintln!(
                    "[ir] field rows {}.{}{}: compact={sites_with_row} rowless={sites_rowless}",
                    cached.class_name, cached.method_name, cached.method_descriptor,
                );
            }
        }
        // Volatile instance fields. `Op::Load` / `Op::Store` carry no memory
        // ordering, so GVN could merge two reads of a volatile flag and LICM
        // could hoist one out of a spin loop that then never sees another
        // thread's write; the lowerer would also emit no StoreLoad fence after
        // a volatile store. The builder refuses any `getfield` / `putfield` at
        // one of these pcs and the method compiles single-pass, which fences
        // every volatile `putfield` and hoists no field read
        // (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
        if let Some(is_volatile) = cp_field_volatile_resolver {
            let volatile_pcs: std::collections::HashSet<usize> = scan
                .field_ops
                .iter()
                .filter(|&&(_, cp_idx)| is_volatile(cp_idx))
                .map(|&(pc, _)| pc)
                .collect();
            builder.set_volatile_field_pcs(volatile_pcs);
        }
        // nio21 wave 2 (lane L3): `final` instance fields, for the memo's
        // final-field forwarding (`ir::ir_field_final_forward_enabled`). Only
        // the `getfield` pcs matter; a `putfield` pc is never consulted.
        if let Some(is_final) = req.cp_field_final_resolver {
            builder.add_final_field_pcs(
                scan.field_ops
                    .iter()
                    .filter(|&&(_, cp_idx)| is_final(cp_idx))
                    .map(|&(pc, _)| pc),
            );
        }
        // JIT round 20 (lane memory): the method's OWN `byte[]`-valued reads
        // (`ByteBuffer.hb`), which `append_ir_inline_site` only ever rebased
        // for spliced bodies; only a `getfield` pc is kept, as there.
        if let Some(is_byte_array) = req.cp_field_byte_array_resolver {
            if ir::ir_scoped_byte_array_base_enabled()
                && ir_lower::ir_scoped_byte_array_base_widen_enabled()
            {
                builder.add_byte_array_field_pcs(
                    scan.field_ops
                        .iter()
                        .filter(|&&(pc, cp_idx)| {
                            code.get(pc) == Some(&0xb4) && is_byte_array(cp_idx)
                        })
                        .map(|&(pc, _)| pc),
                );
            }
        }
    }
    // Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`,
    // default OFF): the field sites (static and instance) the VM admits to an
    // unresolved-field uncommon trap -- an owner not loaded when this method
    // compiles, so a site that has never run. The VM answers only a site its
    // own resolution declines (so a site with a row is never admitted) and
    // records it for the trap's one-shot service; the builder consults the
    // map only where the site has no row (`IrBuilder::plant_field_site_trap`).
    if let Some(admit) = req.cp_field_trap_resolver {
        let mut field_trap_sites: std::collections::HashMap<usize, u8> =
            std::collections::HashMap::new();
        for &(pc, cp_idx) in scan.static_field_ops.iter().chain(scan.field_ops.iter()) {
            if let Some(tag) = admit(cp_idx, pc) {
                field_trap_sites.insert(pc, tag);
            }
        }
        if !field_trap_sites.is_empty() && false {
            let mut pcs: Vec<usize> = field_trap_sites.keys().copied().collect();
            pcs.sort_unstable();
            eprintln!(
                "[cratonvm-jitc] ir field-site traps admitted {}.{}{}: {} site(s) at pcs {:?}",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                pcs.len(),
                pcs
            );
            // Wave 95, lane L2: the builder's `ir field-site trap planted |
            // refused` lines name the method too (`set_invoke_labels` below
            // replaces it with the same text when the method has invokes).
            builder.set_method_label(
                format!(
                    "{}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                )
                .into_boxed_str(),
            );
        }
        builder.set_field_trap_sites(field_trap_sites);
    }
    // Scalar replacement of `new`: only when the elidable-`<init>` resolver
    // is supplied (the production soak flag is on, or a test wires it
    // directly) do we feed the builder the allocation layout for every `new`
    // and the pcs of elidable `<init>()V` invokespecials. Without it, the
    // builder bails on `new`/`invokespecial`, so allocation-bearing methods
    // stay on the single-pass backend exactly as before (inert default).
    // Was any `new` site DEFERRED rather than merely unresolvable? That
    // distinction is what makes an IR bail worth retrying: a deferred class
    // is one nothing has loaded YET, and the very next thing to run is
    // usually its constructor. Read again after `IrBuilder::build`, so the
    // retry is recorded only when the build actually LOST the method.
    // JIT round 19 wave 3, lane eager3 (EAGER2-3): the init words the
    // class-init guards bake, owned by the artifact.
    let mut ir_class_init_cells: Vec<std::sync::Arc<std::sync::atomic::AtomicU8>> = Vec::new();
    let mut any_deferred_new = false;
    // The sites themselves, not just "there was one": the retry grant asks
    // whether these resolve NOW, and it cannot ask that from a bool.
    let mut deferred_new_sites: Vec<(u32, u16)> = Vec::new();
    // Round 9 wave 8b (irl8 request 3): classes whose IR inline-TLAB `new`
    // may call the thin ZGC announce helper instead of `tlab_post_init`
    // (no primitive default, no finalizer). Default ON since the wave-8
    // integration (bintrees -15 %, see `zgc_jit_inline_announce_enabled`);
    // `CRATONVM_JIT_IR_ZGC_ANNOUNCE=0` keeps every site on `tlab_post_init`,
    // byte-for-byte. The lowerer also requires
    // `jit_tlab_registration_required()` and a wired helper.
    let mut ir_tlab_announce_classes: std::collections::HashSet<u32> =
        std::collections::HashSet::new();
    let ir_zgc_announce = true;
    if let (Some(elidable_resolver), Some(new_resolver)) =
        (cp_elidable_init_resolver, cp_new_resolver)
    {
        if !scan.new_ops.is_empty() {
            let mut new_info_map = std::collections::HashMap::with_capacity(scan.new_ops.len());
            let mut new_deferred_map: std::collections::HashMap<usize, (u32, u16)> =
                std::collections::HashMap::new();
            // JIT round 19 wave 2, lane eager2 (EAGER19-1): the deferred
            // sites the VM admits to an uninitialized-class trap.
            let mut uninit_new_trap_pcs = std::collections::HashSet::new();
            // JIT round 20 wave 12 (lane qLoop, W4L3-6): classes resolved with
            // no finalizer, for `ir::Graph::finalizer_free_new_classes`.
            let mut finalizer_free_new_classes: Vec<u32> = Vec::new();
            // JIT round 20 wave 12 (lane hFin): and the classes resolved WITH
            // a finalizer / reference registration, which escape analysis must
            // never scalar-replace (`ir::Graph::finalizable_new_classes`).
            let mut finalizable_new_classes: Vec<u32> = Vec::new();
            // JIT round 19 wave 3, lane eager3 (EAGER2-3): the deferred sites
            // the VM admits to a class-initialized guard, by pc.
            let mut uninit_new_guard_sites = std::collections::HashMap::new();
            for &(pc, cp_idx) in &scan.new_ops {
                // A `Deferred` site has no compile-time class id or field
                // count, so it gets no map entry: the IR builder's 0xbb arm
                // then bails this method to the single-pass backend, which
                // DOES compile the site (through the CP-indexed helper).
                // That is strictly better than the pre-fix behaviour, where
                // the site bailed BOTH backends.
                match new_resolver(cp_idx) {
                    Some(JitNewSite::Resolved {
                        class_id,
                        num_fields,
                        needs_post_alloc_init,
                        has_finalizer,
                    }) => {
                        new_info_map.insert(pc, (class_id, num_fields));
                        if !has_finalizer {
                            finalizer_free_new_classes.push(class_id);
                        } else {
                            finalizable_new_classes.push(class_id);
                        }
                        // Post-init no-op classes. The lowerer decides what
                        // that buys: the thin ZGC announce (only under
                        // `CRATONVM_JIT_IR_ZGC_ANNOUNCE`, which it re-reads),
                        // or no call at all on a collector that walks chunks.
                        if !needs_post_alloc_init && !has_finalizer {
                            ir_tlab_announce_classes.insert(class_id);
                        }
                    }
                    Some(JitNewSite::Deferred {
                        holder_class_id,
                        cp_idx,
                    }) => {
                        any_deferred_new = true;
                        deferred_new_sites.push((holder_class_id, cp_idx));
                        new_deferred_map.insert(pc, (holder_class_id, cp_idx));
                        // EAGER2-3: the row the guard's passing edge allocates
                        // with, the guard's word, and the word's owner.
                        if let Some(site) = req
                            .cp_uninit_new_guard_resolver
                            .and_then(|admit| admit(cp_idx, pc))
                        {
                            new_info_map.insert(pc, (site.class_id, site.num_fields));
                            if !site.has_finalizer {
                                finalizer_free_new_classes.push(site.class_id);
                            } else {
                                finalizable_new_classes.push(site.class_id);
                            }
                            if !site.needs_post_alloc_init && !site.has_finalizer {
                                ir_tlab_announce_classes.insert(site.class_id);
                            }
                            // Cast: the init word's address, baked by the guard.
                            let word = std::sync::Arc::as_ptr(&site.init_state) as usize;
                            uninit_new_guard_sites.insert(pc, (word, site.initialized));
                            ir_class_init_cells.push(site.init_state);
                        }
                        if req
                            .cp_uninit_new_trap_resolver
                            .is_some_and(|admit| admit(cp_idx, pc))
                        {
                            uninit_new_trap_pcs.insert(pc);
                        }
                    }
                    _ => {}
                }
            }
            builder.set_uninit_new_trap_pcs(uninit_new_trap_pcs);
            builder.set_uninit_new_guard_sites(uninit_new_guard_sites);
            let mut trivial_init_pcs = std::collections::HashSet::new();
            for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                if opcode == 0xb7 && elidable_resolver(cp_idx) {
                    trivial_init_pcs.insert(pc);
                }
            }
            builder.set_new_info(new_info_map, trivial_init_pcs);
            builder.note_finalizer_free_new_classes(finalizer_free_new_classes);
            builder.note_finalizable_new_classes(finalizable_new_classes);
            // nio21 wave 15 (lane L3): a deferred site builds as `Op::NewCp`,
            // the CP-indexed helper call the single-pass tier compiles it to,
            // instead of refusing the whole method (`ir::ir_deferred_new_enabled`,
            // default ON). Only with the helper wired: `ir_lower` refuses an
            // `Op::NewCp` whose helper is `0`.
            if !new_deferred_map.is_empty()
                && ir::ir_deferred_new_enabled()
                && helpers.new_object_cp != 0
            {
                builder.set_new_deferred_sites(new_deferred_map);
            }
        }
    }
    // Round nio21 wave 1 (lane L2): the exception table for the compiled local
    // throw edges (`ir::ir_local_throw_edges_enabled`, default OFF). Each catch
    // type is resolved through the SAME `cp_new_resolver` the `new` rows above
    // use, so a thrown `Op::New` and its catch row compare class ids from one
    // constant pool and one loader; a row that does not resolve is `Unknown`
    // and decides no edge. Method-entry compiles only (an OSR build keeps the
    // pre-nio21 walk), and never while a debugger may observe this VM.
    // Wave 2: nor while a JVMTI exception listener may be armed for it
    // (`CompileRequest::exception_events_armed_at_compile`); one armed later is
    // the VM's (`JvmtiEventManager::local_throw_edges_need_the_interpreter`).
    // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the optimizing OSR route gets the
    // table too. Its loop body is the only compiled form of a method run once,
    // and its spliced throws are what the rows are for there (the walk is the
    // method-entry walk plus an entry arm, so the planner's rules hold).
    let osr_compile =
        req.osr_mode_exit_polls || req.osr_optimizing_route || req.osr_requested_bci.is_some();
    // While an exception event may be armed for this VM at run time (in the
    // default `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` mode a listener armed
    // later does not withdraw compiled code), the rows are handed over only
    // for the spliced edges, which test the VM's event flags first; the
    // method's own local throw edges stay off, as before.
    let event_guard = if req.exception_events_armed_at_compile {
        req.exception_event_flag_addrs
            .filter(|_| ir::ir_spliced_throw_edges_enabled())
    } else {
        None
    };
    if ir::ir_local_throw_edges_enabled()
        && !cached.exception_table.is_empty()
        && (!osr_compile || ir::ir_spliced_throw_edges_enabled())
        && !req.debugger_observes_locals
        && (!req.exception_events_armed_at_compile || event_guard.is_some())
    {
        builder.set_exception_event_guard(event_guard);
        if let Some(new_resolver) = cp_new_resolver {
            let rows: Vec<ir::IrLocalThrowRow> = cached
                .exception_table
                .iter()
                .map(|e| ir::IrLocalThrowRow {
                    start_pc: usize::from(e.start_pc),
                    end_pc: usize::from(e.end_pc),
                    handler_pc: usize::from(e.handler_pc),
                    catch: if e.catch_type == 0 {
                        ir::IrCatchClass::Any
                    } else {
                        match new_resolver(e.catch_type) {
                            Some(JitNewSite::Resolved { class_id, .. }) => {
                                ir::IrCatchClass::Class(class_id)
                            }
                            _ => ir::IrCatchClass::Unknown,
                        }
                    },
                })
                .collect();
            // JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): the VM's subclass
            // answer for every pair of a resolved `new` class of this method
            // (the only throws `local_throw_target` decides are this method's
            // own `Op::New`s) and a resolved typed catch row.
            if ir::ir_local_throw_superclass_catch_enabled() {
                if let Some(is_subclass) = req.class_is_subclass_resolver {
                    let catches: Vec<u32> = rows
                        .iter()
                        .filter_map(|r| match r.catch {
                            ir::IrCatchClass::Class(c) => Some(c),
                            _ => None,
                        })
                        .collect();
                    let mut facts: std::collections::HashMap<(u32, u32), bool> =
                        std::collections::HashMap::new();
                    if !catches.is_empty() {
                        for &(_, cp_idx) in &scan.new_ops {
                            let Some(JitNewSite::Resolved { class_id, .. }) = new_resolver(cp_idx)
                            else {
                                continue;
                            };
                            for &c in &catches {
                                if c == class_id || facts.contains_key(&(class_id, c)) {
                                    continue;
                                }
                                if let Some(answer) = is_subclass(class_id, c) {
                                    facts.insert((class_id, c), answer);
                                }
                            }
                        }
                    }
                    builder.set_local_throw_subclass_facts(facts);
                }
            }
            // Each resolved catch class's type-check target, interned under
            // the row's own name and class id (the pair an `instanceof` of
            // that class in this method would carry), for local and spliced
            // runtime throw-edge tests.
            if ir::ir_spliced_throw_edges_enabled() || ir::ir_local_throw_edges_enabled() {
                if let Some(class_name) = cp_class_name_resolver {
                    let targets: Vec<Option<(usize, usize)>> = cached
                        .exception_table
                        .iter()
                        .zip(rows.iter())
                        .map(|(e, r)| match r.catch {
                            ir::IrCatchClass::Class(class_id) if e.catch_type != 0 => {
                                let name = class_name(e.catch_type)?;
                                let (ptr, len) = intern_typecheck_target(&name, Some(class_id));
                                Some((ptr as usize, len))
                            }
                            _ => None,
                        })
                        .collect();
                    builder.set_local_throw_catch_targets(targets);
                }
            }
            builder.set_local_throw_rows(rows);
            // Round nio21 wave 3 (lane L2): the call-site exception edges
            // (`ir::ir_call_site_exception_edges_enabled`, default OFF) need
            // every row's catch-type NAME for `jit_local_handler_lookup`,
            // which resolves it through the declaring class's loader as the
            // interpreter's handler search does. A table with one name that
            // does not resolve arms no call edge at all: a site built from a
            // partial table could answer "propagate" where the real table has
            // a match (the single-pass staging's rule).
            if ir::ir_call_site_exception_edges_enabled() {
                if let Some(class_name) = cp_class_name_resolver {
                    let names: Option<Vec<&'static str>> = cached
                        .exception_table
                        .iter()
                        .map(|e| {
                            if e.catch_type == 0 {
                                Some("")
                            } else {
                                class_name(e.catch_type).map(|n| intern_catch_type_name(&n))
                            }
                        })
                        .collect();
                    if let Some(names) = names {
                        builder
                            .set_call_throw_catch_names(names, cached.declaring_class_id.as_u32());
                    }
                }
            }
        }
    }
    // cov-06: resolved `anewarray` (0xbd) sites for the IR builder — the
    // same `cp_new_resolver` the `new` block above uses (an `anewarray`
    // CP entry is a class reference, exactly like `new`'s). Independent
    // of the elidable-init resolver: `anewarray` has no `<init>` to
    // elide. Only a `Resolved` site enters the map; a `Deferred` site
    // (component class not loaded yet) is omitted, so the builder's
    // 0xbd arm bails that method to single-pass — the same model `new`
    // already uses for its own deferred case.
    if !scan.anewarray_ops.is_empty() {
        if let Some(new_resolver) = cp_new_resolver {
            let mut anewarray_info_map =
                std::collections::HashMap::with_capacity(scan.anewarray_ops.len());
            for &(pc, cp_idx) in &scan.anewarray_ops {
                if let Some(JitNewSite::Resolved { class_id, .. }) = new_resolver(cp_idx) {
                    anewarray_info_map.insert(pc, class_id);
                }
            }
            builder.set_anewarray_info(anewarray_info_map);
        }
    }
    // Round 16 (lane matrix): resolved `multianewarray` (0xc5) sites for the
    // IR builder, behind `CRATONVM_JIT_IR_MULTIANEWARRAY` (with the switch off
    // `ir_compatible` refuses every such method before this point). The site
    // word is the packed `(holder_class_id, cp_idx)` the single-pass door
    // bakes, and its resolver call is kept for the reason it is kept there: a
    // CP index that does not name a class is a refusal, not a site. No wired
    // helper, no map -- the builder then bails the method to single-pass.
    if !scan.multianewarray_ops.is_empty()
        && ir::ir_multianewarray_enabled()
        && helpers.multianewarray_n != 0
    {
        if let Some(resolver) = cp_class_name_resolver {
            let mut sites = std::collections::HashMap::with_capacity(scan.multianewarray_ops.len());
            for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
                if resolver(cp_idx).is_some() {
                    sites.insert(
                        pc,
                        pack_multianewarray_site(cached.declaring_class_id.as_u32(), cp_idx),
                    );
                }
            }
            builder.set_multianewarray_sites(sites);
        }
    }
    // cov-05: resolve `checkcast` (0xc0) / `instanceof` (0xc1) sites for
    // the IR builder — 306 events, the largest single whole-method
    // refusal in the survey, more than every opcode gap combined:
    // `cov-05-checkcast-and-instanceof-RETIRED-20260804.md`.
    //
    // Admits a site ONLY when its target class is already resolved and
    // loaded at compile time. `cp_new_resolver` already answers exactly
    // that question for `new`/`anewarray` — `Resolved` means the
    // CONSTANT_Class entry's target is loaded, `Deferred`/`None` means it
    // is not (or the entry is malformed) — and a checkcast/instanceof CP
    // entry is the identical CONSTANT_Class shape, so this reuses that
    // resolver rather than adding a new one. `num_fields`/the two init
    // flags `Resolved` also carries are irrelevant here and discarded;
    // only "loaded or not" is read.
    //
    // A site that resolves `Deferred` (or has no resolver at all) is
    // simply omitted from `checkcast_info`/`instanceof_info`, so the
    // builder's 0xc0/0xc1 arm bails THAT site — and so the method — to
    // single-pass, which resolves lazily via `jit_typecheck_resolve`'s
    // not-yet-loaded slow path. That path can run a user classloader's
    // `loadClass`/`findClass`, arbitrary Java this tier does not host
    // inside a helper call — see the admission comment in `ir.rs`.
    //
    // `scan.typecheck_ops` is `checkcast_ops ∪ instanceof_ops`; a pc is
    // routed to `checkcast_info` iff it is also in `scan.checkcast_ops`,
    // else to `instanceof_info` — the two are no longer mutually
    // exclusive at the whole-method level as of cov-05 (a method may
    // contain both), unlike the pre-cov-05 shape where `checkcast_ops`
    // non-empty refused the whole method upstream.
    if !scan.typecheck_ops.is_empty() {
        if let (Some(new_resolver), Some(name_resolver)) = (cp_new_resolver, cp_class_name_resolver)
        {
            let checkcast_pcs: std::collections::HashSet<usize> =
                scan.checkcast_ops.iter().map(|&(pc, _)| pc).collect();
            let mut cc_im = std::collections::HashMap::new();
            let mut io_im = std::collections::HashMap::new();
            for &(pc, cp_idx) in &scan.typecheck_ops {
                if let Some(JitNewSite::Resolved {
                    class_id: target_id,
                    ..
                }) = new_resolver(cp_idx)
                {
                    if let Some(name) = name_resolver(cp_idx) {
                        // Interned process-wide, NOT owned by this
                        // compilation — see `intern_typecheck_target`.
                        //
                        // `class_id` is the copy this method's OWN constant
                        // pool resolves to, through its own loader. It was
                        // previously discarded, leaving the runtime helper
                        // to re-resolve a bare name against a dictionary
                        // keyed by `(ClassLoaderId, name)`.
                        //
                        // Round 9 wave 9 (irl9 cross-lane request 1, applied
                        // by the `lib.rs` owner, `evict9`): the same finality
                        // verdict the single-pass typecheck table interns
                        // with, so the IR `instanceof` answers a final USER
                        // class's miss inline and both tiers share a pointer.
                        let target_is_final = req
                            .class_is_final_resolver
                            .is_some_and(|is_final| is_final(target_id));
                        if !target_is_final
                            && !name.starts_with('[')
                            && req
                                .class_is_cha_leaf_resolver
                                .is_some_and(|is_leaf| is_leaf(target_id))
                        {
                            builder.graph.cha_leaf_classes.insert(target_id);
                            let dep = format!("{}{}", crate::frozen_facts::CHA_LEAF_DEP_PREFIX, name);
                            if !builder.graph.frozen_cha_deps.contains(&dep) {
                                builder.graph.frozen_cha_deps.push(dep);
                            }
                        }
                        let (ptr, len) = intern_typecheck_target_with_finality(
                            &name,
                            Some(target_id),
                            target_is_final,
                        );
                        let entry = (ptr as usize, len);
                        if checkcast_pcs.contains(&pc) {
                            cc_im.insert(pc, entry);
                        } else {
                            io_im.insert(pc, entry);
                        }
                    }
                }
            }
            if !cc_im.is_empty() {
                ir_needs_dispatch_for_checkcast = true;
            }
            builder.set_checkcast_info(cc_im);
            builder.set_instanceof_info(io_im);
        }
    }
    // Gap B / inc 22: invokestatic → `Op::Call`. Only when the IR-call gate
    // is on AND the method has no `new`/array allocation (a surviving `New`
    // would need the allocation path the lowerer lacks; array ops bail the
    // builder anyway) AND every invoke is an `invokestatic` whose descriptor
    // is GPR-marshallable (int/reference args + int/void/reference return —
    // no long/float/double). Reference params, reference args, int field
    // ops, and reference returns ARE allowed: any oop live across the call
    // sits in a spilled frame slot, which the conservative GC root scan of
    // the IR frame finds — sound because the GC is non-moving while a JIT
    // frame is active (so a pinned pointer is never relocated). The leaked
    // `JitInvokeInfo` boxes/strings are attached to the returned
    // `CompiledMethod` below so the baked `info_ptr`s outlive the code.
    let mut ir_call_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
    // JIT round 23 wave 3d (lane L1): `(IR kind-4 row, VM kind-1 twin)` of every
    // receiver-carrying direct self-call; both boxes live in `ir_call_infos`.
    // Handed to the lowerer as `Graph::self_call_service_infos`.
    let mut ir_self_call_service_infos: Vec<(usize, usize)> = Vec::new();
    // Their caller pcs, which the inline planner leaves as calls.
    let mut ir_instance_self_call_pcs: Vec<usize> = Vec::new();
    let mut ir_vh_judged_sites: Vec<Box<VarHandleJudgedSite>> = Vec::new();
    let mut ir_call_strings: Vec<Box<str>> = Vec::new();
    // IR direct-call lowering: `pc → (callee_entry, callee_needs_context)`
    // for each statically-bound site the IR lowerer may bind directly, plus
    // the flat entry list recorded on the finished `CompiledMethod` (see
    // `_direct_callee_entries` — keep-alive + invalidation closure).
    let mut ir_direct_calls: std::collections::HashMap<usize, (usize, bool)> =
        std::collections::HashMap::new();
    let mut ir_direct_callee_entries: Vec<(usize, u64)> = Vec::new();
    // Round 11 wave 10 (lane `synccall`): caller-held synchronized direct
    // CALLs, `pc -> (holder class id, class cp index, mirror slot)` for the
    // builder's monitor pair. Each also has an `ir_direct_calls` row under
    // `compile_request::sync_direct_row_key(pc)` for the lowerer, and never
    // one under the plain pc. See `CompileRequest::sync_direct_lookup`.
    let mut ir_sync_direct_sites: std::collections::HashMap<usize, (u32, u16, usize)> =
        std::collections::HashMap::new();
    // Round 9 wave 8b (irl8 request 2): IR closing edges planned as
    // `JitCycleEdgeCell` sites (pc -> cell address), and the cells themselves,
    // which the artifact must own before publication (`JitCache::put`
    // registers `_jit_cycle_cells`). Same gates as the single-pass plan in
    // `build_single_pass_tables`; empty unless `CRATONVM_JIT_CYCLE_EDGE_CELL`.
    let mut ir_cycle_cells: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    let mut ir_owned_cycle_cells: Vec<Arc<JitCycleEdgeCell>> = Vec::new();
    // IR inline caches (jit-inlining-and-ir-calls): `pc → (mic_addr,
    // pic_addr)` for each virtual/interface site the IR lowerer may serve
    // from a MIC + 4-way-PIC cascade, plus the owning boxes, which are moved
    // onto the finished `CompiledMethod` so the baked imm64s stay valid.
    let mut ir_ic_slots: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    let mut ir_mic_boxes: Vec<Box<JitMICSlot>> = Vec::new();
    // JIT round 20 (MHIFACE19-1): the interface handle guards' seen-class
    // cells, moved onto the artifact with the inline caches.
    let mut ir_typecheck_seen_cells: Vec<Box<TypecheckSeenCell>> = Vec::new();
    let mut ir_pic_boxes: Vec<Box<JitPICSlot>> = Vec::new();
    // The virtual/interface gate was default-OFF at the VM call sites for
    // one reason only: the IR lowered `invokevirtual`/`invokeinterface`
    // through the generic `jit_invoke_dispatch` helper with NO inline cache,
    // so admitting them made call-heavy methods slower than the single-pass
    // body they replaced. `ir_lower::emit_inline_cache_call` now emits the
    // same MIC + 4-way-PIC cascade the single-pass backend does, so that
    // reason is gone and the capability becomes opt-OUT
    // (`CRATONVM_JIT_IR_CALL_VIRTUAL=0`) rather than opt-in — matching every
    // other IR capability gate. The caller's parameter can still force it
    // ON; it can no longer force it off. See the design doc for the matching
    // `vm/src/runtime/env_cache.rs` cleanup.
    let ir_emit_virtual_calls = ir_virtual_calls_enabled(ir_emit_virtual_calls);
    // cov-04 census. The invoke bails in `IrBuilder::build` report a line
    // number and a bytecode pc; neither says *which callee*, and the whole
    // first increment of
    // `cov-04-the-invoke-arms-RETIRED-20260803.md`
    // was "group them by callee before writing code". Under
    // `CRATONVM_DBG=ir-compiles` (or `jitc`) hand the builder a
    // diagnostic-only `pc → "0xNN cn.mn desc"` map so each bail names its
    // callee, and print the method-level facts that decide whether
    // `invoke_info` is populated at all. Costs nothing when the flag is off:
    // the resolver is not called and no string is built.
    if ir_stage_reporting() && !scan.invoke_ops.is_empty() {
        if let Some(resolver) = cp_invoke_resolver {
            let mut labels: std::collections::HashMap<usize, Box<str>> =
                std::collections::HashMap::with_capacity(scan.invoke_ops.len());
            for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                if let Some((cn, mn, desc)) = resolver(cp_idx) {
                    labels.insert(
                        pc,
                        format!("{opcode:#04x} {cn}.{mn}{desc}").into_boxed_str(),
                    );
                }
            }
            builder.set_invoke_labels(
                labels,
                format!(
                    "{}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                )
                .into_boxed_str(),
            );
        }
        eprintln!(
            "[ir] invoke-plan {}.{}{}: sites={} new_ops={} anewarray_ops={} \
                 gates(static={ir_emit_calls} special={ir_emit_special_calls} \
                 virtual={ir_emit_virtual_calls}) call_eligible={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            scan.invoke_ops.len(),
            scan.new_ops.len(),
            // `new_ops` is reported but is NOT part of `call_eligible` any
            // more (cov-04 increment 2). It stays in the line because it is
            // what the pre-fix measurement keyed on, so the two runs remain
            // comparable — but this field must keep matching the predicate
            // below, or the census reports a gate that is not the gate.
            scan.anewarray_ops.len(),
            scan.anewarray_ops.is_empty(),
        );
    }
    // JIT round 16 wave 10 (lane indyir, IN16-8; `CRATONVM_JIT_INDY_CONSTANT_INVOKE`,
    // default OFF in the VM): an `invokedynamic` whose call site is constant
    // and whose target is a direct handle of exactly the site's type is, for
    // the rest of this method's life, an `invokestatic` / `invokevirtual` of
    // that target. Such a site joins the invoke plan below as that invoke --
    // its names answered by `ir_indy_direct` under its own constant-pool
    // index, which no `Methodref` shares -- so it gets what an ordinary call
    // gets: a direct bind, an inline cache, a splice of the target's body.
    // The builder's `0xba` arm hands it to the invoke arm
    // (`IrBuilder::indy_direct_opcode`). Every other indy keeps its bridge or
    // trap; with no planned site everything below is exactly as before.
    let ir_indy_guard_trapped = |pc: usize| {
        req.despec.is_some_and(|registry| {
            let key = format!(
                "{}.{}:{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
            let bci = pc as u32;
            registry.contains_speculation(
                &key,
                bci,
                crate::deopt::speculation_id(bci, crate::deopt::DeoptReason::UncommonTrap),
            )
        })
    };
    let mut ir_indy_direct = ir_indy_direct_plan(
        req.cp_indy_direct_resolver,
        &scan.indy_ops,
        ir_emit_calls,
        ir_emit_virtual_calls,
        (
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
        &ir_indy_guard_trapped,
    );
    // JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
    // `CRATONVM_JIT_INDY_MUTABLE_GUARD`, default ON in the VM): a MUTABLE call
    // site whose bound target the VM learnt joins the plan the same way, as a
    // guarded site the `0xba` arm builds (`guard ? call : bridge`). With the
    // switch off the resolver answers nothing and the plan is as before.
    ir_indy_mutable_plan_into(
        &mut ir_indy_direct,
        req.cp_indy_mutable_guard_resolver,
        &scan.indy_ops,
        ir_emit_calls,
        ir_emit_virtual_calls,
        (
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
        direct_helpers.indy_bridge != 0,
    );
    // JIT round 21 wave 17 (lane records, IN16-6;
    // `CRATONVM_JIT_IR_INDY_RECORD_FOLD`, default ON in the VM): a record's
    // generated `hashCode` / `equals` site joins the plan as the fold's
    // `Objects` call, and the `0xba` arm builds the fold. With the switch off
    // the resolver answers nothing and the plan is as before.
    ir_indy_record_fold_plan_into(
        &mut ir_indy_direct,
        req.cp_indy_record_fold_resolver,
        &scan.indy_ops,
        ir_emit_calls,
    );
    // JIT round 16 wave 17 (lane mh7; MH16-8 at compile time,
    // `CRATONVM_JIT_MH_CONSTANT_COMPILE`, default ON in the VM): an
    // `invokevirtual MethodHandle.invokeExact` / `invoke` whose receiver is a
    // `static final` field holding an exact direct static handle is, for the
    // rest of this method's life, an `invokestatic` of the handle's member.
    // Such a site's row is replaced by that invoke -- its names answered by
    // `ir_mh_constant` under its field's constant-pool index -- and the
    // builder drops the receiver (`IrBuilder::mh_constant_opcode`). Every
    // other site is exactly as before.
    let ir_mh_handler_pcs: Vec<usize> = cached
        .exception_table
        .iter()
        .map(|e| usize::from(e.handler_pc))
        .collect();
    // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): a bound handle's site
    // keeps its plan only with its `(argL1 slot, receiver class)` row.
    let ir_mh_constant = ir_mh_constant_plan(
        req.cp_mh_constant_resolver,
        cp_invoke_resolver,
        code,
        code_len,
        &ir_mh_handler_pcs,
        &scan.invoke_ops,
        // JIT round 17 wave 3 (lane mh4; MHCONST17-2): a virtual handle's
        // site joins as an `invokevirtual`, under the virtual-call gate; its
        // receiver guard's earlier trap keeps the door (the indy plan's rule).
        (ir_emit_calls, ir_emit_virtual_calls),
        (
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
        &ir_indy_guard_trapped,
    )
    .with_bound_rows(req.cp_mh_constant_bound_resolver)
    // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a filter species'
    // site keeps its plan only with its PRE call's row.
    .with_filter_rows(
        req.cp_mh_constant_filter_resolver,
        code,
        &|bci: usize| {
            cached
                .exception_table
                .iter()
                .any(|e| usize::from(e.start_pc) <= bci && bci < usize::from(e.end_pc))
        },
        (ir_emit_calls, ir_emit_virtual_calls),
        (
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
        // mh21 (MHFILTER21-1): a synthetic PRE key is free when no invoke of
        // the method is keyed by it and neither the VM's invoke resolver nor
        // the indy plan answers it (they answer only real pool indexes).
        &|key: u16| {
            !scan.invoke_ops.iter().any(|&(_, cp, _)| cp == key)
                && cp_invoke_resolver.map_or(true, |resolve| resolve(key).is_none())
                && ir_indy_direct.names(key).is_none()
        },
    );
    let ir_mh_constant = ir_mh_constant.with_convert_rows(
        req.cp_mh_constant_convert_resolver,
        code,
        &|bci: usize| {
            cached
                .exception_table
                .iter()
                .any(|e| usize::from(e.start_pc) <= bci && bci < usize::from(e.end_pc))
        },
        (ir_emit_calls, ir_emit_virtual_calls),
        &|key: u16| {
            !scan.invoke_ops.iter().any(|&(_, cp, _)| cp == key)
                && cp_invoke_resolver.map_or(true, |resolve| resolve(key).is_none())
                && ir_indy_direct.names(key).is_none()
        },
    );
    // JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3;
    // `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL`, default ON): an `invokeinterface`
    // SAM call on the value of this method's own lambda `invokedynamic`
    // (straight-line code proves it) is, for the rest of this method's life,
    // an `invokestatic` of the lambda's static impl with the captures in
    // front of the SAM arguments -- what the proxy's method does. Such a
    // site's row is replaced by that invoke, its names answered by
    // `ir_lambda_impl` under the lambda's constant-pool index, so it gets
    // what an ordinary static call gets (a direct bind, a splice of the
    // impl), and the builder swaps the proxy for its captures
    // (`IrBuilder::lambda_impl_opcode`). Every other site is as before.
    let spliced_captures = ir_lambda_impl_spliced_captures_enabled();
    let mut ir_indy_proxy_classes: HashMap<usize, u32> = HashMap::new();
    let mut ir_indy_shapes: HashMap<usize, (usize, u8)> = HashMap::new();
    let ir_lambda_impl = match (
        cp_invokedynamic_descriptor_resolver,
        req.indy_lambda_impl_resolver,
    ) {
        (Some(resolver), Some(impls))
            if !scan.indy_ops.is_empty()
                && ir_lambda_impl_call_enabled()
                && !lambda_plan_withheld =>
        {
            let capture_new = ir::ir_site_trap_enabled() && ir::ir_indy_capture_new_enabled();
            // j27 wave 7 (lane T1LAM3, LAMBDA321-3): every dropped site and
            // why, for the `[ir] lambda-impl-skip` census below.
            use crate::lambda_impl_site_skip::{retain_noting, LambdaSiteSkip};
            let mut skipped: Vec<(usize, LambdaSiteSkip)> = Vec::new();
            let (mut lambdas, shapes) = ir_lambda_impl_lambdas(
                req,
                resolver,
                impls,
                capture_new,
                &scan.indy_ops,
                &mut skipped,
            );
            ir_indy_proxy_classes = lambdas
                .iter()
                .filter_map(|(&pc, (_, _, ans))| (ans.class_id != 0).then_some((pc, ans.class_id)))
                .collect();
            ir_indy_shapes = shapes.clone();
            retain_noting(
                &mut lambdas,
                &mut skipped,
                LambdaSiteSkip::IndyDirect,
                |row| ir_indy_direct.names(row.0).is_none(),
            );
            // JIT round 21 (lane lambda3): a capturing lambda only where the
            // optimizing tier can splice its impl.
            if spliced_captures {
                retain_noting(
                    &mut lambdas,
                    &mut skipped,
                    LambdaSiteSkip::NoImplSplice,
                    |row| {
                        let (_, captures, answer) = row;
                        *captures == 0
                            || ir_inline_resolver.is_some_and(|splice| {
                                splice(&answer.class, &answer.name, &answer.descriptor).is_some()
                            })
                    },
                );
            }
            // JIT round 21 (lane lambda3, second host round): capturing
            // sites only under `CRATONVM_JIT_IR_LAMBDA_IMPL_CAPTURING` (default ON).
            if !ir_lambda_impl_capturing_enabled() {
                retain_noting(
                    &mut lambdas,
                    &mut skipped,
                    LambdaSiteSkip::CapturingOff,
                    |row| row.1 == 0,
                );
            }
            let plan = ir_lambda_impl_plan(
                cp_invoke_resolver,
                &lambdas,
                &shapes,
                code,
                code_len,
                &ir_mh_handler_pcs,
                &scan.invoke_ops,
                ir_emit_calls,
                (
                    &*cached.class_name,
                    &*cached.method_name,
                    &*cached.method_descriptor,
                ),
                &|pc: usize| ir_first_call.contains_key(&pc),
                // JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4): a
                // receiver local written once outside every loop. A capturing
                // one not in an OSR compile, whose entry arm hands the loop
                // header the interpreter's proxy (the builder would refuse).
                ir_lambda_impl_loop_slot_enabled().then_some((
                    prologue_param_slots,
                    req.osr_requested_bci.is_none()
                        && !req.osr_optimizing_route
                        && !req.osr_mode_exit_polls,
                )),
            )
            // JIT round 21 (lane lambda3): a capturing site whose splice an
            // earlier attempt dropped keeps the interface call.
            .without_capture_pcs(if spliced_captures {
                ir_splice_skip
            } else {
                &ir_splice_skip[..0]
            });
            if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                let planned = plan
                    .builder_sites()
                    .into_values()
                    .map(|(indy_pc, _, _)| indy_pc);
                skipped.extend(crate::lambda_impl_site_skip::unplanned(
                    lambdas.keys().copied(),
                    planned,
                ));
                for line in crate::lambda_impl_site_skip::skip_lines(&skipped) {
                    eprintln!(
                        "[ir] lambda-impl-skip {}.{}{} {line}",
                        cached.class_name, cached.method_name, cached.method_descriptor,
                    );
                }
            }
            plan
        }
        _ => IrLambdaImplPlan::default(),
    };
    *lambda_plan_built = !ir_lambda_impl.is_empty();
    let ir_invoke_ops = ir_lambda_impl.rewritten_invoke_ops(
        &ir_mh_constant.rewritten_invoke_ops(&ir_indy_direct.merged_invoke_ops(&scan.invoke_ops)),
    );
    let ir_indy_invoke_names = |cp_idx: u16| -> Option<(String, String, String)> {
        ir_indy_direct
            .names(cp_idx)
            .or_else(|| ir_mh_constant.names(cp_idx))
            .or_else(|| ir_lambda_impl.names(cp_idx))
            .or_else(|| cp_invoke_resolver.and_then(|r| r(cp_idx)))
    };
    let ir_indy_invoke_names: &dyn Fn(u16) -> Option<(String, String, String)> =
        &ir_indy_invoke_names;
    let ir_indy_invoke_class_id = |cp_idx: u16| -> Option<u32> {
        ir_indy_direct
            .class_id(cp_idx)
            .or_else(|| ir_mh_constant.class_id(cp_idx))
            .or_else(|| ir_lambda_impl.class_id(cp_idx))
            .or_else(|| cp_invoke_class_id_resolver.and_then(|r| r(cp_idx)))
    };
    let ir_indy_invoke_class_id: &dyn Fn(u16) -> Option<u32> = &ir_indy_invoke_class_id;
    let (cp_invoke_resolver, cp_invoke_class_id_resolver): (
        Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
        Option<&dyn Fn(u16) -> Option<u32>>,
    ) = if ir_indy_direct.is_empty() && ir_mh_constant.is_empty() && ir_lambda_impl.is_empty() {
        (cp_invoke_resolver, cp_invoke_class_id_resolver)
    } else {
        // JIT round 21 (lane lambda2).
        if !ir_lambda_impl.is_empty() {
            if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] lambda-impl-call {}.{}{}: {} SAM call(s) planned as the impl's \
                     invokestatic",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    ir_lambda_impl.site_count(),
                );
            }
            builder.set_lambda_impl_sites(ir_lambda_impl.builder_sites());
            builder.set_lambda_impl_captures_need_splice(spliced_captures);
        }
        if !ir_indy_direct.is_empty() {
            if ir_stage_reporting() {
                eprintln!(
                    "[ir] indy-constant {}.{}{}: {} site(s) planned as plain calls",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    ir_indy_direct.site_count(),
                );
            }
            builder.set_indy_direct_sites(ir_indy_direct.pseudo_opcodes());
            // JIT round 20 wave 11 (lane pIndy, SPLICE19-1).
            let guards = ir_indy_direct.mutable_guards();
            if !guards.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-mutable {}.{}{}: {} site(s) planned as guarded calls",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        guards.len(),
                    );
                }
                builder.set_indy_mutable_guards(guards);
            }
            // JIT round 21 wave 17 (lane records, IN16-6).
            let mut folds = ir_indy_direct.record_folds();
            // j27 wave 5 (lane RECORDS, RECORDS21-3): a `String` component's
            // cached hash, from the same `String.hash` slot the
            // `Object.hashCode()` String prefix reads.
            ir::record_fold_admit_string_hash(&mut folds, direct_helpers.string_hash_field());
            if !folds.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-record-fold {}.{}{}: {} site(s) planned as a field-wise fold",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        folds.len(),
                    );
                }
                builder.set_indy_record_folds(folds);
            }
        }
        if !ir_mh_constant.is_empty() {
            if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] mh-constant {}.{}{}: {} site(s) planned as plain calls",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    ir_mh_constant.site_count(),
                );
            }
            builder.set_mh_constant_sites(ir_mh_constant.receiver_pcs());
            // JIT round 19 wave 1 (lane mhiface; MHRESID18-4).
            builder.set_mh_constant_interface_sites(ir_mh_constant.interface_pcs());
            // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1).
            builder.set_mh_constant_bound_sites(ir_mh_constant.bound_slots());
            // j27 wave 6 (lane T1LAM2).
            builder.set_mh_constant_bound_static_sites(ir_mh_constant.bound_static_pcs());
            // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1).
            builder.set_mh_constant_filter_sites(ir_mh_constant.filter_sites());
            builder.set_mh_constant_convert_sites(ir_mh_constant.convert_sites());
            // JIT round 20 (MHIFACE19-1): one two-way seen-class cell per
            // interface site, both ways starting at the interface's own id (no object has it, and
            // the lowering's exact-id compare already answers it), owned by
            // the artifact (`_jit_typecheck_seen_cells`).
            if mh_interface_guard_seen_class_enabled() {
                let mut cells = HashMap::new();
                for (pc, class_id) in ir_mh_constant.interface_sites() {
                    let cell = Box::new([
                        std::sync::atomic::AtomicU32::new(class_id),
                        std::sync::atomic::AtomicU32::new(class_id),
                        std::sync::atomic::AtomicU32::new(0),
                        std::sync::atomic::AtomicU32::new(0),
                    ]);
                    cells.insert(pc, cell.as_ptr() as usize);
                    ir_typecheck_seen_cells.push(cell);
                }
                builder.set_mh_constant_interface_seen_cells(cells);
            }
        }
        (Some(ir_indy_invoke_names), Some(ir_indy_invoke_class_id))
    };
    if (ir_emit_calls || ir_emit_special_calls || ir_emit_virtual_calls)
        && !ir_invoke_ops.is_empty()
    {
        if let Some(resolver) = cp_invoke_resolver {
            // cov-04. This used to also require `scan.new_ops.is_empty()`,
            // on the premise that "a surviving `New` would need the
            // allocation path the lowerer lacks". `ir_lower` grew that path
            // — its `Op::New` arm goes through the shared
            // compact-layout/TLAB-aware `jit_new_object` stub, the same
            // helper the single-pass backend's `0xbb` uses, and it already
            // refuses (`helpers.new_object == 0`) rather than emitting a
            // call through address zero. So the term outlived its reason,
            // and it was the single largest cause of an invoke refusal
            // measured: 79 compiles lost `invoke_info` to it, producing 39
            // of the 68 invoke bails — including all 13 at the `0xb6`/`0xb8`
            // arm and both at `0xb9`, whose callees are ordinary
            // perfectly-resolvable methods that were never the problem.
            //
            // `anewarray` stays: `IrBuilder::build` has no `0xbd` arm at
            // all, and `ir_compatible` refuses such methods one stage
            // earlier anyway (that conjunct is `cov-06`'s, not this lane's).
            //
            // An allocation that reaches an `Op::Call` as an argument is
            // arg-escaped by the escape analysis, so it is really allocated
            // rather than scalar-replaced — see `build_connection_graph`'s
            // `Op::Call` arm.
            // `ir_compatible` admits up to `IR_MAX_ARRAY_ALLOCATIONS` (16)
            // `anewarray` ops, "each one lowered through the shared
            // `emit_new_array_stub`" — and then this line refused
            // `invoke_info` for a method containing even ONE. Since a
            // missing map bails the builder at whichever invoke comes
            // first, the two gates disagreed and the admission gate lost:
            // the method was admitted, walked, and then refused.
            //
            // The `new_ops` half of this same condition was already deleted
            // as "a term that outlived its reason"; no rationale was ever
            // recorded for the `anewarray` half, and the IR tier lowers
            // `anewarray` through its own helper
            // (`live_anewarray_calls_the_reference_array_helper`).
            //
            // Measured on `org.h2.test.db.TestAlter`: 13 of the 35 methods
            // that lose `invoke_info` lose it here — 37% of the single
            // largest refusal in the tier.
            //
            // `CRATONVM_JIT_IR_CALL_ANEWARRAY=0` restores the coupling.
            let call_eligible = scan.anewarray_ops.is_empty() || ir_calls_with_anewarray_enabled();
            if call_eligible {
                let mut info_map = std::collections::HashMap::new();
                // See `builder.set_object_init_pcs` below.
                let mut object_init_pcs: std::collections::HashSet<usize> =
                    std::collections::HashSet::new();
                let mut all_emittable = true;
                // cov-04 census: which site, and which of the five
                // conditions below, turned `all_emittable` off. Only built
                // under the debug flag; `all_emittable` is the real state.
                // The distinction matters because a single non-emittable
                // site discards `invoke_info` for the WHOLE method, so the
                // builder then bails on whichever invoke comes FIRST — which
                // is rarely the site that caused it.
                let mut nonemittable: Option<String> = None;
                // Unsupported intrinsic sites outside every CFG cycle may
                // stay ordinary calls in an OSR-only artifact. Entry bodies
                // retain the profitability refusal: their callers may loop.
                let mut repeated_intrinsic_pcs: Option<Option<std::collections::HashSet<usize>>> =
                    None;
                // Call sites this tier will lower as arithmetic. Kept OUT
                // of `info_map`: such a site needs no `JitInvokeInfo` box,
                // and putting one there would have the builder emit a call.
                let mut ir_scalar_intrinsic_sites: std::collections::HashMap<usize, ir::ScalarOp> =
                    std::collections::HashMap::new();
                // j27 wave 2 (lane INTRINS): sites expanded into plain IR
                // arithmetic (`ir::PureExpansion`), on the same terms.
                let mut ir_pure_expansion_sites: std::collections::HashMap<
                    usize,
                    ir::PureExpansion,
                > = std::collections::HashMap::new();
                // Unboxing accessors this tier will lower as a guarded
                // field load. Kept out of `info_map` for the same reason,
                // and carrying the receiver class id the guard needs.
                let mut ir_unbox_intrinsic_sites: std::collections::HashMap<
                    usize,
                    (ir::UnboxOp, u32),
                > = std::collections::HashMap::new();
                // Follow-up to the fib44 fix: when
                // `CRATONVM_JIT_IR_SELFREC_DIRECT` is on, an eligible
                // self-recursive static call is emitted as a DIRECT self-call
                // (invoke_kind 4) instead of paying `jit_invoke_dispatch` on
                // every recursion — see the gate below and `ir_lower`'s Op::Call.
                // Default-on when the native-stack guard helper is wired;
                // the env flag remains an opt-out for diagnosis.
                let selfrec_direct = selfrec_direct_enabled();
                // JIT round 19 wave 3, lane throws3 (THROWS3-1): read once per
                // compile; see `selfrec_direct_void_enabled`.
                let selfrec_direct_void = selfrec_direct && selfrec_direct_void_enabled();
                // IR direct-call lowering (this slice) - see
                // `ir_direct_calls_enabled` and `ir_lower`'s
                // `emit_direct_cross_call`.
                let ir_direct = ir_direct_calls_enabled();
                // nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`):
                // the loop ranking the inline-cache profile seed asks
                // (`ir_ic_profile_seed_planned`), built at most once, and only
                // when a starved virtual site asks.
                let ir_ic_seed_order: std::cell::OnceCell<ir_splice_order::ProfilePlanOrder> =
                    std::cell::OnceCell::new();
                let ir_ic_seed_method_entry = !req.osr_mode_exit_polls
                    && !req.osr_optimizing_route
                    && req.osr_requested_bci.is_none();
                for &(pc, cp_idx, opcode) in ir_invoke_ops.iter() {
                    // Admit `invokestatic` (under `ir_emit_calls`), resolved
                    // non-`<init>` `invokespecial` (inc 24, under
                    // `ir_emit_special_calls`), and `invokevirtual` /
                    // `invokeinterface` (inc 25, under `ir_emit_virtual_calls`).
                    // Any invoke kind whose gate is disabled keeps the whole
                    // method on single-pass — the builder bails on an invoke
                    // with no `invoke_info` entry.
                    let is_static = opcode == 0xb8;
                    // JVMS 5.4.6 — an `invokevirtual` resolving to a PRIVATE
                    // method is not a dispatch site; see the matching note
                    // in the single-pass invoke loop below. The optimizing
                    // tier has to make the same reclassification, and make
                    // it HERE, before `is_virtual`/`is_special` feed the
                    // direct-bind decision and the baked `invoke_kind`.
                    //
                    // The same holds for an `invokeinterface` (0xb9) whose
                    // resolved method is a PRIVATE interface method (JVMS
                    // §6.5 selection step 1): the VM's owner resolver answers
                    // `Some(declaring interface)` for exactly that case, and
                    // the site becomes a statically bound (`is_special`) one
                    // with no inline cache.
                    let private_virtual_owner: Option<String> = if matches!(opcode, 0xb6 | 0xb9) {
                        cp_invokespecial_owner_resolver
                            .and_then(|r| r(cp_idx, opcode))
                            .map(|(owner, _)| owner)
                    } else {
                        None
                    };
                    let is_special = opcode == 0xb7 || private_virtual_owner.is_some();
                    let is_virtual = opcode == 0xb6 && private_virtual_owner.is_none();
                    let is_interface = opcode == 0xb9 && private_virtual_owner.is_none();
                    if opcode == 0xb6 && private_virtual_owner.is_some() {
                        PRIVATE_INVOKEVIRTUAL_PINNED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if !((is_static && ir_emit_calls)
                        || (is_special && ir_emit_special_calls)
                        || ((is_virtual || is_interface) && ir_emit_virtual_calls))
                    {
                        all_emittable = false;
                        if ir_stage_reporting() {
                            nonemittable =
                                Some(format!("pc={pc}: gate off for opcode {opcode:#04x}"));
                        }
                        break;
                    }
                    let (cn, mn, desc) = match resolver(cp_idx) {
                        Some(t) => t,
                        None => {
                            all_emittable = false;
                            if ir_stage_reporting() {
                                nonemittable = Some(format!(
                                    "pc={pc}: cp_invoke_resolver declined cp_idx={cp_idx}"
                                ));
                            }
                            break;
                        }
                    };
                    // cov-04. This used to read
                    //
                    //     if is_special && mn == "<init>" { all_emittable = false; break; }
                    //
                    // on the premise that "a constructor is only ever
                    // handled by the scalar-new elision path, and a
                    // `<init>`-bearing method also has a `new`, so
                    // `call_eligible` is already false". The second half of
                    // that is simply not true, and the measurement says so:
                    // 35 of the compiles this term disabled had **no `new`
                    // at all** — they were compiled CONSTRUCTORS, whose
                    // `super(...)` / `this(...)` chain call is an
                    // `invokespecial` to `<init>` on `this`. Eliding one is
                    // never an option (the receiver is a parameter, not a
                    // fresh `Op::New`), so this term was not choosing
                    // between two transforms; it was refusing the only one.
                    //
                    // A `<init>` now takes the ordinary statically-bound
                    // `invoke_kind == 1` route, which is exactly what the
                    // single-pass backend already emits for every
                    // non-elidable constructor call (see the
                    // `cp_elidable_init_resolver` rewrite below, whose
                    // `else` arm is this same dispatch). The builder still
                    // prefers ELISION whenever the pc is elidable and the
                    // receiver is a fresh `Op::New`, so scalar replacement
                    // is unaffected — see `IrBuilder::build`'s `0xb7` arm.
                    //
                    // A `<init>` used to be barred from the direct-call
                    // path below as well, on the grounds that
                    // `direct_target` bakes an entry resolved by running
                    // `callee_compiler`, so admitting constructors was "a
                    // compile-time and recursion-cycle change this lane did
                    // not measure". It is measured now, and the cost of NOT
                    // admitting them is the larger number.
                    //
                    // A non-empty constructor is the one statically bound
                    // call every allocation site pays, and the OPTIMIZING
                    // tier was the only backend refusing to bind it — the
                    // single-pass ladder has always direct-called
                    // `matches!(invoke_kind, 1 | 3)` without excluding
                    // `<init>`. On the Azure host, `for (…) sink = new X()`
                    // in an OSR-compiled loop (JDK 25, real-jdk mode):
                    //
                    //   ctor body `i = ATOMIC.getAndIncrement()`   917 ns/op
                    //   ctor body `i = ++staticInt`                312 ns/op
                    //   ctor body `i = param`                      749 ns/op
                    //   empty ctor (elided) / bare `new Object()`   99 ns/op
                    //
                    // i.e. 200–800 ns of pure `jit_invoke_dispatch` round
                    // trip per allocation, against a 99 ns allocation, on
                    // the path that compiles every hot loop. `jit_entries`
                    // (CRATONVM_DBG_JIT_SCAN_PROF=1) reported exactly one
                    // entry per iteration, which is the tell.
                    //
                    // The two stated hazards are both already handled on
                    // this path and are NOT special to constructors:
                    // `note_recursive_compile_cycle` keeps a
                    // cycle-closing edge on dispatch, and
                    // `jit_direct_call_requires_dispatch` is consulted
                    // after the callee compiles. The background compile
                    // worker — which compiles the OSR bodies these loops
                    // run in — passes a LOOKUP-ONLY callee resolver
                    // (`direct_callee_lookup` in `jit_bridge.rs`), so there
                    // it binds an already-compiled callee and compiles
                    // nothing new at all.
                    //
                    // Scalar replacement is untouched: the IR builder still
                    // prefers ELISION for an elidable pc on a fresh
                    // `Op::New`, and an elided site emits no `Op::Call` for
                    // this entry to lower.
                    let _is_ctor = is_special && mn == "<init>";
                    // CALL-SITE INTRINSIC: hand the method back to the
                    // single-pass backend, which inlines it.
                    //
                    // The optimizing tier has no intrinsic emitter. Every
                    // invoke it admits becomes a real call — a direct
                    // cross-call for a statically-bound site, a MIC/PIC
                    // cascade for a virtual one — so admitting a site that
                    // `try_resolve_intrinsic` matches REPLACES inline
                    // machine code with a dispatch. That is a large loss,
                    // not a small one: measured on this branch, JDK 25,
                    // `for (…) sink = new CtorAtomic()` where the
                    // constructor body is `i = ATOMIC.getAndIncrement()`
                    //
                    //   IR body (intrinsic lost)      ~509 ns/op
                    //   single-pass (intrinsic kept)  ~183 ns/op
                    //
                    // and with the intrinsic kept `ctorAtomic` costs the
                    // same as `ctorPlain` (`i = ++staticInt`, ~189 ns/op),
                    // which is the signature of `lock xadd` actually being
                    // emitted. `CRATONVM_DBG=intrinsic` prints
                    // "IR body installed (single-pass call-site intrinsics
                    // NOT registered)" for exactly these bodies.
                    //
                    // This is the same lesson as the `Thread.currentThread`
                    // and String-intrinsic binds recorded in
                    // `jit_bridge.rs` — an intrinsic registered in one door
                    // is inert in the others — arrived at from the opposite
                    // direction: here the other door cannot emit it at all,
                    // so the fix is to route the method to the door that
                    // can rather than to duplicate the ladder.
                    //
                    // Scoped to methods that ACTUALLY contain such a site;
                    // everything else keeps the optimizing tier.
                    // `CRATONVM_JIT_IR_OVER_INTRINSIC=1` restores the old
                    // behaviour for A/B.
                    // THREE resolvers, not one. The layout-independent
                    // ladder is `try_resolve_intrinsic`; `AtomicInteger`
                    // and `String` have their own because they need a
                    // field layout, and checking only the first one made
                    // this refusal miss exactly the family that motivated
                    // it (`AtomicInteger.getAndIncrement`, netty's
                    // `FastThreadLocal` constructor). Both extra probes
                    // are asked in their most permissive form — any
                    // guard/layout — because the question here is "would
                    // the single-pass backend inline this?", not "can it
                    // inline it at this exact site": a false positive
                    // costs one method the optimizing tier, a false
                    // negative costs every call the intrinsic.
                    // JIT r23 wave 4 (lane L8, L8-8 stage 5;
                    // `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`, opt-in): a
                    // `MemorySegment` element site whose receiver profile names
                    // ONE JDK segment class the VM describes, in a method that
                    // reads a filed layout of the site's carrier, is expanded
                    // inline by the builder (`try_emit_ffm_element_inline`).
                    // It then neither routes the method to the single-pass
                    // backend nor refuses it below, and it is never spliced.
                    // JIT r23 wave 11 (lane L4): a speculation recorded at this
                    // bci (an unreached-site trap that fired, or any other).
                    let ffm_despec_at_pc = req.despec.is_some_and(|registry| {
                        let key = format!(
                            "{}.{}:{}",
                            cached.class_name, cached.method_name, cached.method_descriptor
                        );
                        registry.contains(&key, pc as u32)
                    });
                    // t1 wave 8 (lane FFM27, FFM27-2,
                    // `CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP`, default ON since j27): after
                    // the site's OWN unreached trap fired (and at most the
                    // one-class expansion's class test was withdrawn besides),
                    // a row naming two segment classes plans the bimorphic
                    // expansion before the one-class rules.
                    let ffm_after_trap_bimorphic = if ffm_despec_at_pc
                        && ffm_element_inline::ffm_bimorphic_after_trap_enabled()
                    {
                        let despec_ids: Vec<u32> = req.despec.map_or_else(Vec::new, |registry| {
                            let key = format!(
                                "{}.{}:{}",
                                cached.class_name, cached.method_name, cached.method_descriptor
                            );
                            registry
                                .entries_for(&key)
                                .into_iter()
                                .filter(|&(bci, _)| u32::try_from(pc).ok() == Some(bci))
                                .map(|(_, id)| id)
                                .collect()
                        });
                        ir_ffm_element_bimorphic_plan_after_trap(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            direct_helpers,
                            &ir_ffm_layout_facts,
                            &despec_ids,
                        )
                    } else {
                        None
                    };
                    let mut ir_ffm_inline_plan = match ffm_after_trap_bimorphic {
                        Some((site, second)) => {
                            ir_ffm_bimorphic_seconds.insert(pc, second);
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-ffm-element-bimorphic AFTER-TRAP {cn}.{mn}{desc} \
                                     at pc={pc} classes {} / {} in {}.{}{}",
                                    site.shape.class_id,
                                    second.class_id,
                                    cached.class_name,
                                    cached.method_name,
                                    cached.method_descriptor,
                                );
                            }
                            Some(site)
                        }
                        // t1 wave 9 (lane JITA,
                        // `CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_GUARDS`,
                        // default ON): the VM withdrew one of the expansion's
                        // guards at this bci (anything but the site's own
                        // unreached trap), so planting them again would only
                        // trade a deopt and a recompile per trap until the
                        // method is made not compilable. The site keeps the
                        // route of an unplanned site.
                        None if ffm_despec_at_pc
                            && ffm_element_inline::ffm_element_withdrawn_guards_enabled()
                            && ffm_element_inline::ffm_element_accessor(&cn, &mn, &desc)
                                .is_some()
                            && req.despec.is_some_and(|registry| {
                                let key = format!(
                                    "{}.{}:{}",
                                    cached.class_name, cached.method_name, cached.method_descriptor
                                );
                                let at_pc: Vec<u32> = registry
                                    .entries_for(&key)
                                    .into_iter()
                                    .filter(|&(bci, _)| u32::try_from(pc).ok() == Some(bci))
                                    .map(|(_, id)| id)
                                    .collect();
                                // j27 HANGHUNT (`CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY`,
                                // opt-in): only the expansion's own guards withdraw it.
                                ffm_element_inline::ffm_element_plan_withdrawn(&at_pc, pc)
                            }) =>
                        {
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-ffm-element-inline WITHDRAWN {cn}.{mn}{desc} \
                                     at pc={pc} in {}.{}{}: a guard was de-speculated here",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                            None
                        }
                        None => ir_ffm_element_inline_plan(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            direct_helpers,
                            &ir_ffm_layout_facts,
                            ffm_despec_at_pc && ir_ffm_unreached_site_trap_enabled(),
                        ),
                    };
                    // JIT round 24: a site whose receivers split between two
                    // segment classes, expanded as a class test with one
                    // expansion per class.
                    if ir_ffm_inline_plan.is_none() && !ffm_despec_at_pc {
                        if let Some((site, second)) = ir_ffm_element_bimorphic_plan(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            direct_helpers,
                            &ir_ffm_layout_facts,
                        ) {
                            ir_ffm_bimorphic_seconds.insert(pc, second);
                            ir_ffm_inline_plan = Some(site);
                        }
                    }
                    // JIT r23 wave 11 (lane L4): no plan because the site has
                    // never run -- an unreached-site trap instead of the
                    // whole-method refusal below.
                    let ffm_unreached = ir_ffm_inline_plan.is_none()
                        && ir_ffm_element_unreached_site(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            ffm_despec_at_pc,
                        );
                    if let (true, Some(acc)) = (
                        ffm_unreached,
                        ffm_element_inline::ffm_element_accessor(&cn, &mn, &desc),
                    ) {
                        ir_ffm_unreached_sites.insert(pc, acc);
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-unreached TRAP {cn}.{mn}{desc} at pc={pc} \
                                 in {}.{}{}",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                    }
                    // JIT r23 wave 13 (lane L4): the trap fired, but the eager
                    // recompile ran before the interpreter recorded a receiver:
                    // keep the site a call whose inline cache seeds the profile.
                    if ir_ffm_inline_plan.is_none()
                        && !ffm_unreached
                        && ir_ffm_element_seed_call_site(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            ffm_despec_at_pc,
                        )
                    {
                        ir_ffm_seed_call_sites.insert(pc);
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-seed CALL {cn}.{mn}{desc} at pc={pc} \
                                 in {}.{}{} (trap fired, no receiver yet)",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                    }
                    // t1 wave 6 (lane J6, FFM27-1,
                    // `CRATONVM_JIT_IR_FFM_RECEIVED_SEED_CALL`, opt-in): the
                    // trap fired and the row HAS receivers, but no plan: keep
                    // the site a call too, instead of refusing the method.
                    if ir_ffm_inline_plan.is_none()
                        && !ffm_unreached
                        && !ir_ffm_seed_call_sites.contains(&pc)
                        && ffm_element_inline::ffm_element_received_seed_call_site(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            ffm_despec_at_pc,
                        )
                    {
                        ir_ffm_seed_call_sites.insert(pc);
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-seed CALL {cn}.{mn}{desc} at pc={pc} \
                                 in {}.{}{} (trap fired, receivers but no plan)",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                    }
                    // t1 wave 6 (lane J6, FFM27-1): say which plan condition
                    // an unplanned element site failed, and the route it takes.
                    if ir_ffm_inline_plan.is_none()
                        && x64::dbg_jitc_enabled()
                        && cn == "java/lang/foreign/MemorySegment"
                        && ffm_element_accessor_name(&mn)
                    {
                        if let Some(why) = ffm_element_inline::ffm_element_plan_miss(
                            &cn,
                            &mn,
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            &ir_ffm_layout_facts,
                            ffm_despec_at_pc && ir_ffm_unreached_site_trap_enabled(),
                            |class_id| {
                                ffm_element_inline::ffm_segment_shape(direct_helpers, class_id)
                                    .is_some()
                            },
                        ) {
                            let route = if ffm_unreached {
                                "unreached-trap"
                            } else if ir_ffm_seed_call_sites.contains(&pc) {
                                "seed-call"
                            } else {
                                "no-plan (refused unless another bind serves it)"
                            };
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-plan MISS {cn}.{mn}{desc} at pc={pc} \
                                 in {}.{}{}: {why}; despec_at_pc={ffm_despec_at_pc} route={route}",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                    }
                    if let Some(site) = ir_ffm_inline_plan {
                        ir_ffm_inline_sites.insert(pc, site);
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-inline PLANNED {cn}.{mn}{desc} at pc={pc} \
                                 receiver class id {} bail_on_decline={} in {}.{}{}",
                                site.shape.class_id,
                                site.bail_on_decline,
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                            );
                        }
                    }
                    let is_intrinsic_site = try_resolve_intrinsic(&cn, &mn, &desc).is_some()
                        || try_resolve_atomic_intrinsic(&cn, &mn, &desc, 0).is_some()
                        || cn == "java/util/concurrent/atomic/AtomicInteger"
                        || try_resolve_atomic_long_intrinsic(&cn, &mn, &desc, 0).is_some()
                        || cn == "java/util/concurrent/atomic/AtomicLong"
                        // BOX_UNBOX. Named by TRIPLE, not by class, and
                        // that is the difference from the two `Atomic*`
                        // lines above. Those widen to the whole class
                        // because `AtomicLong` appears in a handful of
                        // methods and a false positive costs one of them
                        // the optimizing tier. `java/lang/Integer` is in
                        // half the tree, so the same shortcut here would
                        // push a large and unrelated population off the
                        // optimizing tier to buy nothing — the intrinsic
                        // serves exactly two triples.
                        //
                        // Calling the resolver with `guard_class_id: 0`
                        // would be a DEAD term: it returns `None` for 0
                        // (no layout can be derived), so the predicate
                        // would silently never fire. Asked directly
                        // instead.
                        || (cn == "java/lang/Long" && mn == "longValue" && desc == "()J")
                        || (cn == "java/lang/Integer" && mn == "intValue" && desc == "()I")
                        // Always `None` today, and left that way on
                        // purpose. `try_resolve_string_intrinsic` returns
                        // before its name ladder when the layout is `None`
                        // (`let layout = string_layout?;`), so this row
                        // cannot fire for any callee and the class-name row
                        // below is what actually covers String -- the
                        // "asked in its most permissive form" claim above
                        // is true of the two Atomic probes and NOT of this
                        // one. Handing it a probe layout would newly catch
                        // `java/lang/CharSequence` accessor sites, which
                        // reach the optimizing tier today, and DEMOTE them;
                        // that is a widening with its own measurement, not
                        // part of the String-expander fix.
                        || try_resolve_string_intrinsic(&cn, &mn, &desc, None).is_some()
                        // Every `java/lang/String` invoke EXCEPT the three
                        // accessors the optimizing tier can now expand for
                        // itself. Without the exception `ir.rs`'s
                        // `try_string_access_intrinsic` is unreachable by
                        // any input: this gate unsets `invoke_info` for the
                        // whole method, and `IrBuilder::build`'s `0xb6` arm
                        // bails on the missing entry BEFORE it offers the
                        // site to the expander. See
                        // `ir_string_access_expander_handles` for why the
                        // carve-out is exactly three names and one receiver
                        // kind, and for the kill switch that restores the
                        // blanket.
                        //
                        // nio20 L3 (NIO19-8): under
                        // `CRATONVM_JIT_IR_STRING_CALL_ADMIT` (default ON
                        // since r23 wave 6) only the
                        // String members the single-pass backend would
                        // intrinsify bail; see
                        // `string_site_bails_ir_planning`.
                        // JIT round 19 (lane irops19, IN16-1), opt-in:
                        // `String.valueOf` has no single-pass intrinsic to
                        // protect, so its site is an ordinary static call
                        // rather than a whole-method refusal.
                        || (string_site_bails_ir_planning_in(&cached.class_name, &cn, &mn, &desc)
                            && !(is_static && ir_ordinary_static_string_call_admits(
                                &cached.class_name, &cn, &mn, &desc))
                            && !(cn == "java/lang/String"
                                && ir_string_value_of_call_admits(&cn, &mn, &desc))
                            // JIT round 21, lane strings: the coder constructor
                            // `StringLatin1` / `StringUTF16` build their results with.
                            && !ir_string_coder_ctor_call_admits(
                                &cached.class_name,
                                &cn,
                                &mn,
                                &desc,
                            )
                            && !ir_string_coder_accessor_call_admits(&cn, &mn, &desc))
                        // FFM element accessors. Registered by their OWN
                        // arm in the single-pass scan rather than by
                        // `try_resolve_intrinsic` (they carry a dispatch
                        // info for the decline edge), so they have to be
                        // named here too — this predicate is what routes a
                        // method to the door that CAN emit an intrinsic,
                        // and a site the IR tier admits becomes a plain
                        // ~1158 ns/element dispatch. Found by an engagement
                        // counter reading `publishes=4000001 fast_hits=0`:
                        // the native was publishing verdicts and the
                        // compiled code was never asking.
                        //
                        // JIT round 23 wave 3, lane L8: not when this compile
                        // binds the site to its judged thin helper
                        // (`ir_ffm_element_direct_entry`), which serves the
                        // same cached road the intrinsic's helper does.
                        || (cn == "java/lang/foreign/MemorySegment"
                            && ffm_element_accessor_name(&mn)
                            && ffm_kind_for_descriptor(&desc).is_some()
                            && ir_ffm_element_direct_entry(&cn, &mn, &desc, direct_helpers)
                                .is_none()
                            && ir_ffm_inline_plan.is_none()
                            && !ir_ffm_unreached_sites.contains_key(&pc)
                            && !ir_ffm_seed_call_sites.contains(&pc))
                        // `MessageDigest.update(byte)`. Bound by its own arm
                        // in the single-pass scan, for the same reason the
                        // FFM row above is: it carries a dispatch info for
                        // the decline edge, so `try_resolve_intrinsic` does
                        // not name it and this predicate has to.
                        //
                        // THE SINGLE-BYTE `ByteBuffer` ACCESSORS USED TO BE
                        // ON THIS ROW TOO, AND ARE NOT ANY MORE. They were
                        // added here because the IR tier had no route to
                        // their helper, so a site that tiered up lost the
                        // fast path — the reasoning is preserved in the
                        // bind's own comment. It now HAS that route (see
                        // `NIO_BYTE_ELEMENT_SITES_IR`, the only thin-helper
                        // bind in the IR planner outside the
                        // `is_static || is_special` gate), so keeping the
                        // row would refuse the method the optimizing tier
                        // to protect a fast path that no longer needs
                        // protecting.
                        //
                        // What the row cost, measured on
                        // `probes/NioBufferCostProbe.java`: a heap
                        // `ByteBuffer.get(int)` ran at 574 ns because the
                        // helper serves DIRECT receivers only, so every
                        // heap call paid the helper crossing, then its
                        // decline, then the generic funnel — while the
                        // enclosing method was held out of the optimizing
                        // tier to buy that. This is the widest term in the
                        // NIO gap: the refusal is method-level, so ONE
                        // `buf.get(i)` anywhere in an HTTP codec method
                        // costs that whole method C2.
                        || (md_update_direct_helper_enabled()
                            && cn == "java/security/MessageDigest"
                            && mn == "update"
                            && desc == "(B)V")
                        // `VarHandle` read/write modes, bound at BOTH the
                        // single-pass and OSR doors and lost at this one for
                        // the same reason as the three rows above.
                        //
                        // MEASURED on `JdkZlibIntegrationTest#
                        // testHugeDecompress` after the other three landed:
                        // the census still showed **269 768 215**
                        // `VarHandle.get` bridge invocations — one per
                        // `ByteBuf.writeByte`, from the `RefCnt` read inside
                        // `ensureAccessible` — while
                        // `CRATONVM_DBG=jit-method-stats` reported
                        // `VarHandle.read=2/0`. Two sites bound at the
                        // single-pass door, none at OSR, and the tiny hot
                        // accessor that holds the site tiering up to here.
                        //
                        // Round 15 wave 2 (lane vh2): only for a slot this
                        // compile's table wires. A VM that minted an exact
                        // handle zeroes all three tables (`vm::jit::helpers::
                        // varhandle_thin_binds_admitted`), and the refusal
                        // would then protect a bind the single-pass door
                        // will not make.
                        //
                        // Round 15 wave 4 (lane vh6; F1 of
                        // `r15w4-vh5-review-findings-CLOSED-20260930.md`): and only
                        // when the single-pass ladder's own admission (the
                        // raw-callee gate, `--real-jdk`'s registry answer)
                        // would make the bind: `varhandle_ir_refusal_protects_bind`.
                        || (cn == "java/lang/invoke/VarHandle"
                            && varhandle_ir_refusal_protects_bind(
                                &mn,
                                &desc,
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                            )
                            && varhandle_ir_access_entry(
                                &mn, &desc, direct_helpers, jdk_only, direct_bind_admission,
                            ).is_none()
                            // nio21 wave 12 L2: not for a site reading a
                            // constant handle, which the optimizing tier's
                            // prefix serves inline with the same thin helper
                            // bound behind it (`ir_vh_const_site_admitted`,
                            // `CRATONVM_JIT_IR_VH_CONST_ADMIT`).
                            && !ir_vh_const_site_admitted(
                                pc,
                                &mn,
                                &desc,
                                &ir_vh_const_getstatics,
                                &|class_id, field_index| {
                                    ir_lower::vh_const_site_served(
                                        direct_helpers,
                                        helpers,
                                        &mn,
                                        &desc,
                                        class_id,
                                        field_index,
                                        cached.declaring_class_id.as_u32(),
                                    )
                                },
                            ));
                    // A call-site intrinsic whose emitted form is a
                    // handful of straight-line instructions is lowered by
                    // the optimizing tier as ARITHMETIC -- better than
                    // either the refusal below or a generic dispatch. The
                    // planner records the site and builds no `invoke_info`
                    // row for it; `IrBuilder::try_emit_scalar_intrinsic` is
                    // the other half, and both ask the SAME function.
                    // A site the compile door found unresolved stays a call
                    // (its row is built below): the guard rides on the call.
                    let first_call_site = ir_first_call.contains_key(&pc);
                    // A `VarHandle.get*` over a constant instance-field handle
                    // is a guarded field load (`UnboxOp::FieldInt` / `FieldLong`),
                    // not a call. Which handle, and so which slot and receiver
                    // class, is decided by the builder from the site's first
                    // operand; the planner only records that the site MAY fold
                    // and, like the other families, builds no `invoke_info` row
                    // for it. A site that does not fold therefore bails the
                    // build to the single-pass backend, whose thin `VarHandle`
                    // helper binds are what these methods used before.
                    if !first_call_site && is_virtual && ir_vh_plan_count > 0 {
                        if let Some(wide) = ir_vh_field_read_candidate(&cn, &mn, &desc) {
                            let candidate = if wide {
                                ir::UnboxOp::FieldLong(0)
                            } else {
                                ir::UnboxOp::FieldInt(0)
                            };
                            ir_unbox_intrinsic_sites.insert(pc, (candidate, 0));
                            continue;
                        }
                    }
                    // A `ScopedMemoryAccess` raw read/write is one memory access
                    // for the shapes a `ByteBuffer` produces (see
                    // `UnboxOp::ScopedGet`). Unlike the `VarHandle` candidate the
                    // site KEEPS the call row built below, bound to the thin
                    // helper as before: the builder takes the inline access only
                    // where a snapshot resumes its guards, and every other site
                    // stays the helper call it was. A site whose guard already
                    // failed often enough to be withdrawn (de-spec) keeps the
                    // call too.
                    //
                    // Not inside `ScopedMemoryAccess` itself: its wrappers and
                    // `*Internal` twins take the base as a parameter, so the
                    // guard cannot know the off-heap (arena handle) case from
                    // the array one and would fail on every call of the
                    // standalone-compiled wrapper that a direct buffer reaches.
                    if !first_call_site
                        && (is_virtual || is_special)
                        && &*cached.class_name != "jdk/internal/misc/ScopedMemoryAccess"
                    {
                        if let Some(uop) = ir::try_ir_scoped_unbox(&cn, &mn, &desc) {
                            let withdrawn = req.despec.is_some_and(|registry| {
                                registry.contains(
                                    &format!(
                                        "{}.{}:{}",
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor
                                    ),
                                    pc as u32,
                                )
                            });
                            if !withdrawn {
                                ir_unbox_intrinsic_sites.insert(pc, (uop, 0));
                            }
                        }
                    }
                    if let Some(sop) = ir::try_ir_scalar_intrinsic(&cn, &mn, &desc)
                        .filter(|_| !first_call_site || ir::scalar_intrinsic_first_call_enabled())
                    {
                        ir_scalar_intrinsic_sites.insert(pc, sop);
                        continue;
                    }
                    // j27 wave 2 (lane INTRINS): `Integer`/`Long.reverse`,
                    // `Math.copySign`, `Math`/`StrictMath.signum` expand into
                    // plain IR arithmetic (`ir::pure_expand`), same contract.
                    if let Some(exp) = ir::try_ir_pure_expansion(&cn, &mn, &desc).filter(|_| {
                        is_static && (!first_call_site || ir::scalar_intrinsic_first_call_enabled())
                    }) {
                        ir_pure_expansion_sites.insert(pc, exp);
                        continue;
                    }
                    // An unboxing accessor is a GUARDED FIELD LOAD, not
                    // arithmetic, so it gets its own recognizer and its own
                    // site map -- but the same contract: the site is
                    // recorded, no `invoke_info` row is built, and the
                    // builder emits the load in place of the call. The
                    // receiver class id comes from the constant pool, and a
                    // site whose id does not resolve declines (the emitter
                    // would have nothing to derive an offset from).
                    if let Some(uop) = cp_invoke_class_id_resolver
                        .filter(|_| !first_call_site)
                        .and_then(|r| r(cp_idx))
                        .and_then(|cid| {
                            let match_class = ir_unbox_match_class(
                                &cn,
                                &mn,
                                &desc,
                                cp_idx,
                                cp_invoke_declaring_class_resolver,
                            );
                            ir::try_ir_unbox_intrinsic(match_class, &mn, &desc, cid)
                                .map(|op| (op, cid))
                        })
                    {
                        ir_unbox_intrinsic_sites.insert(pc, uop);
                        continue;
                    }
                    let cold_osr_intrinsic = if is_intrinsic_site && req.osr_mode_exit_polls {
                        repeated_intrinsic_pcs
                            .get_or_insert_with(|| {
                                let handlers: Vec<(usize, usize, usize)> = cached
                                    .exception_table
                                    .iter()
                                    .map(|h| {
                                        (
                                            usize::from(h.start_pc),
                                            usize::from(h.end_pc),
                                            usize::from(h.handler_pc),
                                        )
                                    })
                                    .collect();
                                bytecode_analysis::InsnCfg::build_with_handlers(
                                    &cached.code,
                                    cached.code.len(),
                                    &handlers,
                                )
                                .map(|cfg| cfg.cyclic_pcs())
                            })
                            .as_ref()
                            .is_some_and(|pcs| !pcs.contains(&pc))
                    } else {
                        false
                    };
                    let arraycopy_entry = ir_arraycopy_entry(
                        &cn,
                        &mn,
                        &desc,
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                    );
                    if !ir_over_intrinsic_enabled()
                        && is_intrinsic_site
                        && !cold_osr_intrinsic
                        && arraycopy_entry.is_none()
                        && !ir_arrays_equals_call_admits(&cn, &mn, &desc)
                        // j27 wave 8 (lane W8REV): `Arrays.fill` / `sort` /
                        // `CRC32C.update` stay calls, opt-in.
                        && !ir_loop_intrinsic_call::admits(&cn, &mn, &desc)
                        && !ir_ffm_element_call_admits(&cn, &mn)
                        && ir_ffm_element_direct_entry(&cn, &mn, &desc, direct_helpers).is_none()
                        && ir_ffm_inline_plan.is_none()
                        // JIT r23 wave 11 (lane L4): a never-reached element
                        // site is built as its trap instead.
                        && !ir_ffm_unreached_sites.contains_key(&pc)
                        // JIT r23 wave 13 (lane L4): a seed-call site stays a call.
                        && !ir_ffm_seed_call_sites.contains(&pc)
                    {
                        // JIT r23 wave 12 (lane L7,
                        // `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP`): and so is any
                        // other site no execution has reached. It gets no
                        // `invoke_info` row; the builder plants its trap.
                        if let Some(shape) = ir_unreached_call_site_shape(
                            &desc,
                            opcode,
                            pc,
                            ir_first_call.contains_key(&pc),
                            profile,
                            req.despec.is_some_and(|registry| {
                                let key = format!(
                                    "{}.{}:{}",
                                    cached.class_name, cached.method_name, cached.method_descriptor
                                );
                                u32::try_from(pc).map_or(true, |b| registry.contains(&key, b))
                            }),
                        ) {
                            ir_unreached_call_sites.insert(pc, shape);
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-unreached-call TRAP {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                            continue;
                        }
                        // Every family that reaches here has an emitted
                        // intrinsic that replaces a LOOP -- `arraycopy`,
                        // `Arrays.fill`/`equals`/`sort`,
                        // `String.equals`/`indexOf`/`hashCode`, CRC32 --
                        // or a memory form this tier has no node for
                        // (unboxing, the `Atomic*` accessors, `VarHandle`).
                        // For those the intrinsic really is worth more than
                        // the rest of the method's optimization, so the
                        // method-level refusal stays. The counter is the
                        // work list for whoever extends
                        // `try_ir_scalar_intrinsic`; it is what says how
                        // much of the original 68-method H2 refusal is left.
                        ir::note_scalar_intrinsic_refused();
                        all_emittable = false;
                        if ir_stage_reporting() {
                            nonemittable = Some(format!(
                                "pc={pc}: {cn}.{mn}{desc} is a loop-shaped or memory call-site intrinsic; \
                                 the IR tier has no node for it"
                            ));
                        }
                        break;
                    }
                    let (desc_args, ret) = match static_call_shape(&desc) {
                        Some(t) => t,
                        None => {
                            all_emittable = false;
                            if ir_stage_reporting() {
                                nonemittable = Some(format!(
                                    "pc={pc}: static_call_shape refused {cn}.{mn}{desc}"
                                ));
                            }
                            break;
                        }
                    };
                    // JVMS §6.5 super-call redirect of an `invokespecial` site,
                    // asked once: the self-call check, the direct bind and the
                    // baked `JitInvokeInfo` below must all name the same
                    // selection start. The baked info used to keep the
                    // constant-pool class, so a redirected super call that
                    // stayed on the dispatch helper ran the constant-pool
                    // class's method while the direct bind ran the selection
                    // start's (interpreter round i1 wave 14, L2).
                    // A constructor is never redirected (JVMS §6.5 excludes
                    // `<init>`), so its site does not ask.
                    let special_redirect: Option<(String, u32)> =
                        if opcode == 0xb7 && mn != "<init>" {
                            cp_invokespecial_owner_resolver.and_then(|r| r(cp_idx, opcode))
                        } else {
                            None
                        };
                    // fib44 perf-regression fix. `static_call_shape` admitting a
                    // wide (`J`/`D`/`F`) RETURN (inc-29/32/33) let a SELF-RECURSIVE
                    // long/FP method lower its recursive call to an IR `Op::Call`,
                    // which is routed through the generic `jit_invoke_dispatch`
                    // runtime helper on EVERY invocation. Single-pass instead emits
                    // a DIRECT call to this method's own compiled entry — far cheaper
                    // for a hot recursive method (`static long fib(int)`: ~8.6x; the
                    // dispatch helper does note_jit_boundary + SATB flush + a
                    // native-stack recursion guard per call). Keep such methods on
                    // single-pass: when the resolved callee IS this method and the
                    // return is wide, mark the body non-emittable so it bails. The
                    // intended unblock — CROSS-method wide-return calls (e.g.
                    // `Pack.bigEndianToLong`) — is non-self-recursive and unaffected.
                    // Round 9 wave 10 (`call10`): a statically bound INSTANCE
                    // self-call has no direct route in this tier -- the
                    // `invoke_kind == 4` marshal below is static-only and
                    // carries no receiver null check -- so it asked
                    // `callee_compiler` for ITSELF, was declined
                    // (`eager-callee-chain-cycle`) and became a blind MIC
                    // dispatch on every recursion (`MutPriv self`: 2.8 s vs
                    // HotSpot's 25 ms). Hand such a method to the single-pass
                    // tier, which plans the same site as a
                    // `JIT_SELF_CALL_GUARD` direct self-call; the predicate is
                    // the one that planner asks, so the bail happens exactly
                    // when that route will be taken (the same exit the
                    // wide-return self-recursion takes above).
                    //
                    // JIT round 23 wave 3d (lane L1, proxy proposal 4e;
                    // `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`, default
                    // OFF): such a site may instead stay in this tier as a
                    // receiver-carrying kind-4 direct self-call
                    // (`instance_self_direct`, see
                    // [`ir_instance_self_call_direct_admits`]).
                    let mut instance_self_direct = false;
                    if is_special
                        && !is_static
                        && mn.as_str() == &*cached.method_name
                        && desc.as_str() == &*cached.method_descriptor
                    {
                        let owner: Option<&str> = if opcode == 0xb7 {
                            special_redirect.as_ref().map(|(owner, _)| owner.as_str())
                        } else {
                            private_virtual_owner.as_deref()
                        };
                        let owner: &str = owner.unwrap_or(cn.as_str());
                        if instance_self_call_goes_direct(
                            cached,
                            opcode,
                            true,
                            owner,
                            &mn,
                            &desc,
                            req.self_call_identity_stable,
                            helpers.self_call_stack_guard,
                        ) {
                            if ir_instance_self_call_direct_admits(
                                ir_direct_instance_self_call_enabled(),
                                selfrec_direct,
                                ret,
                                selfrec_direct_void_admits(
                                    selfrec_direct_void,
                                    req.self_call_identity_stable,
                                ),
                                desc_args,
                                crate::ir_lower::incoming_abi_reg_capacity(),
                            ) {
                                instance_self_direct = true;
                            } else if ir_cold_instance_self_call_dispatch_enabled()
                                && site_unreached_by_branch_profile(profile, code, code_len, pc)
                            {
                                // JIT round 23 wave 9 (lane L1, proxy-define
                                // page): the profile proves this self-call's
                                // arm never ran (`SplitConstantPool.findEntry`
                                // / `tryFindUtf8`: the retry after
                                // `fullScan()`, behind `doneFullScan`), so it is
                                // planned below as the ordinary statically bound
                                // call, and the method keeps its optimizing
                                // body instead of bailing whole to single-pass
                                // for a call that does not run.
                                if ir_stage_reporting() {
                                    eprintln!(
                                        "[ir] pc={pc}: cold instance self-call {cn}.{mn}{desc} \
                                         planned as a dispatched call (never run per the \
                                         profile)"
                                    );
                                }
                            } else {
                                all_emittable = false;
                                if ir_stage_reporting() {
                                    nonemittable = Some(format!(
                                        "pc={pc}: instance self-call {cn}.{mn}{desc} goes to single-pass"
                                    ));
                                }
                                break;
                            }
                        }
                    }
                    let is_self_recursive = is_static
                        && cn.as_str() == &*cached.class_name
                        && mn.as_str() == &*cached.method_name
                        && desc.as_str() == &*cached.method_descriptor;
                    let is_self_recursive_wide =
                        is_self_recursive && matches!(ret, b'J' | b'D' | b'F');
                    // The direct self-call marshals the hidden context pointer
                    // plus EVERY Java argument into an entry-ABI register and
                    // has no stack-argument path of its own, so it can only
                    // serve a method that fits the register file: four on
                    // Win64, six on SysV. `desc_args + 1` because a
                    // self-recursive call is `invokestatic` (no receiver) and
                    // the context takes `abi[0]`.
                    //
                    // Without this the marshal indexes off the end and PANICS
                    // THE COMPILER THREAD, which does not come back — so the
                    // first `static long f(int,int,int,int)` that calls itself
                    // silently disables the JIT for the rest of the process.
                    // MEASURED 2026-08-29 on Windows: `probes/SelfRecArgs.java`
                    // compiles f1/f2/f3, panics on f4, and nothing compiles
                    // after it. It reached a real workload as a HANG —
                    // Hibernate's `DefaultCatalogAndSchemaTest` runs to a
                    // 600 s timeout entirely interpreted.
                    //
                    // The requirement is stated where the marshal is
                    // (`ir_lower::emit_self_recursive_call`) and enforced
                    // here, because eligibility is the only place that can
                    // still choose a different route.
                    let selfrec_fits =
                        desc_args + 1 <= crate::ir_lower::incoming_abi_reg_capacity();
                    // CENSUS, on the existing compile-reporting flag rather
                    // than a new one: this refusal is INVISIBLE from Java —
                    // the method still runs and still answers correctly, it
                    // just takes a slower route — so without a line here
                    // "how much code does this guard turn away" has no
                    // answer at all. Before the guard existed the same
                    // population panicked the compiler thread instead, and
                    // that was invisible too, which is how it survived to
                    // reach a suite. One line per refused SITE; the reader
                    // counts distinct methods.
                    if is_self_recursive && !selfrec_fits && false {
                        eprintln!(
                            "CRATONVM_DBG_JIT_COMPILED: selfrec-refused {cn}.{mn}{desc} \
                                 args={desc_args} entry_regs={}",
                            crate::ir_lower::incoming_abi_reg_capacity()
                        );
                    }
                    // Integer and reference self-recursion has the same direct-call
                    // ABI as the already-supported wide-return path. A void call
                    // stayed on `jit_invoke_dispatch` ("no result slot to fill");
                    // under `CRATONVM_JIT_IR_SELFREC_DIRECT_VOID` (JIT round 19
                    // wave 3, lane throws3, THROWS3-1) it takes the same direct
                    // route -- see `selfrec_direct_void_admits`.
                    let is_self_recursive_direct = selfrec_direct
                        && helpers.self_call_stack_guard != 0
                        && is_self_recursive
                        && (ret != b'V'
                            || selfrec_direct_void_admits(
                                selfrec_direct_void,
                                req.self_call_identity_stable,
                            ))
                        && selfrec_fits;
                    // `!selfrec_fits` takes the same exit as `!selfrec_direct`,
                    // and for the same reason: a self-recursive WIDE-return
                    // method that cannot take the direct route must go to
                    // single-pass, which emits its own direct self-call, rather
                    // than to `jit_invoke_dispatch` — that route is what the
                    // fib44 note above measured at 8.6x slower.
                    if is_self_recursive_wide && (!selfrec_direct || !selfrec_fits) {
                        // Opt-out: bail the whole method to single-pass (fast
                        // direct self-call). The direct path keeps
                        // it on the IR path with a direct self-call instead
                        // (invoke_kind 4 below).
                        all_emittable = false;
                        if ir_stage_reporting() {
                            nonemittable = Some(format!(
                                "pc={pc}: self-recursive wide return {cn}.{mn}{desc}"
                            ));
                        }
                        break;
                    }
                    // Every kind except `invokestatic` marshals the receiver
                    // as arg0 (a reference → one GPR slot), so it carries one
                    // more JIT arg than its descriptor lists. `invoke_kind`
                    // matches the dispatch helper's encoding: 0 = virtual,
                    // 1 = special (non-virtual dispatch to the resolved
                    // target), 2 = interface, 3 = static. For virtual /
                    // interface the helper dispatches on the receiver's
                    // runtime class (the baked class/name/descriptor are the
                    // static call-site signature it resolves against).
                    let has_receiver = !is_static;
                    let num_args = desc_args + if has_receiver { 1 } else { 0 };
                    // `mut`: the CHA-bound caller-held synchronized arm below
                    // (round 11 wave 13) re-marks its genuinely virtual site
                    // statically bound (1) once its class guard is planned.
                    let mut invoke_kind: u8 = if is_self_recursive_direct || instance_self_direct {
                        // 4 = guarded self-recursive static DIRECT call
                        // (wave 3d: or a statically bound INSTANCE self-call
                        // carrying its receiver in argument 0; the VM is
                        // handed a kind-1 twin of this row, see
                        // `ir_self_call_service_infos` below).
                        // `ir_lower` emits a direct `CALL`
                        // to this method's own entry instead of routing through
                        // `jit_invoke_dispatch`. Never seen by the dispatch
                        // helper (the direct path never calls it).
                        4
                    } else if is_special {
                        1
                    } else if is_virtual {
                        0
                    } else if is_interface {
                        2
                    } else {
                        3
                    };
                    // IR direct-call lowering. `invokestatic` and a
                    // non-`<init>` `invokespecial` are STATICALLY bound, so
                    // the resolved callee is the only possible target and no
                    // receiver type check is needed — exactly the property
                    // that lets the single-pass backend bind them directly
                    // (`x64.rs` `direct_calls`). Mirror its guards here:
                    //
                    //  * `verdicts.note_recursive_compile_cycle` — a call that
                    //    closes an in-flight compile cycle (A-B-A) keeps the
                    //    dispatch route, whose depth guard is the only stack
                    //    protection such an edge has.
                    //  * `verdicts.direct_call_requires_dispatch_of` — the
                    //    cycle members that must stay on dispatch (the
                    //    registry's sticky set; every recorded member under
                    //    `CRATONVM_JIT_CYCLE_DIRECT_BIND=0`), matched by the
                    //    site's class identity where it has one.
                    //  * self-recursion is handled by the dedicated
                    //    `invoke_kind == 4` path above, not here.
                    //  * JVMS §6.5 — for `invokespecial` the direct target is
                    //    the *selection-start* class, not the plain CP class,
                    //    so apply `cp_invokespecial_owner_resolver` first
                    //    (single-pass does the same before its
                    //    `callee_compiler` call). Without this a super call
                    //    could be bound to the wrong method body.
                    if is_static {
                        STATIC_SITES_SEEN_IR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let mut direct_target: Option<(usize, bool)> = None;
                    // Set when `direct_target` came from a thin VM-side
                    // helper rather than from a compiled callee. Those two
                    // are the same thing to the *lowerer* — both are just
                    // an address to `CALL` — and completely different to
                    // `prepare_for_publication`, which pins every entry in
                    // `_direct_callee_entries` and REFUSES to publish a
                    // body whose baked target it cannot resolve to a live
                    // JIT artifact. A thin helper is a process-lifetime
                    // `extern "C"` function with no artifact to resolve, so
                    // recording it there would make every compile that
                    // binds one look like the use-after-free window that
                    // check exists to catch.
                    let mut direct_target_is_thin_helper = false;
                    if ir_direct
                        && (is_static || is_special)
                        && !is_self_recursive
                        && !instance_self_direct
                    {
                        let special_owner: Option<String> = if opcode == 0xb7 {
                            special_redirect.as_ref().map(|(owner, _)| owner.clone())
                        } else {
                            private_virtual_owner.clone()
                        };
                        let direct_class: &str = special_owner.as_deref().unwrap_or(cn.as_str());
                        // The class the recursion-cycle checks name this site
                        // by: the resolved method's DECLARING class for an
                        // inherited-name call (`Sub.f` -> `A.f`), `direct_class`
                        // otherwise. See `cycle_site_declaring_class`.
                        let cycle_declaring = cycle_site_declaring_class(
                            verdicts,
                            cp_invoke_declaring_class_resolver,
                            cp_idx,
                            direct_class,
                            &mn,
                            &desc,
                            || direct_class == cn.as_str(),
                        );
                        let cycle_class: &str = cycle_declaring.as_deref().unwrap_or(direct_class);
                        // The loaded class `cycle_class` names, for the
                        // recursion-cycle sets: the constant pool's own
                        // resolution while it IS the constant-pool class, the
                        // VM's id for an inherited-name call's declaring class
                        // (wave 17), and names for a selection-start redirect.
                        // Asked only when a cycle member has these names.
                        let direct_class_id = || {
                            if cycle_declaring.is_some() {
                                return cycle_site_declaring_class_id(
                                    req.cp_invoke_declaring_class_id_resolver,
                                    cp_idx,
                                );
                            }
                            if cycle_class != cn.as_str() {
                                return None;
                            }
                            let resolve = cp_invoke_class_id_resolver?;
                            resolve(cp_idx).map(cratonvm_types::ClassId::new)
                        };
                        // Statically bound door (`is_static || is_special`):
                        // the closing edge, if this is one, is never an
                        // inline cache.
                        let closes_cycle = verdicts.note_recursive_compile_cycle(
                            cycle_class,
                            &mn,
                            &desc,
                            false,
                            direct_class_id,
                        );
                        // `Thread.currentThread()` thin direct-call native
                        // helper. This has to be recognised HERE as well as
                        // in the single-pass ladder, because the two
                        // backends resolve direct calls independently and
                        // this loop asks only `callee_compiler` — which
                        // answers `None` for a registered native, since a
                        // native has no compiled body to bind.
                        //
                        // That asymmetry is exactly why the interpreter-side
                        // `Thread.currentThread` fix read as landed while
                        // being inert with the JIT on: hot loops are
                        // OSR-compiled at the optimizing tier, which is this
                        // path, and it had no route to the thin helpers at
                        // all. Measured with `CRATONVM_INTRINSIC_STATS=1`:
                        // the compiled-code bypass counter sat at 0 for a
                        // 8,000,000-call loop until this was added.
                        //
                        // Scope note: of the other six `*_DIRECT_FN` helpers,
                        // `Integer.valueOf` and `Integer.intValue` are bound
                        // at this door too now (below); the two `HashMap` ones
                        // and the two `String` lower-case ones are still
                        // single-pass-only for the same reason.
                        // That is a real and separate finding — see
                        // `native-call-funnel-per-call-floor-item2-20260805.md`
                        // — and is deliberately NOT fixed here: each of
                        // those changes what the optimizing tier emits on a
                        // measured hot path, and none of them has been
                        // A/B'd at this tier.
                        if is_static
                            && direct_class == "java/lang/Thread"
                            && mn == "currentThread"
                            && desc == "()Ljava/lang/Thread;"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.thread_current_thread,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                // `needs_ctx = true`: the helper's first
                                // argument is `vm_ptr`, which
                                // `emit_direct_cross_call` loads from
                                // `context_slot_off` into `ENTRY_ABI_REGS[0]`.
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                THREAD_CURRENT_THREAD_SITES_IR
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // nio21 wave 3, lane L4 (step 5 of
                        // `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`):
                        // `Thread.currentCarrierThread()`, whose registered
                        // native IS `currentThread`'s, on the same terms --
                        // its own helper, so a decline dispatches this triple.
                        // Counted with the `currentThread` sites.
                        if direct_target.is_none()
                            && is_static
                            && direct_class == "java/lang/Thread"
                            && mn == "currentCarrierThread"
                            && desc == "()Ljava/lang/Thread;"
                            && crate::direct_helpers::carrier_thread_direct_enabled()
                        {
                            let entry = direct_native_helper(
                                direct_helpers.thread_current_carrier_thread,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                THREAD_CURRENT_THREAD_SITES_IR
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        if direct_target.is_none() && is_static {
                            let entry = system_clock_direct_helper(
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // The two census-driven helpers, at THIS door too.
                        // The scope note above says the other six stayed
                        // single-pass-only because none had been A/B'd at
                        // the optimizing tier; these two are added here
                        // deliberately, because the workload that motivates
                        // them — netty's `writeZero`, 131 072
                        // `ByteBuffer.putLong` per MiB — runs entirely in
                        // OSR/optimizing-tier bodies, so a single-pass-only
                        // bind would be inert exactly where it is needed.
                        // Both doors are counted separately
                        // (`census_direct_helper_sites`) so that claim is
                        // checkable rather than assumed.
                        if direct_target.is_none()
                            && census_direct_helpers_enabled()
                            && is_static
                            && direct_class == "jdk/internal/util/Preconditions"
                            && mn == "checkIndex"
                            && desc == "(IILjava/util/function/BiFunction;)I"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.preconditions_check_index,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                PRECONDITIONS_CHECK_INDEX_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        if direct_target.is_none()
                            && is_static
                            && direct_class == "java/lang/System"
                            && ((mn == "nanoTime" && desc == "()J")
                                || (mn == "currentTimeMillis" && desc == "()J"))
                        {
                            let cell = if mn == "nanoTime" {
                                direct_helpers.system_nano_time
                            } else {
                                direct_helpers.system_current_time_millis
                            };
                            let entry = direct_native_helper(
                                cell,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // JIT round 16 (lane strings, STR16-6): the JDK's
                        // int/long -> LATIN1 digit writer, one thin call in
                        // place of its per-digit `Unsafe.putByte` chain. At
                        // this door because `AbstractStringBuilder.append(I)`
                        // and `Integer.toString(I)` are optimizing-tier bodies
                        // once hot. See `decimal_digits_latin1_helper`.
                        if direct_target.is_none() && is_static {
                            let entry = decimal_digits_latin1_helper(
                                direct_helpers,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                DECIMAL_DIGITS_CENSUS[DECIMAL_DIGITS_SITES_IR]
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // Strings round 2 (`str17`): a primitive array's hash
                        // (`String.hashCode()` of a fresh string) as one thin
                        // call. See `vectorized_hash_code_helper`.
                        if direct_target.is_none() && is_static {
                            let entry = vectorized_hash_code_helper(
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // JIT round 23 wave 4, lane L5: `Array.newArray` (the
                        // allocation inside `Arrays.copyOf` / `toArray(T[])`)
                        // as one thin call. See `reflect_new_array_helper`.
                        if direct_target.is_none() && is_static {
                            let entry = reflect_new_array_helper(
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                if crate::x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] reflect-new-array-direct bind (ir) {cn}.{mn} @pc={pc}"
                                    );
                                }
                            }
                        }
                        // JIT round 22 wave 1, lane intrinsics (INTRINSICS21-1):
                        // `ArraysSupport.vectorizedMismatch` as one thin call.
                        // See `vectorized_mismatch_helper`.
                        if direct_target.is_none() && is_static {
                            let entry = vectorized_mismatch_helper(
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // JIT round 22 wave 2, lane intrinsics (INTRINSICS22-2):
                        // the CRC32 / Adler32 `byte[]` checksums as one thin
                        // call. See `zip_checksum_helper`.
                        if direct_target.is_none() && is_static {
                            let entry = zip_checksum_helper(
                                direct_helpers,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        if direct_target.is_none()
                            && census_direct_helpers_enabled()
                            && is_static
                            && direct_class == "java/lang/ref/Reference"
                            && mn == "reachabilityFence"
                            && desc == "(Ljava/lang/Object;)V"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.reachability_fence,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                REACHABILITY_FENCE_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // W2A: `System.identityHashCode` as one thin CALL (the
                        // header-read prefix still answers a hashed object).
                        if direct_target.is_none()
                            && is_static
                            && identity_hash_direct_enabled()
                            && is_identity_hash_code_site(direct_class, &mn, &desc)
                        {
                            let entry = direct_native_helper(
                                direct_helpers.identity_hash_code,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // `Long.valueOf(J)` at THIS door too, for the reason
                        // the two above are here: the netty workloads that
                        // motivate it box inside loops, and a loop is
                        // OSR-compiled at the optimizing tier, which is this
                        // path. The `Integer.valueOf` twin was left
                        // single-pass-only so the A/B on
                        // `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS` measured the
                        // `Long` binds alone; it is bound here too since round
                        // 11 wave 11 (the arm after this one).
                        //
                        // `Long.longValue()` cannot be bound here at all:
                        // this door is gated `is_static || is_special`, and
                        // `longValue` is an `invokevirtual`. That is the
                        // same reason `Integer.intValue` is single-pass-only
                        // and is a property of the door, not a decision.
                        if direct_target.is_none()
                            && long_box_direct_helpers_enabled()
                            && is_static
                            && direct_class == "java/lang/Long"
                            && mn == "valueOf"
                            && desc == "(J)Ljava/lang/Long;"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.long_value_of,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                LONG_VALUE_OF_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // `Integer.valueOf(I)` at the OPTIMIZING door too
                        // (round 11 wave 11, lane `hashmap`'s patch page
                        // `r11w11-hashmap-ir-valueof-direct-bind-patch`, landed
                        // by lane `calls`). The single-pass tier and the OSR
                        // door bind it already; without this an optimizing
                        // body pays `jit_invoke_dispatch` plus the native
                        // funnel per box. `jit_integer_value_of_direct` keeps
                        // the IntegerCache identity for -128..127 (and for a
                        // raised `IntegerCache.high`) by routing that range
                        // through the canonical native. `Intrinsic`, so
                        // `direct_native_helper` admits it in both modes. The
                        // box-unbox fold (`ea_ir_bridge`) keys on the call's
                        // `JitInvokeInfo` names, not on its route, so a folded
                        // site simply leaves this row unused.
                        if direct_target.is_none()
                            && is_static
                            && direct_class == "java/lang/Integer"
                            && mn == "valueOf"
                            && desc == "(I)Ljava/lang/Integer;"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.integer_value_of,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                            }
                        }
                        // `Integer.intValue()` / `Long.longValue()` at the
                        // OPTIMIZING door.
                        //
                        // The scope note above says these are
                        // "single-pass-only" because they are
                        // `invokevirtual` and this door is gated
                        // `is_static || is_special`. That was true when it
                        // was written and is not true now:
                        // `invokevirtual_site_final_owner` pins an
                        // unoverridable `invokevirtual` as statically
                        // bound, and BOTH wrapper classes are `final`, so
                        // `private_virtual_owner` is `Some` and the site
                        // arrives here as `is_special` with `direct_class`
                        // already the declaring class. The door the note
                        // says these sites cannot reach is the door they
                        // now come through.
                        //
                        // `needs_ctx = true` puts `vm_ptr` in ARG_REGS[0]
                        // ahead of the receiver, which is the helpers' own
                        // `(vm_ptr, receiver)` signature — the same
                        // convention `Thread.currentThread` uses above.
                        //
                        // `Long.longValue` is NOT bound here, and the
                        // reason is a measurement rather than an argument.
                        // The argument applies unchanged -- `java/lang/Long`
                        // is `final` too, so its sites arrive here pinned
                        // exactly as `Integer`'s do -- but the arm was
                        // written, built and measured, and
                        // `CRATONVM_DBG_DIRECT_BINDS=1` reported
                        // `Long.longValue: sites_bound=0` against
                        // `Integer.intValue: sites_bound=4 served=159 199`
                        // on the same run. Nothing on the workload this
                        // change is measured against reaches it, so it
                        // ships as a follow-up rather than as unexercised
                        // code: re-add the `Long` half and watch
                        // `sites_bound` move before believing it.
                        if direct_target.is_none()
                            && int_value_direct_enabled()
                            && !is_static
                            && direct_class == "java/lang/Integer"
                            && mn == "intValue"
                            && desc == "()I"
                        {
                            let entry = direct_native_helper(
                                direct_helpers.integer_int_value,
                                jdk_only,
                                direct_bind_admission,
                                direct_class,
                                &mn,
                                &desc,
                            );
                            if entry != 0 {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                note_integer_int_value_direct_site();
                            }
                        }
                        // JIT r20 w11: set when `callee_compiler` declines so late-bind cell arm may take site.
                        let mut late_static_cell_candidate = false;
                        if direct_target.is_none()
                            && !closes_cycle
                            && !(is_special
                                && ir::is_jdk_throwable_init_candidate(direct_class, &mn, &desc))
                            && !verdicts.direct_call_requires_dispatch_of(
                                cycle_class,
                                &mn,
                                &desc,
                                direct_class_id,
                            )
                        {
                            // JIT round 21 lane indylate2: an indy-planned
                            // site's callee is asked in the plan's class
                            // context (the target class), not the caller's.
                            let indy_context = ir_indy_direct
                                .class_context_at(pc)
                                .filter(|_| pc < code_len)
                                .zip(req.indy_context_callee_compiler);
                            if let Some(compiler) = callee_compiler.as_ref() {
                                let bound = match indy_context {
                                    Some((context, by_context)) => {
                                        by_context(context, direct_class, &mn, &desc)
                                    }
                                    None => compiler(direct_class, &mn, &desc),
                                };
                                if let Some((entry, callee_needs_ctx)) = bound {
                                    // Re-check: compiling the callee may have
                                    // discovered a cycle through this edge.
                                    if entry != 0
                                        && !verdicts.direct_call_requires_dispatch_of(
                                            cycle_class,
                                            &mn,
                                            &desc,
                                            direct_class_id,
                                        )
                                    {
                                        direct_target = Some((entry, callee_needs_ctx));
                                        DIRECT_CALLEE_BIND_HITS
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    } else {
                                        verdicts.mark_current_compile_method_recursive_cycle();
                                        DIRECT_CALLEE_BIND_MISSES
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                } else {
                                    // The one that matters: the compiler had
                                    // nothing to give, so this site is bound
                                    // to the dispatch helper for the life of
                                    // this body. See `DIRECT_CALLEE_BIND_HITS`.
                                    DIRECT_CALLEE_BIND_MISSES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    late_static_cell_candidate = true;
                                }
                            }
                        }
                        // Round 9 wave 8b (irl8 request 2): the statically
                        // bound CLOSING edge of a compile-time cycle gets a
                        // guarded call through a cell instead of dispatch for
                        // the life of this body -- the IR twin of
                        // `build_single_pass_tables`' plan, same gates. The
                        // site keeps its dispatch row (no `direct_target`);
                        // `ir_lower::emit_cycle_edge_cell_call` serves it. Not
                        // for an inherited-name edge: the cell is keyed by the
                        // site's class id, and the ancestor publishes under its
                        // declaring class, so it would never fill.
                        //
                        // JIT round 20 wave 11 (lane pLate, W15L1-P1 /
                        // W18L4-3): the same cell for EVERY unbound
                        // `invokestatic` whose callee `callee_compiler` could
                        // not give yet, not only a closing edge. See
                        // [`late_static_cell_site`].
                        let late_static_cell =
                            late_static_cell_site(closes_cycle, late_static_cell_candidate);
                        // JIT r20 wave 11 review (lane rev11): a late cell
                        // bypasses `jit_invoke_dispatch` once filled, which is
                        // where the GPU offload hook lives; the single-pass
                        // twin and the direct bind below both keep a kernel on
                        // the helper, so this arm does too.
                        if direct_target.is_none()
                            && (closes_cycle
                                || (late_static_cell
                                    && !crate::offload_hook::keeps_dispatch_helper(
                                        direct_class,
                                        &mn,
                                        &desc,
                                    )))
                            && cycle_declaring.is_none()
                            && is_static
                            && opcode == 0xb8
                            && jit_cycle_edge_cell_enabled()
                            && jit_cycle_direct_bind_enabled()
                            && direct_jit_callee_calls_enabled()
                            && !verdicts.direct_call_requires_dispatch_of(
                                cycle_class,
                                &mn,
                                &desc,
                                direct_class_id,
                            )
                            && helpers.self_call_stack_guard != 0
                            && helpers.invoke_dispatch != 0
                            && req.cycle_edge_admission.is_some_and(|admit| {
                                admit(cached.declaring_class_id.as_u32(), direct_class, &mn, &desc)
                            })
                        {
                            if let Some(target_class_id) =
                                cp_invoke_class_id_resolver.and_then(|r| r(cp_idx))
                            {
                                let cell = JitCycleEdgeCell::new_any_abi(
                                    direct_class,
                                    &mn,
                                    &desc,
                                    cratonvm_types::ClassId::new(target_class_id),
                                );
                                ir_cycle_cells.insert(pc, Arc::as_ptr(&cell) as usize);
                                ir_owned_cycle_cells.push(cell);
                                if crate::x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] ir {} {}.{}{} @pc={pc} -> {direct_class}.{mn}{desc} class_id={target_class_id}",
                                        if closes_cycle { "cycle-edge-cell" } else { "late-static-cell" },
                                        cached.class_name, cached.method_name, cached.method_descriptor
                                    );
                                }
                            }
                        }
                        // Round 11 wave 10 (lane `synccall`, page
                        // `r11w9-sync-caller-held-monitor-direct-call-patch`):
                        // a `static synchronized` callee, which
                        // `callee_compiler` refuses because its body takes no
                        // monitor, is CALLed directly with THIS method holding
                        // the class monitor around the CALL. The builder emits
                        // `ConstClass m; MonitorEnter m; Call; MonitorExit m`
                        // and the lowerer releases `m` on the call's
                        // exceptional edge. The VM answers only for a callee
                        // body that can neither trap nor call, so nothing can
                        // stop it while the caller holds a monitor no frame
                        // state names. `invokestatic` only, the merged sentinel
                        // route, and every helper the pair lowers to wired.
                        // Since round 11 wave 12 (lane `calls`) every result
                        // kind: see `compile_request::sync_direct_result_admitted`.
                        if direct_target.is_none()
                            && !closes_cycle
                            && is_static
                            && opcode == 0xb8
                            && crate::compile_request::sync_direct_result_admitted(
                                ret,
                                helpers.dispatch_threw,
                            )
                            && crate::compile_request::sync_direct_enabled()
                            && crate::x64::merged_call_sentinel_enabled()
                            && helpers.ldc_class_cp != 0
                            && helpers.monitor_enter != 0
                            && helpers.monitor_exit != 0
                            && !verdicts.direct_call_requires_dispatch_of(
                                cycle_class,
                                &mn,
                                &desc,
                                direct_class_id,
                            )
                            && !crate::offload_hook::keeps_dispatch_helper(direct_class, &mn, &desc)
                            && !crate::offload_hook::is_reflect_ctor_dispatch(
                                direct_class,
                                &mn,
                                &desc,
                            )
                        {
                            if let Some(t) =
                                req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx))
                            {
                                if t.entry != 0 && !t.instance {
                                    ir_direct_calls.insert(
                                        crate::compile_request::sync_direct_row_key(pc),
                                        (t.entry, t.needs_ctx),
                                    );
                                    // Keeps the callee mapped, and puts this
                                    // method in its invalidation closure.
                                    ir_direct_callee_entries
                                        .push((t.entry, jit_entry_artifact_id(t.entry)));
                                    ir_sync_direct_sites.insert(
                                        pc,
                                        (
                                            cached.declaring_class_id.as_u32(),
                                            t.class_cp_idx,
                                            t.mirror_slot,
                                        ),
                                    );
                                    if crate::x64::dbg_jitc_enabled() {
                                        eprintln!(
                                            "[cratonvm-jitc] ir-sync-direct-call {direct_class}.{mn}{desc} @pc={pc} entry={:#x} mirror_slot={:#x}",
                                            t.entry, t.mirror_slot
                                        );
                                    }
                                }
                            }
                        }
                        // Round 11 wave 11 (lane `calls`, page
                        // `r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee`):
                        // the INSTANCE twin of the arm above. A statically bound
                        // call (`invokespecial`, or an `invokevirtual` the VM
                        // pinned to a private/final target) of a `synchronized`
                        // instance callee is CALLed directly with THIS method
                        // holding the RECEIVER's monitor around the CALL. The
                        // builder emits `Guard(recv != null); MonitorEnter recv;
                        // Call; MonitorExit recv`: the guard deopts to the invoke
                        // BEFORE any monitor action, so a null receiver raises the
                        // interpreter's own NPE (JVMS 6.5) exactly where it would
                        // have, and the lowerer releases `recv` on the call's
                        // exceptional edge. The same callee predicate as the static
                        // arm (the VM answers only for a trap-free, call-free
                        // body), the same result kinds and helpers. Soundness of the
                        // TARGET: the VM answers for a method the Methodref's own
                        // class declares, so the site's selected owner must BE that
                        // class, and an `invokevirtual` binds only a method no
                        // receiver can override (private, final, or a final class).
                        // `<init>` is never synchronized; it is excluded anyway.
                        if direct_target.is_none()
                            && !closes_cycle
                            && is_special
                            && !is_static
                            && matches!(opcode, 0xb6 | 0xb7)
                            && mn != "<init>"
                            // The VM resolved the selected owner (never the
                            // constant-pool fallback), and it IS the Methodref
                            // class.
                            && special_owner.is_some()
                            && direct_class == cn.as_str()
                            && crate::compile_request::sync_direct_result_admitted(
                                ret,
                                helpers.dispatch_threw,
                            )
                            && crate::compile_request::sync_direct_enabled()
                            && crate::x64::merged_call_sentinel_enabled()
                            && helpers.monitor_enter != 0
                            && helpers.monitor_exit != 0
                            && !verdicts.direct_call_requires_dispatch_of(
                                cycle_class,
                                &mn,
                                &desc,
                                direct_class_id,
                            )
                        {
                            let mut self_locking_answer = false;
                            if let Some(t) =
                                req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx))
                            {
                                self_locking_answer = t.entry == 0 && t.instance;
                                if t.entry != 0 && t.instance && (opcode == 0xb7 || t.unoverridable)
                                {
                                    ir_direct_calls.insert(
                                        crate::compile_request::sync_direct_row_key(pc),
                                        (t.entry, t.needs_ctx),
                                    );
                                    // Keeps the callee mapped, and puts this
                                    // method in its invalidation closure.
                                    ir_direct_callee_entries
                                        .push((t.entry, jit_entry_artifact_id(t.entry)));
                                    // No class mirror: the builder takes the
                                    // receiver-monitor shape from the opcode.
                                    ir_sync_direct_sites.insert(pc, (0, 0, 0));
                                    if crate::x64::dbg_jitc_enabled() {
                                        eprintln!(
                                            "[cratonvm-jitc] ir-sync-direct-call {direct_class}.{mn}{desc} @pc={pc} entry={:#x} receiver-monitor opcode={opcode:#04x}",
                                            t.entry
                                        );
                                    }
                                }
                            }
                            // Interpreter round i2 wave 59, lane L2 (the
                            // i58-L2 proposal, "a hot synchronized callee that
                            // calls gets a direct call"): the lookup above may
                            // have just compiled and PUBLISHED the callee's
                            // body (`sync_direct_compile_unpublished_callee`)
                            // and refused it as a caller-held target because
                            // it is a SELF-LOCKING body -- which is exactly the
                            // body `callee_compiler` binds as a plain direct
                            // CALL (round 13 wave 6), but `callee_compiler` ran
                            // first, found nothing published, and refused. Ask
                            // it once more -- only when the VM answered that
                            // way (`entry == 0` on an instance target: the
                            // body it found is self-locking). Measured:
                            // `StringBuffer.append(C)`, `length()`,
                            // `setLength(I)` from an optimizing loop went
                            // through blind dispatch on every iteration of the
                            // body's first compile.
                            if direct_target.is_none()
                                && self_locking_answer
                                && !ir_sync_direct_sites.contains_key(&pc)
                                && crate::compile_request::self_locking_direct_bind_retry_enabled()
                                && !verdicts.direct_call_requires_dispatch_of(
                                    cycle_class,
                                    &mn,
                                    &desc,
                                    direct_class_id,
                                )
                            {
                                if let Some(compiler) = callee_compiler.as_ref() {
                                    if let Some((entry, callee_needs_ctx)) =
                                        compiler(direct_class, &mn, &desc).filter(|&(entry, _)| {
                                            entry != 0
                                                && !verdicts.direct_call_requires_dispatch_of(
                                                    cycle_class,
                                                    &mn,
                                                    &desc,
                                                    direct_class_id,
                                                )
                                        })
                                    {
                                        direct_target = Some((entry, callee_needs_ctx));
                                        DIRECT_CALLEE_BIND_HITS
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        if crate::x64::dbg_jitc_enabled() {
                                            eprintln!(
                                                "[cratonvm-jitc] ir-direct-call SELF-LOCKING-RETRY {direct_class}.{mn}{desc} @pc={pc} entry={entry:#x}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Round 11 wave 13 (lane `cha`, page
                    // `r11w12-calls-cha-sync-target-needs-a-class-guard-before-the-enter`):
                    // the CHA twin of the instance arm above, for a GENUINELY
                    // virtual `invokevirtual` of a `synchronized` method of a
                    // non-final class (`Hashtable`-style code). Bound only when
                    // class-hierarchy analysis says the Methodref's class C is
                    // the ONLY instantiable class loaded at or below itself
                    // (`unique_concrete_resolver` answers C for C's own
                    // subtree; a non-overriding subclass counts as a second
                    // provider), and the constant-pool class id agrees.
                    //
                    // SOUNDNESS is the guard, not the hierarchy: the builder
                    // emits `Guard(recv != null); Guard(ExactClassIs(recv, C));
                    // MonitorEnter recv; Call; MonitorExit recv`
                    // (`IrBuilder::enter_receiver_monitor`). The VM answers only
                    // for a method C itself declares, so a receiver of EXACTLY
                    // class C selects this body (JVMS 5.4.6) whatever is loaded
                    // later; any other receiver deopts to the invoke BEFORE the
                    // enter and the interpreter dispatches. That covers a frame
                    // already running this body when a subclass is defined (the
                    // `live` arm of `R11CallsSyncCha`), which no invalidation
                    // dependency can stop. No `inlined_methods` dependency is
                    // recorded: it would only save the first miss's deopt, and
                    // it refuses OSR entry to the whole body
                    // (`OSR_REFUSE_INLINED_SCOPE`). The miss evicts the body and
                    // the recompile's CHA answer is then "many".
                    //
                    // The site is re-marked `invoke_kind == 1`: the lowerer's
                    // caller-held route accepts a statically bound instance call
                    // only, and behind the class guard this one is. A class
                    // guard that already failed here (the de-spec registry has
                    // `(pc, UncommonTrap)`) is not planted again.
                    if direct_target.is_none()
                        && ir_direct
                        && is_virtual
                        && opcode == 0xb6
                        && mn != "<init>"
                        && crate::compile_request::sync_direct_result_admitted(
                            ret,
                            helpers.dispatch_threw,
                        )
                        && crate::compile_request::sync_direct_enabled()
                        && crate::x64::merged_call_sentinel_enabled()
                        && helpers.monitor_enter != 0
                        && helpers.monitor_exit != 0
                        && !verdicts.direct_call_requires_dispatch_of(
                            cn.as_str(),
                            &mn,
                            &desc,
                            || {
                                cp_invoke_class_id_resolver
                                    .and_then(|r| r(cp_idx))
                                    .map(cratonvm_types::ClassId::new)
                            },
                        )
                    {
                        let exact_class_id = crate::compile_request::cha_sync_exact_class(
                            req.unique_concrete_resolver
                                .and_then(|cha| cha(cn.as_str(), &mn, &desc)),
                            cp_invoke_class_id_resolver.and_then(|r| r(cp_idx)),
                        );
                        let despecialised = exact_class_id.is_some()
                            && req.despec.is_some_and(|registry| {
                                let key = format!(
                                    "{}.{}:{}",
                                    cached.class_name, cached.method_name, cached.method_descriptor
                                );
                                let bci = pc as u32;
                                registry.contains_speculation(
                                    &key,
                                    bci,
                                    crate::deopt::speculation_id(
                                        bci,
                                        crate::deopt::DeoptReason::UncommonTrap,
                                    ),
                                )
                            });
                        if let Some(exact_class_id) = exact_class_id.filter(|_| !despecialised) {
                            if let Some(t) =
                                req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx))
                            {
                                if t.entry != 0 && t.instance {
                                    ir_direct_calls.insert(
                                        crate::compile_request::sync_direct_row_key(pc),
                                        (t.entry, t.needs_ctx),
                                    );
                                    // Keeps the callee mapped, and puts this
                                    // method in its invalidation closure.
                                    ir_direct_callee_entries
                                        .push((t.entry, jit_entry_artifact_id(t.entry)));
                                    ir_sync_direct_sites.insert(pc, (exact_class_id, 0, 0));
                                    invoke_kind = 1;
                                    if crate::x64::dbg_jitc_enabled() {
                                        eprintln!(
                                            "[cratonvm-jitc] ir-sync-direct-call {cn}.{mn}{desc} @pc={pc} entry={:#x} receiver-monitor cha exact_class_id={exact_class_id}",
                                            t.entry
                                        );
                                    }
                                }
                            }
                        }
                    }
                    // `ByteBuffer.get(int)` / `put(int,byte)` at the
                    // OPTIMIZING door — the only bind in this function that
                    // is deliberately OUTSIDE the `is_static || is_special`
                    // gate above.
                    //
                    // Every other thin helper here is statically bound, so
                    // the gate is free. `java/nio/ByteBuffer` is abstract
                    // with several concrete subclasses, so its sites are
                    // genuinely virtual and can never be pinned by
                    // `invokevirtual_site_final_owner` the way the `final`
                    // wrapper classes are. The gate is therefore not a
                    // soundness condition for THIS family — it is a
                    // statement about which sites the other helpers can
                    // reach — and the two things that would make a virtual
                    // bind unsound are both already handled:
                    //
                    //  * the callee is not known from the site. It does not
                    //    have to be: the helper's own prologue
                    //    (`dbb_direct_elem_addr`) probes the receiver and
                    //    DECLINES to `jit_invoke_dispatch` for anything it
                    //    is not certain of, which is exactly the contract
                    //    the single-pass door already relies on (it binds
                    //    the same helper with `guard_class_id: 0`).
                    //  * the receiver must reach the helper in the right
                    //    register. `lower_data_node`'s direct-call arm
                    //    already computes `has_receiver` from
                    //    `invoke_kind` and admits `0 | 1 | 2`, so
                    //    `emit_direct_cross_call` marshals a virtual site's
                    //    receiver correctly with no change here.
                    //
                    // WHY IT HAD TO BE ADDED. Without it this family was in
                    // the `is_intrinsic_site` refusal list a few hundred
                    // lines up, which cost the WHOLE enclosing method the
                    // optimizing tier for the sake of a single-pass-only
                    // bind. Measured on `probes/NioBufferCostProbe.java`,
                    // heap `ByteBuffer.get(int)`: 574 ns with the refusal,
                    // 53 ns with the bind disabled entirely (method reaches
                    // the IR tier, site becomes an ordinary MIC dispatch) —
                    // but disabling the bind cost the DIRECT receiver
                    // 118 ns -> 393 ns, because a direct buffer's own
                    // `get` bottoms out in a registered native. Binding
                    // here keeps both: the method is optimized AND the
                    // direct fast path survives.
                    //
                    // AND ONLY WHERE IT CAN WIN. The helper serves DIRECT
                    // receivers; for a `HeapByteBuffer` it declines, and
                    // the site then pays the helper crossing, the decline,
                    // and the generic funnel — 574 ns against the 53 ns the
                    // ordinary MIC path costs, because the MIC reaches the
                    // COMPILED `HeapByteBuffer.get`, which is four
                    // bytecodes over a `byte[]`. So the bind is gated on the
                    // site's own profiled receiver being a class the funnel
                    // has actually served with the modelled layout. A site
                    // with no profile, no dominant receiver, or a heap
                    // receiver refuses the bind and keeps the MIC — which is
                    // the better path for it, not a fallback.
                    //
                    // Both facts are observed rather than assumed:
                    // `dominant_receiver` is the same profile the MIC seeds
                    // itself from a few lines below, and `class_is_served`
                    // is the same table the helper's own prologue consults
                    // before it agrees to answer.
                    // `Buffer.session()` at this door too, and outside the
                    // `is_static || is_special` gate for the same reason
                    // the `byteElement` bind below is: the site is a
                    // genuine `invokevirtual`, the lowerer already
                    // marshals a receiver for `invoke_kind` `0 | 1 | 2`,
                    // and the helper screens the receiver itself.
                    // Not while the raw `ScopedMemoryAccess` fold is on: the
                    // `session()` it feeds is `segment != null ? .. : null`, a
                    // body the splice inlines for ~2 ns, against the helper's
                    // 30-60 ns (`nosession`: ByteBuffer.getInt 160 -> 110 ns
                    // before the fold existed). The single-pass door keeps its bind.
                    if direct_target.is_none()
                        && ir_direct
                        && buffer_session_direct_enabled()
                        && !ir::ir_scoped_fold_enabled()
                        && (is_virtual || is_special)
                        && is_buffer_session_site(&mn, &desc)
                    {
                        let entry = direct_helpers.buffer_session;
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            BUFFER_SESSION_SITES_IR
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    // JIT round 17 wave 1, lane nio (NIO2-2): `Object.getClass()`
                    // at this door too, outside the static gate for the
                    // `session()` arm's reason: a genuine `invokevirtual`
                    // whose receiver the lowerer marshals and the helper
                    // screens. Opt-in, `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT`.
                    if direct_target.is_none()
                        && ir_direct
                        && is_virtual
                        // JIT r20 (lane qMisc): resolved to `Object.getClass`,
                        // not a class's own private / static `getClass()`.
                        && object_get_class_site_resolved(
                            &mn,
                            &desc,
                            cp_invoke_declaring_class_resolver,
                            cp_idx,
                        )
                        && object_get_class_direct_enabled()
                    {
                        let entry = direct_native_helper(
                            direct_helpers.object_get_class,
                            jdk_only,
                            direct_bind_admission,
                            "java/lang/Object",
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] object-getclass-direct bind (ir) {cn}.{mn} @pc={pc}"
                                );
                            }
                        }
                    }
                    if direct_target.is_none() && ir_direct && (is_virtual || is_special) {
                        let entry = object_notify_direct_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &mn,
                            &desc,
                            cp_invoke_declaring_class_resolver,
                            cp_idx,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                        }
                    }
                    // JIT round 22 wave 2, lane runtime (RUNTIME22-2): `Class.getName()`
                    // as one thin CALL into the `getName()` memos instead of
                    // the dispatch funnel. Opt-in. `java/lang/Class` is final
                    // and declares `getName`, so the site's triple is the call.
                    if direct_target.is_none()
                        && ir_direct
                        && (is_virtual || is_special)
                        && is_class_get_superclass_site(&cn, &mn, &desc)
                    {
                        let entry = direct_native_helper(
                            direct_helpers.class_get_superclass,
                            jdk_only,
                            direct_bind_admission,
                            "java/lang/Class",
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                        }
                    }
                    if direct_target.is_none()
                        && ir_direct
                        && (is_virtual || is_special)
                        && is_class_get_name_site(&cn, &mn, &desc)
                        && class_get_name_direct_enabled()
                    {
                        let entry = direct_native_helper(
                            direct_helpers.class_get_name,
                            jdk_only,
                            direct_bind_admission,
                            "java/lang/Class",
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] class-getname-direct bind (ir) {cn}.{mn} @pc={pc}"
                                );
                            }
                        }
                    }
                    // JIT round 23 wave 4, lane L5: `String.matches` /
                    // `replaceAll` / `replaceFirst` as one thin CALL into the
                    // registered native's exact path. See `string_regex_helper`.
                    if direct_target.is_none() && ir_direct && (is_virtual || is_special) {
                        if let Some((entry, _, _)) = string_regex_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &cn,
                            &mn,
                            &desc,
                        ) {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] string-regex-direct bind (ir) {cn}.{mn} @pc={pc}"
                                );
                            }
                        }
                    }
                    if direct_target.is_none()
                        && ir_direct
                        && scoped_memory_direct_enabled()
                        && (is_virtual || is_special)
                    {
                        if let Some(op) = is_scoped_memory_op(&cn, &mn, &desc) {
                            let entry = op.helper_entry(direct_helpers);
                            // Round 16 wave 17 (lane mega5, NIO2-1).
                            if entry != 0
                                && scoped_memory_bind_admitted(
                                    direct_bind_admission,
                                    &cn,
                                    &mn,
                                    &desc,
                                )
                            {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                SCOPED_MEMORY_SITES_IR
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    // Plain `Unsafe.get/put{Byte,Short,Int,Long}(Object,long)`
                    // at this door too. Genuinely `ACC_NATIVE` (no bytecode
                    // exists to shadow), so -- like `Buffer.session()`'s field
                    // read above -- no `direct_native_helper`/§1.4 check
                    // applies; see `UnsafeAccessorOp`'s own doc comment.
                    if direct_target.is_none() && ir_direct && (is_virtual || is_special) {
                        if let Some(op) =
                            is_unsafe_accessor_op(&cn, &mn, &desc).filter(|op| op.direct_enabled())
                        {
                            let entry = op.helper_entry(direct_helpers);
                            // `Unsafe` is capability-classified (`RawMemory`),
                            // unlike every other thin-helper op this door
                            // recognises, so the jit crate's own admission
                            // question -- the one VM-crate predicate it CAN
                            // ask -- has to be consulted, not just the
                            // `entry != 0` wiring check. See
                            // `docs/feature-designs/jit-ir-optimizations.md`.
                            if entry != 0
                                && direct_bind_admission
                                    .map_or(true, |admits| admits(&cn, &mn, &desc))
                            {
                                direct_target = Some((entry, true));
                                direct_target_is_thin_helper = true;
                                op.note_site(1);
                            }
                        }
                    }
                    // `byte[].clone()` thin direct call at this door too (strings
                    // round 2, `str17`; see `byte_array_clone_helper`).
                    if direct_target.is_none() && ir_direct && is_virtual {
                        let entry = byte_array_clone_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &cn,
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                        }
                    }
                    // `super.clone()` to `Object.clone`, thin direct call at
                    // this door too (see `object_clone_helper`).
                    if direct_target.is_none() && ir_direct && is_special {
                        let entry = object_clone_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &cn,
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                        }
                    }
                    // j27 wave 3 (lane CLONE): a virtual `clone()` -- an array's,
                    // or an instance's whose profiled class inherits
                    // `Object.clone` -- thin direct call at this door too (see
                    // `clone_thin_bind`).
                    if direct_target.is_none() && ir_direct && is_virtual {
                        let entry = clone_thin_bind::virtual_clone_helper_at(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            profile,
                            pc,
                            (cn.as_str(), mn.as_str(), desc.as_str()),
                            "ir",
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                        }
                    }
                    // `ThreadLocal.get()` thin direct call at this door too (SBR
                    // round 2, wave 8, lane `tlocal`; see the single-pass door's
                    // twin). `ThreadLocal` is not final, so the site arrives as an
                    // ordinary virtual call and the helper screens the receiver's
                    // class itself. `--real-jdk` only.
                    if direct_target.is_none()
                        && ir_direct
                        && thread_local_get_direct_enabled()
                        && is_virtual
                        && (cn == "java/lang/ThreadLocal"
                            || cn == "java/lang/InheritableThreadLocal")
                        && mn == "get"
                        && desc == "()Ljava/lang/Object;"
                        // j27 wave 1 (lane TYPECHK,
                        // `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE`): not where
                        // the profile's dominant class overrides `get()`.
                        && !thread_local_thin_bind::thread_local_thin_bind_refused(
                            direct_helpers,
                            profile,
                            pc,
                            false,
                            "ir",
                        )
                    {
                        let entry = direct_native_helper(
                            direct_helpers.thread_local_get,
                            jdk_only,
                            direct_bind_admission,
                            &cn,
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            note_thread_local_get_direct_site();
                        }
                    }
                    // `ThreadLocal.set(Object)`, the twin of the `get()` bind above.
                    if direct_target.is_none()
                        && ir_direct
                        && jdk_only
                        && thread_local_get_direct_enabled()
                        && is_virtual
                        && (cn == "java/lang/ThreadLocal"
                            || cn == "java/lang/InheritableThreadLocal")
                        && mn == "set"
                        && desc == "(Ljava/lang/Object;)V"
                        && !thread_local_thin_bind::thread_local_thin_bind_refused(
                            direct_helpers,
                            profile,
                            pc,
                            true,
                            "ir",
                        )
                    {
                        let entry = direct_native_helper(
                            direct_helpers.thread_local_set,
                            jdk_only,
                            direct_bind_admission,
                            &cn,
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            note_thread_local_get_direct_site();
                        }
                    }
                    if direct_target.is_none()
                        && ir_direct
                        && nio_byte_direct_helpers_enabled()
                        && is_virtual
                        && cn == "java/nio/ByteBuffer"
                        && ((mn == "put" && desc == "(IB)Ljava/nio/ByteBuffer;")
                            || (mn == "get" && desc == "(I)B"))
                        && !nio_byte_element_bind_refused(
                            direct_helpers,
                            profile,
                            pc,
                            mn == "put",
                            "ir",
                        )
                    {
                        let is_put = mn == "put";
                        // `direct_native_helper_for_impl`, not
                        // `direct_native_helper`: the registered row whose
                        // `NativeKind` decides the Real-JDK policy question
                        // lives on the IMPLEMENTING class, not on the
                        // abstract `java/nio/ByteBuffer` the site names.
                        // Same argument, and the same call, as the
                        // single-pass door.
                        let entry = direct_native_helper_for_impl(
                            if is_put {
                                direct_helpers.nio_bytebuffer_put_byte
                            } else {
                                direct_helpers.nio_bytebuffer_get_byte
                            },
                            jdk_only,
                            direct_bind_admission,
                            cn.as_str(),
                            "java/nio/DirectByteBuffer",
                            &mn,
                            &desc,
                        );
                        if entry != 0 {
                            direct_target = Some((entry, true));
                            direct_target_is_thin_helper = true;
                            NIO_BYTE_ELEMENT_SITES_IR
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    if direct_target.is_none()
                        && !is_static
                        && cn.as_str() == "java/lang/reflect/Constructor"
                        && mn == "newInstance"
                        && desc == "([Ljava/lang/Object;)Ljava/lang/Object;"
                        && direct_helpers.ctor_new_instance != 0
                    {
                        direct_target = Some((direct_helpers.ctor_new_instance, true));
                        direct_target_is_thin_helper = true;
                    }
                    // A GPU kernel keeps its dispatch helper.
                    //
                    // A raw `CALL` to the callee's entry is the one door in
                    // this backend that bypasses `jit_invoke_dispatch` for a
                    // statically-bound site, and that helper is where the
                    // offload hook now lives. Binding this site directly
                    // would compile the caller and silently end offload --
                    // which is exactly what `offload_jit_gate` used to
                    // refuse to compile the whole method to prevent.
                    //
                    // Unarmed (no `--gpu`, or nothing registered) this is
                    // one relaxed bool. See `crate::offload_hook`.
                    // `keeps_dispatch_helper`, not `is_kernel`: a registry
                    // miss can mean "could not have known yet", and this
                    // decision is one-way. See its AUDIT 2026-09-07 note.
                    let site_is_gpu_kernel = (is_static
                        && crate::offload_hook::keeps_dispatch_helper(
                            cn.as_str(),
                            mn.as_str(),
                            desc.as_str(),
                        ))
                        || crate::offload_hook::is_reflect_ctor_dispatch(
                            cn.as_str(),
                            mn.as_str(),
                            desc.as_str(),
                        );
                    if let Some((entry, callee_needs_ctx)) =
                        direct_target.filter(|_| !site_is_gpu_kernel && !instance_self_direct)
                    {
                        ir_direct_calls.insert(pc, (entry, callee_needs_ctx));
                        if !direct_target_is_thin_helper {
                            ir_direct_callee_entries.push((entry, jit_entry_artifact_id(entry)));
                        }
                    } else if crate::x64::dbg_jitc_enabled()
                        && !ir_sync_direct_sites.contains_key(&pc)
                    {
                        // Round 11 wave 11 (lane `calls`): a kind-4 site is not a
                        // miss. It never asks for a direct target -- `ir_lower`'s
                        // `emit_self_recursive_call` CALLs this method's own entry
                        // -- and reporting it as `MISSED` sent the `CratonBench
                        // fib` investigation looking for a bind that was there.
                        if is_self_recursive_direct || instance_self_direct {
                            eprintln!(
                                "[cratonvm-jitc] ir-self-call DIRECT {cn}.{mn}{desc} @pc={pc}"
                            );
                        } else {
                            eprintln!(
                                "[cratonvm-jitc] ir-direct-call MISSED {cn}.{mn}{desc} @pc={pc} ir_direct={ir_direct} static={is_static} special={is_special}"
                            );
                        }
                    }
                    // IR inline caches (jit-inlining-and-ir-calls). A
                    // virtual / interface site is NOT statically bound, so
                    // it cannot take the direct path above — instead give it
                    // the same eagerly-allocated MIC + PIC pair the
                    // single-pass backend gets (HIGH-7 strategy: allocate at
                    // first compile regardless of miss history; the slots
                    // start empty, cold sites fall straight through to the
                    // helper, and the helper populates them so later
                    // invocations hit inline with no recompile).
                    //
                    // Preconditions mirror the lowerer's:
                    //  * the miss-path helper must be wired — without
                    //    `jit_invoke_virtual_mic` there is nothing to
                    //    populate the caches, so the guards would never hit;
                    //  * the receiver plus args plus the hidden context
                    //    pointer must fit the entry ABI register file plus
                    //    at most `ir_lower::ir_ic_max_stack_words()` outgoing stack words
                    //    (round 11 wave 13, lane `icwide`: the hit path
                    //    marshals the overflow onto the stack, which is what
                    //    gives `HashMap.putVal`'s `newNode` a cache on
                    //    Win64). `ir_lower::ir_ic_admits_arity` is the ONE
                    //    predicate, asked here and by the lowerer. A wider
                    //    site keeps helper dispatch.
                    //
                    // Deliberately NO extra recursion-cycle gate here,
                    // unlike the direct path above. A direct call bakes a
                    // callee entry THIS compile resolved, so it needs
                    // `note_recursive_compile_cycle` to avoid binding an
                    // edge whose only stack protection is the dispatch
                    // helper's depth guard. An inline cache bakes no callee
                    // at all: every entry it ever calls was installed by
                    // `jit_invoke_virtual_mic` itself, under exactly the
                    // policy that helper already applies for the
                    // single-pass MIC/PIC caches. The IR cascade is
                    // therefore equivalent to the single-pass one by
                    // construction, and adds no new call edge shape.
                    //
                    // nio21 wave 15, lane L1 (`CRATONVM_JIT_IR_BOUND_SITE_IC`):
                    // a statically bound INSTANCE site (`invoke_kind == 1`) the
                    // direct bind above left unbound gets the same pair, see
                    // [`ir_bound_site_ic_planned`]. Its slots are filled only by
                    // the VM's bound-site arm of `jit_invoke_virtual_mic`, never
                    // by virtual selection, and are never profile-seeded.
                    let bound_site_ic = ir_bound_site_ic_planned(
                        is_special && !is_static && invoke_kind == 1,
                        mn.as_str(),
                        ir_direct_calls.contains_key(&pc)
                            || ir_sync_direct_sites.contains_key(&pc)
                            || ir_cycle_cells.contains_key(&pc)
                            // JIT r22 (lane inline): `Object.<init>` is elided,
                            // never a call worth a cache.
                            || (mn == "<init>" && cn == "java/lang/Object"),
                    );
                    if (is_virtual || is_interface || bound_site_ic)
                        && helpers.invoke_virtual_mic != 0
                        && crate::ir_lower::ir_ic_admits_arity(num_args)
                    {
                        if bound_site_ic && x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-bound-site-ic {cn}.{mn}{desc} @pc={pc} in {}.{}{} \
                                 (statically bound, no direct entry at compile time)",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        let mut mic = Box::new(JitMICSlot::new_at(pc));
                        // nio21 wave 16, lane L4: a starved in-loop virtual
                        // site of a method-entry body records what its inline
                        // cache serves into the receiver profile and asks for
                        // one recompile (`IcProfileSeedSite`).
                        // JIT round 21 wave 20 (lane mhfilter): not a filter
                        // species' call (its PRE row's pc is a `getstatic`).
                        let own_virtual_site = (is_virtual || is_interface)
                            && !bound_site_ic
                            && pc < code_len
                            && !ir_mh_constant.is_filter_pc(pc)
                            && !ir_mh_constant.is_convert_two_call_pc(pc);
                        if own_virtual_site && ir_ic_seed_method_entry {
                            let observations = profile
                                .and_then(|p| p.receiver_summary(pc))
                                .map_or(0, |s| s.observations);
                            let in_loop = observations < MIN_GUARDED_RECEIVER_EVIDENCE
                                && (ir_ic_seed_order
                                    .get_or_init(|| {
                                        ir_splice_order::profile_plan_order(
                                            code,
                                            code_len,
                                            &ir_invoke_ops,
                                            profile,
                                        )
                                    })
                                    .class_of(pc)
                                    == Some(ir_splice_order::SiteClass::InLoop)
                                    // t1 wave 7 (lane J6,
                                    // `CRATONVM_JIT_IR_FFM_SEED_CALL_TEXTUAL_LOOP`):
                                    // an FFM seed-call site's trap proved it
                                    // runs; a stale branch profile calling it
                                    // cold left the seed unarmed for good.
                                    || ffm_element_inline::ffm_seed_call_site_arms_in_loop(
                                        ir_ffm_seed_call_sites.contains(&pc),
                                        code,
                                        code_len,
                                        pc,
                                    ));
                            if ir_ic_profile_seed_planned(
                                own_virtual_site,
                                ir_ic_seed_method_entry,
                                in_loop,
                                observations,
                            ) {
                                mic.arm_profile_seed(IcProfileSeedSite::new(
                                    cached.declaring_class_id.as_u32(),
                                    std::sync::Arc::clone(&cached.class_name),
                                    std::sync::Arc::clone(&cached.method_name),
                                    std::sync::Arc::clone(&cached.method_descriptor),
                                    pc,
                                ));
                                if x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] ir-ic-profile-seed ARMED {cn}.{mn}{desc} @pc={pc} \
                                         in {}.{}{} (receiver observations {observations})",
                                        cached.class_name, cached.method_name, cached.method_descriptor,
                                    );
                                }
                            }
                        }
                        // Seed from the receiver-type profile exactly as the
                        // single-pass planner does: a site with a dominant
                        // receiver lands in the MIC (and, via
                        // `seed_from_mic`, PIC slot 0) with its class id
                        // only — `entry_ptr` stays 0, so the first dispatch
                        // still rings the helper, which installs the target;
                        // thereafter the guard hits.
                        if let Some(prof) = profile.filter(|_| !bound_site_ic) {
                            if let Some(receiver_counts) = prof.receivers.get(&pc) {
                                if let Some(dom) = profile::dominant_receiver(receiver_counts, 80) {
                                    mic.prepopulate(dom);
                                }
                            }
                        }
                        let pic = Box::new(JitPICSlot::new_at(pc));
                        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`):
                        // this slot's code compares the MIC before the inline
                        // ways (`ir_lower::emit_inline_cache_call`), so the
                        // MIC's class takes no way: five classes inline. Before
                        // the two seeds below, which read the mark. Not for a
                        // bound site, whose ways are not keyed by virtual
                        // selection (t1 w7 NBD).
                        if !bound_site_ic && inline_cache_pic::ic_mic_plus_four_enabled() {
                            pic.note_mic_read_first();
                        }
                        // `mic` was just built by `JitMICSlot::new()`, so its
                        // `cached_entry_word` is 0 and `jit_entry_publishable`
                        // short-circuits before it reads the policy at all —
                        // `prepopulate` above sets only the class id. `false`
                        // is therefore not a policy claim, it is unreachable.
                        pic.seed_from_mic(&mic, false);
                        // Round 13 wave 11 (lane mega9, M13-9): a site the
                        // profile already calls megamorphic starts on the
                        // cell-first path (`CRATONVM_JIT_IC_PROFILE_MEGA_SEED`).
                        // Never a bound site: its target does not depend on the
                        // receiver, and the shared megamorphic table is keyed by
                        // VIRTUAL selection.
                        if let Some(receiver_counts) = profile
                            .filter(|_| !bound_site_ic)
                            .and_then(|p| p.receivers.get(&pc))
                        {
                            pic.seed_megamorphic_from_profile(receiver_counts);
                        }
                        // Round 12 wave 6 (lane mega5, W5-6), applied in wave 7
                        // (lane mega6): every slot here reaches the helper only
                        // through `emit_inline_cache_call`, which probes its
                        // inline ways first.
                        if crate::direct_jit_callee_calls_enabled() {
                            pic.note_inline_ways_probed();
                        }
                        let mic_addr = &*mic as *const JitMICSlot as usize;
                        let pic_addr = &*pic as *const JitPICSlot as usize;
                        ir_mic_boxes.push(mic);
                        ir_pic_boxes.push(pic);
                        ir_ic_slots.insert(pc, (mic_addr, pic_addr));
                    }
                    // A redirected super call bakes its selection start and
                    // that class's id, as the single-pass tier does. A private
                    // or `final` owner keeps the constant-pool class: the
                    // helper's walk up from it finds the same declaration.
                    let (cn, owner_class_id) = match special_redirect {
                        Some((owner, owner_id)) if invoke_kind == 1 || instance_self_direct => {
                            (owner, owner_id)
                        }
                        _ => (cn, 0),
                    };
                    // JIT r20 (lane qMisc, correctness): a `getClass()`-named
                    // site NOT resolved to `Object.getClass` takes no inline
                    // prefix (`ir::Graph::getclass_prefix_refused_infos`).
                    let getclass_not_object = invoke_kind != 2
                        && is_object_get_class_site(&mn, &desc)
                        && !object_get_class_site_resolved(
                            &mn,
                            &desc,
                            cp_invoke_declaring_class_resolver,
                            cp_idx,
                        );
                    let class_ref = intern_jit_invoke_str(&cn);
                    let method_ref = intern_jit_invoke_str(&mn);
                    let desc_ref = intern_jit_invoke_str(&desc);
                    let info = Box::new(JitInvokeInfo {
                        class_name: class_ref,
                        method_name: method_ref,
                        descriptor: desc_ref,
                        num_jit_args: num_args,
                        return_type: ret,
                        invoke_kind,
                        declaring_class_id: ir_indy_direct
                            .class_context_at(pc)
                            .or_else(|| ir_mh_constant.class_context_at(pc))
                            .unwrap_or(cached.declaring_class_id.as_u32()),
                        owner_class_id,
                    });
                    let info_ptr = &*info as *const JitInvokeInfo as usize;
                    // JIT round 23 wave 3d (lane L1, proxy proposal 4e): the
                    // row the VM reads for a receiver-carrying direct self-call
                    // is a kind-1 (statically bound instance) twin of the IR's
                    // kind-4 row. Every VM reader of `invoke_kind` (the
                    // callee-deopt service, its re-run and argument decoders,
                    // `bail_to_interpreter`) knows kind 4 only as the
                    // receiver-less static self-call; kind 1 is exactly what
                    // this site is to them. The lowerer swaps it in for the
                    // service call (`Graph::self_call_service_infos`).
                    if instance_self_direct {
                        let twin = Box::new(JitInvokeInfo {
                            invoke_kind: 1,
                            ..*info
                        });
                        let twin_ptr = &*twin as *const JitInvokeInfo as usize;
                        ir_call_infos.push(twin);
                        ir_self_call_service_infos.push((info_ptr, twin_ptr));
                        ir_instance_self_call_pcs.push(pc);
                    }
                    ir_call_infos.push(info);
                    info_map.insert(pc, (info_ptr, num_args, ret));
                    // JIT round 21 wave 4, lane ic (IC21-2): the site's
                    // compile-time shared-table bind, before any code reads it.
                    if let Some(&(_, pic_addr)) = ir_ic_slots.get(&pc) {
                        // SAFETY: `info_ptr` addresses the box just pushed into
                        // `ir_call_infos` and `pic_addr` the one in
                        // `ir_pic_boxes`; both outlive the compile.
                        let (site_info, site_pic) = unsafe {
                            (
                                &*(info_ptr as *const JitInvokeInfo),
                                &*(pic_addr as *const JitPICSlot),
                            )
                        };
                        offer_mega_site_bind(req, site_info, site_pic);
                    }
                    if getclass_not_object {
                        builder.graph.getclass_prefix_refused_infos.insert(info_ptr);
                    }
                    // Read off the names this iteration already resolved,
                    // via the boxed `JitInvokeInfo` (the `cn`/`mn`/`desc`
                    // locals were moved into it above).
                    if is_special {
                        // SAFETY: `info_ptr` addresses the box just pushed
                        // into `ir_call_infos`, which outlives this loop.
                        let this_info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                        if this_info.class_name == "java/lang/Object"
                            && this_info.method_name == "<init>"
                            && this_info.descriptor == "()V"
                        {
                            object_init_pcs.insert(pc);
                        }
                    }
                }
                // `invokedynamic` sites the builder may trap at. Resolved
                // HERE rather than reusing the `indy_info` block far below,
                // because that block runs after the IR path has already
                // decided -- and because this needs only the stack effect,
                // not the bridge site or the per-argument tags the
                // single-pass emitter wants.
                if !scan.indy_ops.is_empty() && ir::ir_site_trap_enabled() {
                    let mut indy_sites: std::collections::HashMap<usize, (usize, u8)> =
                        std::collections::HashMap::new();
                    let resolved = cp_invokedynamic_descriptor_resolver
                        .map(|resolver| {
                            scan.indy_ops
                                .iter()
                                .all(|&(pc, cp_idx)| match resolver(cp_idx) {
                                    Some((descriptor, _bridge)) => {
                                        indy_sites.insert(
                                            pc,
                                            (
                                                count_param_slots(&descriptor),
                                                return_type(&descriptor),
                                            ),
                                        );
                                        true
                                    }
                                    None => false,
                                })
                        })
                        .unwrap_or(false);
                    // All or nothing: a partially resolved set would let
                    // the builder walk past an indy it has no shape for,
                    // and the arm would bail the method anyway -- later,
                    // and after wasting the build.
                    if resolved {
                        builder.set_indy_trap_sites(indy_sites);
                    } else if ir_stage_reporting() {
                        eprintln!(
                            "[ir] indy-trap {}.{}{}: descriptor resolver declined; the method keeps the single-pass backend",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                }
                // Hand over the arithmetic-lowered sites whenever the plan
                // SURVIVED, and independently of `info_map` being non-empty:
                // a method whose only invokes are `Math.max` sites has an
                // empty `info_map` and is exactly the case this is for.
                if all_emittable && !ir_scalar_intrinsic_sites.is_empty() {
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] scalar-intrinsics {}.{}{}: {} site(s) lowered as arithmetic",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            ir_scalar_intrinsic_sites.len(),
                        );
                    }
                    builder.set_scalar_intrinsics(std::mem::take(&mut ir_scalar_intrinsic_sites));
                }
                if all_emittable && !ir_pure_expansion_sites.is_empty() {
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] pure-expansions {}.{}{}: {:?}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            ir_pure_expansion_sites.values().collect::<Vec<_>>(),
                        );
                    }
                    builder.set_pure_expansions(std::mem::take(&mut ir_pure_expansion_sites));
                }
                if all_emittable && !ir_unbox_intrinsic_sites.is_empty() {
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] unbox-intrinsics {}.{}{}: {} site(s) lowered as a guarded field load",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            ir_unbox_intrinsic_sites.len(),
                        );
                    }
                    builder.set_unbox_intrinsics(std::mem::take(&mut ir_unbox_intrinsic_sites));
                }
                // JIT r23 wave 4 (lane L8): the FFM element sites to expand
                // inline. Cloned, not taken: the splice planner below keeps
                // them calls.
                // JIT r23 wave 11 (lane L4): the never-reached element sites.
                // JIT r23 wave 12 (lane L7): the generic never-reached sites.
                if all_emittable && !ir_unreached_call_sites.is_empty() {
                    builder.set_unreached_call_sites(ir_unreached_call_sites.clone());
                }
                if all_emittable && !ir_ffm_unreached_sites.is_empty() {
                    builder.set_ffm_unreached_sites(ir_ffm_unreached_sites.clone());
                }
                if all_emittable && !ir_ffm_inline_sites.is_empty() {
                    builder.set_ffm_inline_sites(ir_ffm_inline_sites.clone());
                    builder.set_ffm_bimorphic_seconds(ir_ffm_bimorphic_seconds.clone());
                    // JIT r23 wave 9 (lane L4): the `HeapSession` class the
                    // loop-entry session proof tests (`ffm_field_hoist`).
                    builder.set_ffm_heap_session_class(ir_lower::scoped_heap_session_class_id(
                        direct_helpers,
                    ));
                }
                // nio21 wave 1 L3: a literal-null-base `ScopedMemoryAccess`
                // site (a direct buffer) folds when the lowering can resolve an
                // arena handle inline -- the same predicate the lowering asks.
                builder.set_scoped_offheap_fold(
                    ir_lower::scoped_arena_jit_table(direct_helpers).is_some(),
                );
                if all_emittable && !info_map.is_empty() {
                    if false {
                        eprintln!(
                            "[cratonvm-ircall] {}.{}{}: emitting {} invoke(static/special/virtual/interface) Op::Call(s), {} bound as DIRECT calls",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            info_map.len(),
                            ir_direct_calls.len(),
                        );
                    }
                    builder.set_invoke_info(info_map);
                    // The caller-held monitor pair of every synchronized direct
                    // CALL planned above (round 11 wave 10, lane `synccall`).
                    builder.set_sync_direct_calls(std::mem::take(&mut ir_sync_direct_sites));
                    // The terminal `super()` of every constructor chain.
                    // Collected from the SAME resolved names this loop
                    // already built `invoke_info` from, so it cannot
                    // disagree with what would otherwise be dispatched.
                    // Eliding it is the single-pass backend's long-standing
                    // rule (`x64::bytecode_walk`'s 0xb7 arm); the IR tier
                    // had no equivalent, so a compiled constructor paid a
                    // `jit_invoke_dispatch` round trip per allocation for a
                    // method whose body is `return`.
                    builder.set_object_init_pcs(object_init_pcs);
                } else {
                    // A non-emittable invoke is present → leave `invoke_info`
                    // unset (the builder bails on every invoke → single-pass)
                    // and drop the now-unreferenced boxes/strings.
                    ir_call_infos.clear();
                    ir_self_call_service_infos.clear();
                    ir_instance_self_call_pcs.clear();
                    ir_call_strings.clear();
                    ir_direct_calls.clear();
                    ir_direct_callee_entries.clear();
                    ir_sync_direct_sites.clear();
                    ir_cycle_cells.clear();
                    ir_owned_cycle_cells.clear();
                    // The inline-cache plan's baked addresses point INTO
                    // these boxes, so plan and storage must be dropped
                    // together — never one without the other.
                    ir_ic_slots.clear();
                    ir_mic_boxes.clear();
                    ir_pic_boxes.clear();
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] invoke-plan {}.{}{}: NO invoke_info — {}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            nonemittable
                                .as_deref()
                                .unwrap_or("no emittable invoke site found"),
                        );
                    }
                }
            } else if ir_stage_reporting() {
                eprintln!(
                    "[ir] invoke-plan {}.{}{}: NO invoke_info — call_eligible=false \
                         (anewarray_ops={})",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    scan.anewarray_ops.len(),
                );
            }
        }
    }
    // Round 16 (lane indy, SY4-1; `CRATONVM_JIT_IR_INDY_BRIDGE`, default
    // OFF): the `invokedynamic` sites this tier CALLS through the VM's bridge
    // instead of trapping at.
    //
    // A site qualifies on the single-pass tier's own three terms
    // (`x64/op_invoke.rs` `indy_site_is_bridged`): a resolved bootstrap (a
    // non-zero bridge site from the resolver), the bridge helper wired, and a
    // non-`void` descriptor.
    //
    // OUTSIDE the invoke-plan block above, deliberately. That block runs only
    // for a method with an ordinary invoke, so a method whose only call is an
    // indy (`() -> "x"`, `return l + ":" + d;`) never received its stack
    // effects and bailed at the `0xba` arm (`IrBuilder::build refused at
    // ir.rs:<0xba arm>`, no other line). Here such a method gets its shapes
    // ONLY when every one of its sites is bridged, so this block never plants
    // a trap the default arm would not have planted: a method with an
    // unbridgeable site keeps exactly the shapes the block above gave it (or
    // none, and the same bail).
    // nio21 wave 11, lane L3 (W10L3-1): a spliced constant-word lambda site
    // lowers to an `Op::IndyBridge` whose cold path calls the bridge, so the
    // builder admits one only when it is wired.
    builder.set_spliced_indy_wired(direct_helpers.indy_bridge != 0);
    // JIT round 23 wave 2b, lane L1 (churn page J1;
    // `CRATONVM_JIT_IR_OSR_DOOR_TRAPS_IN_PLACE`, default ON): the optimizing OSR
    // door's build plants a site trap off every loop even after a side effect,
    // because the door resumes it in the live frame
    // (`IrBuilder::trap_resumable_here`).
    builder.set_osr_door_traps_in_place(
        req.osr_mode_exit_polls && ir::ir_osr_door_traps_in_place_enabled(),
    );
    // JIT round 23 wave 4 (proxy-define page;
    // `CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE`, default ON since wave 8): only a method
    // whose sinks resume an own-code trap from its frame (the conditions of
    // the VM's `sink_precise_resume_allowed`), and no lambda implementation
    // body (its helper sink can only replay from entry).
    // Wave 8 (lane W8-L1, `CRATONVM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE`): the
    // same admission serves an unresolved-field site trap; the builder asks
    // each cause's own switch.
    builder.set_typecheck_traps_resume_in_place(
        (ir::ir_unresolved_typecheck_in_place_enabled()
            || ir::ir_unresolved_field_in_place_enabled())
            && deopt_sink_resume_enabled()
            && !cached.is_synchronized
            && !bytecode_holds_monitor(code, code_len.min(code.len()))
            && !cached.method_name.starts_with("lambda$"),
    );
    // JIT round 22 (lane throws, churn-loop J1;
    // `CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL`, opt-in): the optimizing
    // OSR door's build gets the bridge plan below as a FALLBACK only -- used at
    // a site where the default arm would bail the whole build (its trap
    // refused by the replay guard, or no shape). `IrBuilder::
    // indy_default_trap_would_plant` decides per site; every site whose trap
    // the default arm plants keeps it. The shapes the default door gave are
    // read BEFORE this block replaces them. (A later lambda-word /
    // capture-new block may still give shapes; such a method's sites are
    // then bridged rather than trapped, which in an OSR body is the better
    // side of the same trade.)
    // JIT round 22 wave 2 (lane throws, THROWS22-2;
    // `CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL_LOOP_METHODS`, opt-in): a
    // method-entry compile takes the same refusal-only plan when the method
    // has a loop and every `invokedynamic` lies outside every loop
    // ([`indy_sites_all_outside_loops`]), so the bare bridge runs once per
    // call while the loop gets the optimizing body.
    let indy_bridge_refusal_only = !ir::ir_indy_bridge_enabled()
        && ((req.osr_mode_exit_polls && ir::ir_indy_bridge_on_trap_refusal_enabled())
            || (!req.osr_mode_exit_polls
                && !scan.indy_ops.is_empty()
                && ir::ir_indy_bridge_on_trap_refusal_loop_methods_enabled()
                && indy_sites_all_outside_loops(
                    code,
                    code_len,
                    &scan.indy_ops,
                    &cached.exception_table,
                )));
    if indy_bridge_refusal_only {
        let default_had_shapes = builder.has_indy_trap_sites();
        builder.set_indy_bridge_refusal_only(true, default_had_shapes);
    }
    if !scan.indy_ops.is_empty()
        && ir::ir_site_trap_enabled()
        && (ir::ir_indy_bridge_enabled() || indy_bridge_refusal_only)
        && direct_helpers.indy_bridge != 0
    {
        if let Some(resolver) = cp_invokedynamic_descriptor_resolver {
            let mut shapes: std::collections::HashMap<usize, (usize, u8)> =
                std::collections::HashMap::new();
            let mut bridged: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            let mut all_resolved = true;
            for &(pc, cp_idx) in &scan.indy_ops {
                let Some((descriptor, bridge_site)) = resolve_indy_site(req, resolver, cp_idx, pc)
                else {
                    all_resolved = false;
                    break;
                };
                let ret = return_type(&descriptor);
                shapes.insert(pc, (count_param_slots(&descriptor), ret));
                if bridge_site != 0 && ret != b'V' {
                    bridged.insert(pc, bridge_site);
                }
            }
            let every_site_bridged = all_resolved && bridged.len() == scan.indy_ops.len();
            if ir_stage_reporting() {
                eprintln!(
                    "[ir] indy-bridge {}.{}{}: {} of {} site(s) bridged{}",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    bridged.len(),
                    scan.indy_ops.len(),
                    if every_site_bridged {
                        ""
                    } else {
                        " -- the rest keep the default arm's trap or refusal"
                    },
                );
            }
            if every_site_bridged {
                builder.set_indy_trap_sites(shapes);
            }
            if all_resolved && !bridged.is_empty() {
                builder.set_indy_bridge_sites(bridged);
            }
        }
    }
    // JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3;
    // `CRATONVM_JIT_IR_INDY_LAMBDA_WORD`, opt-in): a method's OWN
    // non-capturing lambda site whose singleton is published is built as
    // `word != null ? word : bridge` (`IrBuilder::try_indy_lambda_word`),
    // independently of `CRATONVM_JIT_IR_INDY_BRIDGE`: its answer is already
    // known, and the hot path has no memory effect.
    if !scan.indy_ops.is_empty()
        && ir::ir_site_trap_enabled()
        && ir::ir_indy_lambda_word_enabled()
        && direct_helpers.indy_bridge != 0
    {
        if let (Some(resolver), Some(words)) = (
            cp_invokedynamic_descriptor_resolver,
            req.indy_constant_word_resolver,
        ) {
            let (rows, shapes) = ir_indy_lambda_word_plan(req, resolver, words, &scan.indy_ops);
            if let Some(shapes) = shapes {
                builder.set_indy_trap_sites(shapes);
            }
            if !rows.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-lambda-word {}.{}{}: {} of {} site(s) read their singleton",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        rows.len(),
                        scan.indy_ops.len(),
                    );
                }
                builder.set_indy_lambda_words(rows);
            }
        }
    }
    // JIT round 21 (lane lambdatrap, TLOCAL21-2;
    // `CRATONVM_JIT_IR_INDY_PUBLISHED_WORD_BRIDGE`, default ON): with the
    // lambda-word expansion off, the same rows (the method's own non-capturing
    // sites whose singleton is published) are built as the single-pass tier
    // builds them -- an `Op::IndyBridge`, whose constant-word probe answers the
    // singleton without a call -- never as the uncommon trap the `0xba` arm
    // plants otherwise, which fired on the first arrival of EVERY call
    // (`jr21-tlocal-lambda-word-optimizing-body-traps-20261014`). Shapes only
    // when every site of the method is such a site, as the plan above.
    if !scan.indy_ops.is_empty()
        && ir::ir_site_trap_enabled()
        && !ir::ir_indy_lambda_word_enabled()
        && ir::ir_indy_published_word_bridge_enabled()
        && direct_helpers.indy_bridge != 0
    {
        if let (Some(resolver), Some(words)) = (
            cp_invokedynamic_descriptor_resolver,
            req.indy_constant_word_resolver,
        ) {
            let (rows, shapes) = ir_indy_lambda_word_plan(req, resolver, words, &scan.indy_ops);
            if let Some(shapes) = shapes {
                builder.set_indy_trap_sites(shapes);
            }
            if !rows.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-published-word-bridge {}.{}{}: {} of {} site(s) bridged, \
                         not trapped",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        rows.len(),
                        scan.indy_ops.len(),
                    );
                }
                builder
                    .add_indy_bridge_sites(rows.into_iter().map(|(pc, (site, _slot))| (pc, site)));
            }
        }
    }
    // JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2;
    // `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`, default ON): a method's OWN linked
    // capturing lambda site is built as `Op::New` of its proxy class plus one
    // store per capture (`IrBuilder::try_indy_capture_new`), so escape analysis
    // may scalar-replace it. Independent of `CRATONVM_JIT_IR_INDY_BRIDGE`; a
    // method whose every site is such a site or a published-word site gets its
    // shapes here.
    // t1 wave 11, lane JLAM: the pcs it allocates inline, for the late-lambda
    // bridge block's coverage below.
    let mut indy_capture_new_pcs: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    if !scan.indy_ops.is_empty() && ir::ir_site_trap_enabled() && ir::ir_indy_capture_new_enabled()
    {
        if let (Some(resolver), Some(captures)) = (
            cp_invokedynamic_descriptor_resolver,
            req.indy_capture_alloc_resolver,
        ) {
            // The word rows exist only where a block above built them (the
            // lambda-word expansion, or JIT round 21's published-word bridge).
            let words = if (ir::ir_indy_lambda_word_enabled()
                || ir::ir_indy_published_word_bridge_enabled())
                && direct_helpers.indy_bridge != 0
            {
                req.indy_constant_word_resolver
            } else {
                None
            };
            let (rows, shapes) =
                ir_indy_capture_new_plan(req, resolver, captures, words, &scan.indy_ops);
            if let Some(shapes) = shapes {
                builder.set_indy_trap_sites(shapes);
            }
            if !rows.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-capture-new {}.{}{}: {} of {} site(s) allocate their proxy inline",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        rows.len(),
                        scan.indy_ops.len(),
                    );
                }
                indy_capture_new_pcs.extend(rows.keys().copied());
                builder.set_indy_capture_news(rows);
            }
        }
    }
    // t1 wave 11, lane JLAM (LAMBDATRAP21-4;
    // `CRATONVM_JIT_IR_INDY_LATE_LAMBDA_BRIDGE`, default ON since j27): a method's OWN
    // non-capturing lambda site nobody evaluated yet (first reached after
    // this compile) is built as the single-pass tier builds it -- an
    // `Op::IndyBridge`, whose first evaluation links the instruction and
    // publishes the singleton its later probes read -- instead of the trap
    // whose first firing bans the method's optimizing body. Lambda sites
    // only (the VM's `constant_lambda` answer), unlike the general
    // `CRATONVM_JIT_IR_INDY_BRIDGE`. Shapes only when every site of the method
    // is such a site, a published-word site or a capture-new site, so this
    // block never plants a trap the default arm would not have planted.
    if !scan.indy_ops.is_empty()
        && ir::ir_site_trap_enabled()
        && ir::ir_indy_late_lambda_bridge_enabled()
        && direct_helpers.indy_bridge != 0
    {
        if let (Some(resolver), Some(is_lambda)) = (
            cp_invokedynamic_descriptor_resolver,
            req.indy_constant_lambda_site_resolver,
        ) {
            let words = req.indy_constant_word_resolver;
            let published = |site: usize| words.is_some_and(|w| w(site) != 0);
            // The published-word rows exist only where a block above built
            // them.
            let published_covered = words.is_some()
                && (ir::ir_indy_lambda_word_enabled()
                    || ir::ir_indy_published_word_bridge_enabled());
            let site_of = |cp_idx: u16, pc: usize| resolve_indy_site(req, resolver, cp_idx, pc);
            let (rows, shapes) = ir_indy_late_lambda_bridge_plan(
                &site_of,
                is_lambda,
                &published,
                published_covered,
                &indy_capture_new_pcs,
                &scan.indy_ops,
            );
            if let Some(shapes) = shapes {
                builder.set_indy_trap_sites(shapes);
            }
            if !rows.is_empty() {
                if ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] indy-late-lambda-bridge {}.{}{}: {} of {} site(s) bridged before \
                         their first evaluation, not trapped",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        rows.len(),
                        scan.indy_ops.len(),
                    );
                }
                builder.add_indy_bridge_sites(rows);
            }
        }
    }
    // ── IR-tier inlining ────────────────────────────────────────────
    //
    // Resolve a body for each admitted call site and relocate it into a
    // COMBINED buffer that `build` walks: the compiling method's code
    // first, then one copy of each spliced body after it. The builder jumps
    // `pc` into a body at its `invoke` and back out at its return; JVM
    // branch offsets are relative, so a whole body moves without rewriting.
    //
    // This is the missing first half of deleting a short-lived wrapper. An
    // accessor that `areturn`s a fresh object shows escape analysis an
    // object that leaves its own method, and `scalar-replaced 0/N` is the
    // only answer per-method EA can give — at any tier, however good.
    // Inlining the chain into its consuming loop first is what lets the
    // object die where it is used. See `ir::IrInlineSite`.
    let mut ir_combined: Option<Vec<u8>> = None;
    // JIT r25 wave 9 (W9-3): what the chain cost rule kept in THIS compile,
    // printed with the compile's outcome (`chain-compile` line) under
    // `CRATONVM_DBG_JITC`. Local to the compile; no static.
    let mut ir_chain_census = r25_w7b_chain_splice::ChainCompileCensus::default();
    // `(start, end, invoke_bci)` per relocated region, for `ir_lower`'s
    // `resume_bci`. A top-level site's own body and every body nested inside
    // it are appended contiguously (`append_ir_inline_site` writes its own
    // code, then recurses), so one range covers the whole chain.
    let mut ir_spliced_ranges: Vec<(usize, usize, usize)> = Vec::new();
    // JIT round 19 wave 3 (lane splice3, SPLICE2-3): the caller's own call
    // bcis registered as guarded splices (`CompiledMethod::ir_guarded_splice_bcis`).
    let mut ir_guarded_splice_bcis: Vec<u32> = Vec::new();
    // JIT round 19 wave 4 (lane splice4, SPLICE3-1): the class each of those
    // tests (`CompiledMethod::ir_guarded_splice_classes`).
    let mut ir_guarded_splice_classes: Vec<(u32, u32)> = Vec::new();
    // See `CompiledMethod::spliced_bodies_side_effect_free`. Vacuously true
    // until a body is actually spliced.
    let mut ir_spliced_bodies_pure = true;
    // Combined-buffer pc → the spliced callees enclosing it. Built from the
    // same site table the builder gets, and for a different consumer: this
    // one is read at CALL RETURN sites by `ir_lower` so a stack trace can
    // name the callees this tier inlined. Empty when nothing is spliced.
    let mut ir_inline_frame_sites = ir::IrInlineFrameSites::default();
    // Round 13 wave 6 (lane chain2): the spliced bodies' class ids for the
    // VM's chain builder (`CompiledMethod::splice_scope_class_ids`). Empty
    // unless this compile records chain snapshots.
    let mut ir_splice_scope_class_ids: Vec<(String, u32)> = Vec::new();
    // Round 14 wave 3 (lane chain, R14DP-5): the spliced bodies' own bytecode
    // for the same builder (`CompiledMethod::splice_scope_sources`).
    let mut ir_splice_scope_sources: Vec<SpliceScopeSource> = Vec::new();
    // t1 ARGFIX item 2: the spliced bodies' bytecode for the keep set's
    // per-scope local liveness (`spliced_body_codes`). Filled only under its
    // switch.
    let mut ir_spliced_body_codes: Vec<ir_lower::SplicedBodyCode> = Vec::new();
    let mut ir_osr_new_local_classes: HashMap<usize, u32> = HashMap::new();
    let prune_despec_key = format!(
        "{}.{}:{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    );
    let prune_withdrawn = |pc: usize| {
        u32::try_from(pc).map_or(true, |b| {
            req.despec.is_some_and(|d| d.contains(&prune_despec_key, b))
        })
    };
    let ir_pruned_branches: std::collections::HashSet<usize> = if ir::ir_speculate_enabled() {
        profile
            .map(|prof| {
                prof.branches
                    .iter()
                    .filter(|(pc, c)| {
                        c.not_taken == 0
                            && c.taken >= ir::MIN_OBSERVATIONS_TO_PRUNE
                            && speculatable_forward_branch(code, code_len, **pc)
                            && !prune_withdrawn(**pc)
                            && !prof.branch_trapped(**pc)
                    })
                    .map(|(&pc, _)| pc)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };
    let ir_never_taken_set: Option<std::collections::HashSet<usize>> =
        if ir::ir_prune_never_taken_enabled() {
            Some(ir_never_taken_branches(
                profile,
                code,
                code_len,
                req.despec.map(|r| &**r),
                &prune_despec_key,
            ))
        } else {
            None
        };
    let ir_live_without_pruned = ir::reachable_pcs_without_pruned_branches(
        code,
        code_len,
        &ir_pruned_branches,
        ir_never_taken_set
            .as_ref()
            .unwrap_or(&std::collections::HashSet::new()),
    );
    if ir_inline_enabled() {
        if let (Some(ir_resolver), Some(invoke_resolver)) = (ir_inline_resolver, cp_invoke_resolver)
        {
            let mut combined: Vec<u8> = code[..code_len.min(code.len())].to_vec();
            let mut tables = ir::IrInlineTables::default();
            // Combined-buffer pcs of the spliced volatile `getfield`s
            // (round 9 wave 10, irl10). See `append_ir_inline_site`.
            let mut ir_spliced_volatile_pcs: Vec<usize> = Vec::new();
            // `CRATONVM_JIT_IR_INLINE_BUDGET_WIDE` (default ON): a compile with
            // an OSR request (the loop is the method) gets at least twice the
            // default spliced-byte budget, so the loop's catch arm is not left as
            // a row of opaque calls once the body of the loop has spent the flat
            // one. `CRATONVM_JIT_IR_INLINE_BUDGET` still sets the base.
            let mut budget = ir_inline_total_budget();
            if req.osr_requested_bci.is_some() && true {
                budget = budget.max(2 * IR_INLINE_MAX_TOTAL_BYTES);
            }
            let mut sites_planned = 0usize;
            // Bounded self-recursive inlining: built once per compile from this
            // method's identity and its own kind-4 self-call row (asked of the
            // builder BEFORE `apply_inline_tables`, while `invoke_info` holds
            // only the caller's rows). `None` keeps today's planner exactly.
            let ir_self_rec: Option<ir::IrSelfRecursion> = if ir::ir_recursive_inline_enabled() {
                builder.self_recursive_call_row(code_len).map(|row| {
                    ir::IrSelfRecursion::new_with_code_len(
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        cached.declaring_class_id.as_u32(),
                        row,
                        code_len,
                    )
                })
            } else {
                None
            };
            // Round 12 wave 4 (lane iropt3, W3-1): `(caller pc, class id)` of
            // every planned site resolved for an exact receiver. Merged only on
            // success, like the site's other rows.
            let mut ir_exact_sites: Vec<(usize, u32)> = Vec::new();
            // JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`):
            // caller pc -> the CHA watermark of an exact site planned from the
            // constant-pool answer (`ir::take_cha_watermark`).
            let mut ir_cha_watermarks: std::collections::HashMap<usize, u32> =
                std::collections::HashMap::new();
            // j27 wave 5 (lane PROXY, `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`):
            // caller pc -> the exact class set of an interface site planned
            // from the profile answer (`InlineSite::cha_class_set`).
            let mut ir_cha_class_sets: std::collections::HashMap<usize, Vec<u32>> =
                std::collections::HashMap::new();
            // nio21 wave 6, lane L4: the planned one-class profile-guarded
            // sites whose body is over the cold cap, admitted under the hot
            // tier (`ir_guarded_splice_site_cap`); the builder is told so
            // (`IrBuilder::add_guarded_hot_cap_site`). Merged only on success.
            let mut ir_guarded_hot_pcs: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // Round 13 wave 10 (lane iropt8, HS-2): the same for exact-receiver
            // bodies NESTED in a planned site (combined pcs). Registered as
            // exact sites only, never as guarded ones.
            let mut ir_exact_nested_sites: Vec<(usize, u32)> = Vec::new();
            // JIT r25 wave 7 (W7-B fix): the method keys of the chain links
            // the cost rule kept (`r25_w7b_chain_splice::strip_chain_tree`),
            // for their own hot-cap slots.
            let mut ir_chain_link_keys: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            // JIT r25 wave 9 (W9-3): a re-plan starts the census afresh.
            ir_chain_census = Default::default();
            // Round 14 wave 2 (lane guard, GS-1): `(caller pc, second class id)`
            // of every planned bimorphic site; its first class is the site's
            // `ir_exact_sites` row. Merged only on success.
            let mut ir_bimorphic_sites: Vec<(usize, u32)> = Vec::new();
            // Round 16 (lane mega): `(caller pc, third and later class ids)`
            // of every planned polymorphic site. Merged only on success.
            let mut ir_polymorphic_sites: Vec<(usize, Vec<u32>)> = Vec::new();
            // Round 12 wave 5 (lane iropt4): the replay-fence relaxation
            // switches, read once per compile (`ir::SpliceFenceRelax`).
            let ir_fence_flags = ir::SpliceFenceRelax::from_flags();
            // Round 12 wave 6 (lane iropt5, W5-1): the planner also models the
            // committed-STORE fence (`ir_splice_store_fence_refuses_site`).
            // Both switches read once per compile.
            let ir_plan_store_fence = true;
            let ir_committed_store_admitted = true;
            // Round 13 wave 3 (lane framestate): W5-3 stage 3, read once per
            // compile. Chain snapshots need this method's key, which an
            // `ACC_SYNCHRONIZED` method does not get (see below).
            // Round 14 wave 4 (lane resume2, CH3W-4): a synchronized method gets
            // chains only under the census switch (default OFF), at all three sites.
            // Round 15 wave 3 (lane sync2): never for a self-locking body
            // (`ir_self_lock_requested`), which keeps flat frames.
            let ir_chain_fences = ir::ir_splice_chain_fences_enabled() && ir_sync_chains_ok;
            // Round 16 wave 16 (lane chains2): the switch's engagement line, one
            // per `ACC_SYNCHRONIZED` IR compile that reaches the planner -- the
            // only compiles the switch can change. Zero lines on a workload
            // means both arms compile the same bodies.
            if cached.is_synchronized && x64::dbg_jitc_enabled() {
                eprintln!(
                    "{}",
                    sync_method_chains_dbg_line(
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        deopt::sync_method_chains_enabled(),
                        ir_self_lock_requested,
                        ir_chain_fences,
                    )
                );
            }
            // Round 14 wave 3 (lane chain): the call sites a chain trap's
            // charges withdrew (`ir_splice_site_withdrawn`). The key is built
            // only when a registry is present and the switch is on.
            let ir_despec_splice_key: Option<String> = req.despec.filter(|_| true).map(|_| {
                format!(
                    "{}.{}:{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                )
            });
            // al16: `local -> class` for the write-once locals holding a
            // spliced factory call's result (`ir_factory_receiver_enabled`).
            let ir_factory_receiver = true;
            let ir_factory_over_profile =
                ir_factory_receiver && ir_factory_receiver_over_profile_enabled();
            let mut ir_factory_locals: HashMap<usize, u32> = HashMap::new();
            // The factory sites whose result a local holds
            // (`ir_factory_trap_miss_enabled`).
            let mut ir_factory_site_pcs: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // Round nio20 wave 6 (lane L1): the planned `java.nio` byte-buffer
            // sites (`ir_buffer_trap_miss_enabled`), read once per compile.
            let ir_buffer_trap_miss = ir_buffer_trap_miss_enabled();
            let ir_buffer_lone_put_call = ir_buffer_trap_lone_put_call_enabled();
            let mut ir_buffer_site_pcs: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // nio21 wave 3 (lane L3): the `put*` ones among them
            // (`ir_buffer_trap_lone_put_call_enabled`).
            let mut ir_buffer_put_site_pcs: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // JIT round 18 wave 10 (lane buffers): with
            // `CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST` the sites whose next
            // instruction is a `return` (run at most once per invocation) are
            // visited after every other site, so they no longer spend the
            // site cap and byte budget ahead of the loop body's helpers. See
            // `ir_splice_order`.
            // Round nio20 wave 1 (lane L5, NIO19-4): the profile-ordered visit
            // (`CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER`, default ON; supersedes
            // the tail-exit order) and the hot-loop site cap
            // (`CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP`, default ON): in-loop
            // sites first, cold exits last, and a method with a hot loop may
            // plan `IR_INLINE_MAX_SITES_HOT_LOOP` sites, the slots past
            // `IR_INLINE_MAX_SITES` going to in-loop sites only.
            let ir_profile_order_on = ir_splice_order::ir_splice_profile_order_enabled();
            let ir_hot_loop_cap_on = ir_splice_order::ir_splice_hot_loop_cap_enabled();
            let ir_ranked: Option<ir_splice_order::ProfilePlanOrder> =
                (ir_profile_order_on || ir_hot_loop_cap_on).then(|| {
                    ir_splice_order::profile_plan_order(code, code_len, &ir_invoke_ops, profile)
                });
            let ir_site_cap = match ir_ranked.as_ref() {
                Some(r) if ir_hot_loop_cap_on && r.hot_loop => {
                    ir_splice_order::IR_INLINE_MAX_SITES_HOT_LOOP.max(IR_INLINE_MAX_SITES)
                }
                _ => IR_INLINE_MAX_SITES,
            };
            if let Some(r) = ir_ranked.as_ref() {
                if x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-splice-profile-order {} site(s) ({} in-loop, {} cold, \
                         profiled={}, order={}, cap {}) in {}.{}{}: {}",
                        r.sites.len(),
                        r.count_of(ir_splice_order::SiteClass::InLoop),
                        r.count_of(ir_splice_order::SiteClass::Cold),
                        r.profiled,
                        if ir_profile_order_on {
                            "profile"
                        } else {
                            "bytecode"
                        },
                        ir_site_cap,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        r.describe(),
                    );
                }
            }
            let ir_plan_order: std::borrow::Cow<'_, [(usize, u16, u8)]> =
                if let Some(r) = ir_ranked.as_ref().filter(|_| ir_profile_order_on) {
                    std::borrow::Cow::Owned(r.ops())
                } else if ir_splice_order::ir_splice_tail_exits_last_enabled() {
                    let (order, moved) =
                        ir_splice_order::tail_exits_last(code, code_len, &ir_invoke_ops);
                    if moved > 0 && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-tail-exits-last {moved} of {} site(s) \
                             deferred in {}.{}{}",
                            order.len(),
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                    std::borrow::Cow::Owned(order)
                } else {
                    std::borrow::Cow::Borrowed(&ir_invoke_ops[..])
                };
            // JIT round 20 (MHIFACE19-2; `CRATONVM_JIT_MH_INTERFACE_CHA_SPLICE`):
            // an interface constant handle's site has no receiver profile (its
            // method is compiled at its first call, and an interpreted
            // `invokeExact` profiles the handle, not the member's receiver), so
            // the class-hierarchy answer stands in: the single loaded concrete
            // class providing the member, spliced behind the guarded splice's
            // exact class test, whose miss edge is the ordinary interface call
            // (an implementor loaded later takes it). Only these sites: their
            // receiver is already proven an instance of the interface
            // (`IrBuilder::mh_constant_interface_receiver_guard`).
            let ir_mh_interface_pcs = if mh_interface_cha_splice_enabled() {
                ir_mh_constant.interface_pcs()
            } else {
                std::collections::HashSet::new()
            };
            let ir_mh_interface_cha_class = |pc: usize, cn: &str, mn: &str, desc: &str| {
                if !ir_mh_interface_pcs.contains(&pc) {
                    return None;
                }
                let bind = req
                    .unique_concrete_resolver
                    .and_then(|cha| cha(cn, mn, desc));
                if x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-mh-interface-cha {cn}.{mn}{desc} at pc={pc} in {}.{}{}: {}",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        match &bind {
                            Some(b) => format!("implementor class id {}", b.implementor_class_id),
                            None => "no single implementor".to_string(),
                        },
                    );
                }
                bind.map(|b| b.implementor_class_id).filter(|&id| id != 0)
            };
            // JIT round 21 wave 3 (lane splice; `CRATONVM_JIT_IR_CHA_GUARDED_SPLICE`,
            // perf, opt-in): any own virtual / interface site with NO usable
            // receiver row (`ir_cha_guard_site_unprofiled`: an eagerly compiled
            // kernel, a site the interpreter reached fewer than
            // `MIN_GUARDED_RECEIVER_EVIDENCE` times) takes the class-hierarchy
            // answer as its guard class, as the MH interface sites above do and
            // as the single-pass `plan_inline` ladder does for
            // `ReceiverShape::Unprofiled`. Sound for any answer: the splice is
            // behind `ExactClassIs` with the ordinary call as its miss edge, so a
            // class loaded later takes the call. Never for a profiled site (a
            // megamorphic row contradicts a one-implementor answer).
            let ir_cha_guard_on = ir_cha_guarded_splice_enabled();
            let ir_cha_guard_class = |pc: usize, opcode: u8, cn: &str, mn: &str, desc: &str| {
                if !ir_cha_guard_on
                    || pc >= code_len
                    || !matches!(opcode, 0xb6 | 0xb9)
                    || !ir_cha_guard_site_unprofiled(profile, pc)
                {
                    return None;
                }
                let id = req
                    .unique_concrete_resolver
                    .and_then(|cha| cha(cn, mn, desc))
                    .map(|b| b.implementor_class_id)
                    .filter(|&id| id != 0);
                if x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-cha-guard {cn}.{mn}{desc} at pc={pc} in {}.{}{}: {}",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        match id {
                            Some(id) => format!("implementor class id {id}"),
                            None => "no single implementor".to_string(),
                        },
                    );
                }
                id
            };
            let ir_root_handler_pcs: Vec<usize> = cached
                .exception_table
                .iter()
                .map(|e| usize::from(e.handler_pc))
                .collect();
            // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: this method's
            // write-once locals holding a `new`, by the class it allocates.
            let ir_root_new_locals: HashMap<usize, u32> =
                if ir::ir_static_final_exact_class_enabled() {
                    ir::write_once_new_locals(code, code_len)
                        .into_iter()
                        .filter(|(_, new_pc)| {
                            !cached
                                .exception_table
                                .iter()
                                .any(|e| usize::from(e.handler_pc) == *new_pc)
                        })
                        .filter_map(|(k, new_pc)| Some((k, builder.new_class_at(new_pc)?)))
                        .collect()
                } else {
                    HashMap::new()
                };
            // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: and the write-once
            // locals holding a static factory's result whose body allocates
            // one class (`asCharSequence(asciiString)`'s `new $1`), resolved
            // here because the factory's own site is planned after the
            // in-loop sites that pass the local on. A planning hint only: the
            // builder splices a nested body resolved for that class only when
            // its proof (the slot's one store wrote the spliced `new`, the OSR
            // arm re-tests it) holds, and keeps the call otherwise.
            let mut ir_root_new_locals = ir_root_new_locals;
            if ir::ir_static_final_exact_class_enabled() && ir_factory_receiver {
                ir::set_splice_arg_classes(Vec::new());
                for &(pc, cp_idx, opcode) in ir_plan_order.iter() {
                    if opcode != 0xb8 || pc >= code_len {
                        continue;
                    }
                    let Some(k) = ir_factory_result_local(code, code_len, pc, opcode) else {
                        continue;
                    };
                    if ir_root_new_locals.contains_key(&k) {
                        continue;
                    }
                    let Some((cn, mn, desc)) = invoke_resolver(cp_idx) else {
                        continue;
                    };
                    if let Some(class_id) = ir_resolver(&cn, &mn, &desc)
                        .as_ref()
                        .and_then(ir_factory_new_class)
                    {
                        ir_root_new_locals.insert(k, class_id);
                    }
                }
                ir::set_splice_arg_classes(Vec::new());
            }
            ir_osr_new_local_classes = ir_root_new_locals.clone();
            for &(pc, cp_idx, opcode) in ir_plan_order.iter() {
                if sites_planned >= ir_site_cap || budget == 0 {
                    break;
                }
                // Past the flat cap only an in-loop site of a hot-loop method
                // may still be planned (`ir_site_cap` is the flat cap
                // otherwise, so this never fires then).
                if sites_planned >= IR_INLINE_MAX_SITES
                    && !ir_ranked
                        .as_ref()
                        .is_some_and(|r| r.takes_hot_loop_slot(pc))
                {
                    continue;
                }
                // Round 12 wave 6 (lane iropt5, W5-2): a site an earlier build
                // of this compile refused in stays the ordinary call.
                if ir_live_without_pruned
                    .as_ref()
                    .is_some_and(|live| !live.contains(&pc))
                {
                    continue;
                }
                if ir_splice_skip.contains(&pc) {
                    continue;
                }
                // JIT round 21 wave 17 (lane records, IN16-6): a record fold's
                // `Objects` call is built by the fold, never spliced here.
                if ir_indy_direct.is_record_fold_pc(pc) {
                    continue;
                }
                // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): neither
                // call of a filter species site is spliced (a deopt inside a
                // splice re-executes the `invokeExact`, running the other
                // call twice). A box-returning converting site's member,
                // planned at its `getstatic` pc, is the exception: the builder
                // splices it only when its body cannot raise
                // (`IrBuilder::convert_member_splice_admitted`), so no trap
                // resumes inside it, and boxes its result after it.
                let convert_member_at = ir_mh_constant.convert_member_invoke_pc(pc);
                if ir_mh_constant.is_filter_pc(pc)
                    || (ir_mh_constant.is_convert_two_call_pc(pc) && convert_member_at.is_none())
                {
                    continue;
                }
                // The member's receiver is the `invokeExact`'s first argument;
                // its exact class (a `static final` read) selects the body.
                let convert_member_receiver: Option<u32> = convert_member_at
                    .filter(|_| opcode != 0xb8)
                    .and_then(|invoke_pc| {
                        let &(_, invoke_cp, _) =
                            scan.invoke_ops.iter().find(|&&(p, _, _)| p == invoke_pc)?;
                        let (_, _, invoke_desc) =
                            cp_invoke_resolver.and_then(|resolve| resolve(invoke_cp))?;
                        ir_root_site_arg_classes(
                            &builder,
                            req.static_final_ref_class_resolver,
                            &ir_root_new_locals,
                            &ir_indy_proxy_classes,
                            &ir_indy_shapes,
                            &scan.invoke_ops,
                            cp_invoke_resolver,
                            code,
                            code_len,
                            &ir_root_handler_pcs,
                            invoke_pc,
                            0xb6,
                            &invoke_desc,
                        )
                        .get(1)
                        .copied()
                        .flatten()
                    });
                // A site the compile door found unresolved stays a call: its
                // first-execution guard rides on it (interpreter round i1 wave
                // 50, lane L1).
                if ir_first_call.contains_key(&pc) {
                    continue;
                }
                if let Some(key) = ir_despec_splice_key.as_deref() {
                    if ir_splice_site_withdrawn(req.despec.map(|r| &**r), key, pc) {
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-splice withdrawn at pc={pc} in {key}: \
                                 call-site wildcard de-spec, kept as a call"
                            );
                        }
                        continue;
                    }
                }
                let Some((cn, mn, desc)) = invoke_resolver(cp_idx) else {
                    continue;
                };
                // A box javac stores straight into the varargs array of a
                // reflective call (`m.invoke(o, i)`) stays a call: the spliced
                // `Method.invoke` and escape analysis remove that array, and the
                // box folds only as a call (`ea_ir_bridge`'s box folds).
                let next_invoke = ir_invoke_ops
                    .iter()
                    .filter(|&&(p, _, _)| p > pc)
                    .min_by_key(|&&(p, _, _)| p)
                    .and_then(|&(_, c, _)| invoke_resolver(c));
                if reflective_varargs_box_site(
                    code,
                    code_len,
                    pc,
                    (&cn, &mn, &desc),
                    next_invoke
                        .as_ref()
                        .map(|(c, m, _)| (c.as_str(), m.as_str())),
                ) {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused reflective-varargs-box {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                // zs s7/vl (`CRATONVM_DBG_JIT_SINGLE_PASS_ONLY`): a callee the
                // measurement filter names stays a call, so it runs its own
                // (single-pass) body rather than this tier's lowering of it.
                if dbg_jit_single_pass_only(&cn, &mn) {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused single-pass-only {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                // nio20 wave 3 fix-back (L2): a `String.equals` / `compareTo` /
                // `indexOf` site the inline search prefix answers stays a CALL.
                // Spliced, the site has no `Op::Call` for the prefix to sit on
                // and runs the JDK body instead (`equals` -> `StringLatin1.equals`,
                // `indexOf(I)` -> `indexOf(I,I)` -> ...): measured on
                // `Nio20StringIr`, equals 42 ns/op spliced vs 9 on the
                // single-pass intrinsic, indexofc 85. `hashCode` (its spliced
                // body is the same cached read) and the guarded CharSequence
                // accessors (whose profiled non-String receivers splice behind
                // a guard) keep splicing.
                // nio20 wave 4 (L5): the kind list is
                // `ir_string_prefix_site_stays_a_call` (adds
                // `StringBuilder.append(C)`), shared with the nested twin.
                // rx27 wave 3 review item 1: a `CharSequence.subSequence` site
                // whose receiver profile is not String keeps its splice
                // (`top_level_string_prefix_site_stays_call`).
                if top_level_string_prefix_site_stays_call(
                    &cn,
                    &mn,
                    &desc,
                    opcode,
                    profile.and_then(|p| p.receivers.get(&pc)),
                    resolved_string_layout.map(|l| l.string_class_id),
                ) {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused string-search-prefix-site {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                if x64::dbg_jitc_enabled()
                    && ir_string_search_prefix_handled_kind(&cn, &mn, &desc)
                        == Some(IrStringSearchKind::SeqSubSequence)
                {
                    eprintln!(
                        "[cratonvm-jitc] ir-splice string-search-prefix-site {cn}.{mn}{desc} at pc={pc} in {}.{}{}: \
                         receiver profile not String, left to the guarded planner",
                        cached.class_name, cached.method_name, cached.method_descriptor,
                    );
                }
                // Round rx27 wave 3 (lane splitfast,
                // `CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX`, opt-in): an
                // `ArrayList.add(Object)` site the add prefix fronts stays a
                // CALL, as the String prefix sites above.
                if ir_arraylist_add_site_stays_call(&cn, &mn, &desc, opcode, resolved_string_layout)
                {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused arraylist-add-prefix-site {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                // JIT r23 w4 (lane L8): `MemorySegment` element accessors and JDK `Throwable` `<init>`
                // stay a CALL so dispatch-time fast paths handle them directly.
                if ir_ffm_element_bound_site_stays_a_call(&cn, &mn, &desc, direct_helpers)
                    || (opcode == 0xb7 && ir::is_jdk_throwable_init_candidate(&cn, &mn, &desc))
                    || ir_ffm_inline_sites.contains_key(&pc)
                    || ir_ffm_unreached_sites.contains_key(&pc)
                    // JIT r23 wave 13 (lane L4): an FFM seed-call site stays a call.
                    || ir_ffm_seed_call_sites.contains(&pc)
                    // JIT r23 wave 12 (lane L7): a generic never-reached trap site.
                    || ir_unreached_call_sites.contains_key(&pc)
                {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused ffm-element-direct-site {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                // nio20 w5 fix-back (L5): a StringBuilder site that reaches the
                // splice planner here (not carved out above) says so, with the
                // reason, so a prefix that never fires at a hot site can be told
                // apart from one the lowering declined.
                if cn == "java/lang/StringBuilder" && x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-splice string-builder-site {cn}.{mn}{desc} at pc={pc} \
                         opcode={opcode:#x} prefix_kind={:?} in {}.{}{}: not kept as a call",
                        ir_string_search_prefix_handled_kind(&cn, &mn, &desc),
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                    );
                }
                // A virtual site the plain resolver refused may still be
                // monomorphic because its receiver is exactly the constant-pool
                // class (`map.put` on `HashMap map = new HashMap<>()`). Only the
                // compiling method's own `invokevirtual`s: the builder's proof
                // reads the caller's locals.
                // Round 14 wave 2 (lane guard, GS-1): the second body of a
                // bimorphic site, when the arm below plans one.
                let mut ir_bimorphic_second: Option<IrGuardedBody> = None;
                // Round 16 (lane mega): the third and later bodies of a
                // polymorphic site, when the arm below plans one.
                let mut ir_polymorphic_extra: Vec<IrGuardedBody> = Vec::new();
                // nio21 wave 6, lane L4: the chosen body is a one-class
                // profile-guarded one over the cold cap (hot tier).
                let mut site_guarded_hot = false;
                // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: whether a body
                // resolved for this site may throw by a compiled edge -- the
                // site lies in a `try` whose first covering row's handler
                // follows it (the planner's own candidate rule).
                ir::set_ir_root_site_in_try(
                    ir::ir_spliced_throw_edges_enabled()
                        && cached
                            .exception_table
                            .iter()
                            .find(|e| usize::from(e.start_pc) <= pc && pc < usize::from(e.end_pc))
                            .is_some_and(|e| usize::from(e.handler_pc) > pc),
                );
                // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the exact
                // classes this site's operands carry into the body.
                let mut _site_objects: Option<ir::SpliceSiteObjects> = None;
                let mut site_arg0_class: Option<u32> = None;
                if ir::ir_static_final_exact_class_enabled() {
                    // A convert member's pc is a `getstatic`, not its call:
                    // only the receiver's class is known.
                    let mut classes = if convert_member_at.is_some() {
                        convert_member_receiver
                            .map(|c| vec![Some(c)])
                            .unwrap_or_default()
                    } else {
                        ir_root_site_arg_classes(
                            &builder,
                            req.static_final_ref_class_resolver,
                            &ir_root_new_locals,
                            &ir_indy_proxy_classes,
                            &ir_indy_shapes,
                            &scan.invoke_ops,
                            cp_invoke_resolver,
                            code,
                            code_len,
                            &ir_root_handler_pcs,
                            pc,
                            opcode,
                            &desc,
                        )
                    };
                    // `CRATONVM_FROZEN_PAGES`: and the frozen objects they are
                    // expected to be, whose classes the snapshot knows.
                    if let Some(facts) = req.frozen_facts.filter(|_| convert_member_at.is_none()) {
                        let this_addr = req
                            .frozen_receivers
                            .filter(|_| !cached.is_static)
                            .and_then(|seen| seen.get(&*cached.class_name).copied());
                        let mut objects = ir_root_site_arg_objects(
                            &builder,
                            facts,
                            this_addr,
                            code,
                            code_len,
                            &ir_root_handler_pcs,
                            pc,
                            opcode,
                            &desc,
                        );
                        // A virtual call's receiver nothing names: the object
                        // this site is nearly always called on, of the class
                        // the snapshot has (an exact row the builder proves
                        // with an identity guard at the call).
                        if matches!(opcode, 0xb6 | 0xb9)
                            && objects.first().is_none_or(Option::is_none)
                        {
                            let site = format!(
                                "{}.{}:{}@{pc}",
                                cached.class_name, cached.method_name, cached.method_descriptor
                            );
                            if let Some(&addr) =
                                req.frozen_receivers.and_then(|seen| seen.get(&site))
                            {
                                let class = facts
                                    .object_id(addr)
                                    .and_then(|id| facts.objects.get(id as usize))
                                    .map_or(0, |o| o.class_id);
                                if objects.is_empty() {
                                    objects.push(None);
                                }
                                objects[0] = Some((class, addr));
                            }
                        }
                        for (slot, object) in objects.iter().enumerate() {
                            if let Some((class, _)) = object.filter(|&(class, _)| class != 0) {
                                if classes.len() <= slot {
                                    classes.resize(slot + 1, None);
                                }
                                classes[slot].get_or_insert(class);
                            }
                        }
                        _site_objects = Some(ir::SpliceSiteObjects::set(
                            objects.iter().map(|o| o.map(|(_, addr)| addr)).collect(),
                        ));
                    }
                    site_arg0_class = classes.first().copied().flatten();
                    ir::set_splice_arg_classes(classes);
                }
                // JIT round 23 wave 6: the CHA watermark the exact-receiver
                // resolver noted for the constant-pool answer, kept only when
                // that answer is the one planned (below).
                let mut site_cha_watermark: u32 = 0;
                // j27 wave 5 (lane PROXY): the class set the profile-receiver
                // resolver noted for the profile answer, kept only when that
                // answer is the one planned (below).
                let mut site_cha_class_set: Vec<u32> = Vec::new();
                // JIT round 21 lane indylate2: an indy-planned site's body is
                // resolved in the plan's class context (the target class).
                let ir_site_answer = match ir_indy_direct
                    .class_context_at(pc)
                    .filter(|_| pc < code_len)
                    .zip(req.indy_context_ir_inline_resolver)
                {
                    Some((context, by_context)) => by_context(context, &cn, &mn, &desc),
                    None => ir_resolver(&cn, &mn, &desc),
                };
                // A convert member of an instance method: the body for its
                // receiver's exact class, or the call stays.
                let convert_member_exact = match (convert_member_at, opcode) {
                    (None, _) | (Some(_), 0xb8) => None,
                    (Some(_), _) => {
                        let answer = convert_member_receiver.and_then(|class_id| {
                            req.ir_receiver_inline_resolver
                                .and_then(|resolve| resolve(class_id, &cn, &mn, &desc))
                                .filter(|site| !site.callee_is_static)
                                .map(|site| (site, class_id))
                        });
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-mh-convert-member {cn}.{mn}{desc} at pc={pc} in {}.{}{}: \
                                 receiver class {convert_member_receiver:?} -> {}",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                if answer.is_some() { "planned" } else { "kept as a call" },
                            );
                        }
                        match answer {
                            Some(answer) => Some(answer),
                            None => continue,
                        }
                    }
                };
                let (mut site, exact_receiver) = match convert_member_exact {
                    Some((site, class_id)) => (site, Some(class_id)),
                    None => match ir_site_answer {
                        Some(site) => (site, None),
                        None => {
                            let exact_answer = match req.ir_exact_receiver_resolver {
                                Some(exact)
                                    if opcode == 0xb6
                                        && pc < code_len
                                        && ir::ir_exact_receiver_splice_enabled() =>
                                {
                                    // Cleared first, so only this call's note is read.
                                    let _ = ir::take_cha_watermark();
                                    let answer = exact(&cn, &mn, &desc)
                                        .filter(|&(_, class_id)| class_id != 0);
                                    let noted = ir::take_cha_watermark();
                                    if answer.is_some() && ir::ir_cha_monomorphic_splice_enabled() {
                                        site_cha_watermark = noted;
                                    }
                                    answer
                                }
                                _ => None,
                            };
                            // Round 13 wave 12 (lane guardsplice, W7-1): the class
                            // the receiver profile says dominates the site, when it
                            // is not the constant-pool class, resolved and guarded
                            // in place of it (`ir_profile_guarded_site`). The
                            // constant-pool answer stays the fallback.
                            // al16: no profile at the site, but its receiver is
                            // a spliced factory's allocation. The for-each
                            // residual round: with a profile too
                            // (`ir_factory_receiver_over_profile_enabled`).
                            let factory_class = if ir_factory_receiver
                                && (ir_factory_over_profile
                                    || ir::profile_guarded_receiver_class(profile, pc).is_none())
                                && pc < code_len
                            {
                                ir_factory_receiver_class(
                                    code,
                                    pc,
                                    opcode,
                                    &desc,
                                    &ir_factory_locals,
                                )
                            } else {
                                None
                            };
                            if let Some(class_id) = factory_class {
                                if x64::dbg_jitc_enabled() {
                                    eprintln!(
                                    "[cratonvm-jitc] ir-factory-receiver {cn}.{mn}{desc} at pc={pc} \
                                     in {}.{}{}: receiver class id {class_id}",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                                }
                            }
                            // nio21 wave 6, lane L4: a profile-hot site's body may
                            // exceed the cold cap (`ir_guarded_splice_site_cap`);
                            // nio21 wave 7, lane L4: so may the bodies of a hot
                            // bimorphic / polymorphic site, sharing it
                            // (`ir_guarded_multi_body_cap`).
                            let site_cap = ir_guarded_splice_site_cap(
                                pc < code_len
                                    && ir_guarded_site_is_hot(ir_ranked.as_ref(), profile, pc),
                                budget,
                                ir_guarded_hot_pcs.len(),
                            );
                            let profiled =
                                ir_profile_guard_class(
                                    opcode,
                                    pc < code_len,
                                    exact_answer.as_ref().map(|&(_, class_id)| class_id),
                                    // JIT round 21 wave 19 (lane mhbind2;
                                    // MHBIND21-1): a bound constant handle's site
                                    // knows its receiver's exact class (the handle's
                                    // `final` bound value); the profile there is the
                                    // handle's own class.
                                    ir_mh_constant
                                        .bound_receiver_class_at(pc)
                                        .or(site_arg0_class)
                                        .or_else(|| ir::profile_guarded_receiver_class(profile, pc))
                                        .or_else(|| ir_mh_interface_cha_class(pc, &cn, &mn, &desc))
                                        .or_else(|| ir_cha_guard_class(pc, opcode, &cn, &mn, &desc))
                                        // JIT r23 wave 7 (L1): the top class of a site
                                        // every multi-class reader declines
                                        // (`CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE`).
                                        .or_else(|| {
                                            ir::profile_major_receiver_class(
                                        profile,
                                        pc,
                                        ir::ir_polymorphic_guarded_splice_enabled()
                                            || (req.osr_mode_exit_polls
                                                && ir::osr_door_ir_polymorphic_splice_enabled()),
                                    )
                                        })
                                        // j27 wave 5 (lane PROXY): the top class of an
                                        // interface site every reader above declined,
                                        // as the seed of a class-set guard
                                        // (`CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`).
                                        .or_else(|| {
                                            ir::cha_class_set_seed_class(
                                        profile,
                                        pc,
                                        opcode,
                                        ir::ir_polymorphic_guarded_splice_enabled()
                                            || (req.osr_mode_exit_polls
                                                && ir::osr_door_ir_polymorphic_splice_enabled()),
                                    )
                                        }),
                                )
                                .and_then(|class_id| {
                                    let cap = site_cap;
                                    ir_profile_guarded_site_capped(
                                        req.ir_receiver_inline_resolver,
                                        class_id,
                                        &cn,
                                        &mn,
                                        &desc,
                                        cap,
                                    )
                                });
                            // j27 wave 5 (lane PROXY): the VM's class set for the
                            // profile answer (`InlineSite::cha_class_set`), at an
                            // outermost interface site.
                            if let Some((site, _)) = profiled.as_ref() {
                                if opcode == 0xb9
                                    && pc < code_len
                                    && ir::ir_cha_interface_class_set_enabled()
                                {
                                    site_cha_class_set = site.cha_class_set.clone();
                                }
                            }
                            // al16: the factory class as an EXACT-receiver site
                            // (the builder splices only a proven receiver and keeps
                            // the call otherwise), so no guarded-body size cap.
                            let factory_exact: Option<(InlineSite, u32)> = match factory_class {
                                Some(class_id) if profiled.is_none() || ir_factory_over_profile => {
                                    req.ir_receiver_inline_resolver
                                        .and_then(|resolve| resolve(class_id, &cn, &mn, &desc))
                                        .filter(|site| !site.callee_is_static)
                                        .map(|site| (site, class_id))
                                }
                                _ => None,
                            };
                            if let Some((_, class_id)) = profiled.as_ref() {
                                if x64::dbg_jitc_enabled() {
                                    eprintln!(
                                    "[cratonvm-jitc] ir-profile-guard {cn}.{mn}{desc} at pc={pc} \
                                     in {}.{}{}: receiver class id {class_id}{}",
                                    cached.class_name,
                                    cached.method_name,
                                    cached.method_descriptor,
                                    match exact_answer.as_ref() {
                                        Some((_, cp)) => format!(" (constant-pool class id {cp})"),
                                        None => String::new(),
                                    },
                                );
                                }
                            } else if (opcode == 0xb6 || opcode == 0xb9)
                                && pc < code_len
                                && x64::dbg_jitc_enabled()
                            {
                                // rx27 lane bimodal: the receiver evidence a site
                                // that got no one-class guarded splice was read
                                // from (`no-profile`, `no-row`, or the summary).
                                eprintln!(
                                "[cratonvm-jitc] ir-profile-guard-miss {cn}.{mn}{desc} at pc={pc} \
                                 in {}.{}{}: {}",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                ir_receiver_evidence_note(profile, pc),
                            );
                            }
                            // Round 14 wave 2 (lane guard, GS-1): no class holds
                            // the one-class share, but two together do: both
                            // bodies, each behind its own test, in place of the
                            // constant-pool answer (which stays the fallback). Not
                            // under a self-recursion plan, whose copies the second
                            // body's preparation does not model.
                            let bimorphic = if profiled.is_none()
                                && factory_exact.is_none()
                                && ir_self_rec.is_none()
                            {
                                let answer = ir_profile_bimorphic_sites_capped(
                                    opcode,
                                    pc < code_len,
                                    ir::profile_bimorphic_receiver_classes(profile, pc),
                                    req.ir_receiver_inline_resolver,
                                    &cn,
                                    &mn,
                                    &desc,
                                    site_cap,
                                );
                                if x64::dbg_jitc_enabled() {
                                    if let Some(line) = ir_bimorphic_census_line(profile, pc) {
                                        eprintln!(
                                            "[cratonvm-jitc] ir-bimorphic-census {cn}.{mn}{desc} \
                                         at pc={pc} in {}.{}{}: {line} -> {}",
                                            cached.class_name,
                                            cached.method_name,
                                            cached.method_descriptor,
                                            match &answer {
                                                Ok(((_, a), (_, b))) => format!("planned {a}+{b}"),
                                                Err(why) => (*why).to_string(),
                                            },
                                        );
                                    }
                                }
                                // Round 16 (lane mega): no pair holds the share, but
                                // three or four classes do: every body behind its own
                                // test (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`).
                                // JIT round 19 wave 1 (lane osrpoly19,
                                // OSRFOLLOW18-3): or, for an OSR-door compile,
                                // `CRATONVM_JIT_OSR_DOOR_IR_POLYMORPHIC_SPLICE`.
                                let ir_polymorphic_on = ir::ir_polymorphic_guarded_splice_enabled()
                                    || (req.osr_mode_exit_polls
                                        && ir::osr_door_ir_polymorphic_splice_enabled());
                                let answer = match answer {
                                    Err("profile-not-bimorphic") if ir_polymorphic_on => {
                                        let poly = ir_profile_polymorphic_sites_capped(
                                            ir_polymorphic_on,
                                            opcode,
                                            pc < code_len,
                                            ir::profile_polymorphic_receiver_classes(profile, pc),
                                            req.ir_receiver_inline_resolver,
                                            &cn,
                                            &mn,
                                            &desc,
                                            site_cap,
                                        );
                                        if x64::dbg_jitc_enabled() {
                                            let what = match &poly {
                                                Ok(bodies) => {
                                                    let ids: Vec<String> = bodies
                                                        .iter()
                                                        .map(|(_, c)| c.to_string())
                                                        .collect();
                                                    format!("planned {}", ids.join("+"))
                                                }
                                                Err(why) => (*why).to_string(),
                                            };
                                            eprintln!(
                                            "[cratonvm-jitc] ir-polymorphic-census {cn}.{mn}{desc} \
                                             at pc={pc} in {}.{}{}: {what}",
                                            cached.class_name,
                                            cached.method_name,
                                            cached.method_descriptor,
                                        );
                                        }
                                        match poly {
                                            Ok(mut bodies) => {
                                                let rest = bodies.split_off(2.min(bodies.len()));
                                                let mut pair = bodies.into_iter();
                                                match (pair.next(), pair.next()) {
                                                    (Some(first), Some(second)) => {
                                                        ir_polymorphic_extra = rest;
                                                        Ok((first, second))
                                                    }
                                                    _ => Err("second-body-refused"),
                                                }
                                            }
                                            Err(why) => Err(why),
                                        }
                                    }
                                    other => other,
                                };
                                answer.ok()
                            } else {
                                None
                            };
                            match bimorphic {
                                Some((first, second)) => {
                                    // nio21 wave 7, lane L4: any body over the
                                    // cold cap makes this a hot-cap site for the
                                    // builder (`guarded_body_row_admitted`).
                                    let cold = ir::GUARDED_SPLICE_MAX_BODY_BYTES;
                                    site_guarded_hot = first.0.callee_code_len > cold
                                        || second.0.callee_code_len > cold
                                        || ir_polymorphic_extra
                                            .iter()
                                            .any(|(s, _)| s.callee_code_len > cold);
                                    // Two receiver types can select one method. Share a
                                    // leaf body behind the exact class-set guard instead
                                    // of duplicating it (or attributing an exact receiver
                                    // fact to a body that serves both classes).
                                    if ir_polymorphic_extra.is_empty() {
                                        site_cha_class_set =
                                            same_target_splice::receiver_classes(&first, &second)
                                                .unwrap_or_default();
                                    }
                                    if site_cha_class_set.is_empty() {
                                        ir_bimorphic_second = Some(second);
                                    } else if x64::dbg_jitc_enabled() {
                                        eprintln!(
                                        "[cratonvm-jitc] ir-same-target-splice {cn}.{mn}{desc} at pc={pc} in {}.{}{}: class ids {:?} target {}.{}{}",
                                        cached.class_name, cached.method_name, cached.method_descriptor,
                                        site_cha_class_set, first.0.class_name, first.0.method_name, first.0.descriptor,
                                    );
                                    }
                                    // Not the constant-pool answer.
                                    site_cha_watermark = 0;
                                    (first.0, Some(first.1))
                                }
                                // The factory answer first: it is `None` with a
                                // profile unless the switch above put it there.
                                None => {
                                    site_guarded_hot = factory_exact.is_none()
                                        && profiled.as_ref().is_some_and(|(s, _)| {
                                            s.callee_code_len > ir::GUARDED_SPLICE_MAX_BODY_BYTES
                                        });
                                    // JIT round 23 wave 6: the watermark belongs to
                                    // the constant-pool answer only.
                                    if factory_exact.is_some() || profiled.is_some() {
                                        site_cha_watermark = 0;
                                    }
                                    // j27 wave 5 (lane PROXY): the class set
                                    // belongs to the profile answer only.
                                    if factory_exact.is_some() {
                                        site_cha_class_set.clear();
                                    }
                                    match factory_exact.or(profiled).or(exact_answer) {
                                        Some((site, class_id)) => (site, Some(class_id)),
                                        None => continue,
                                    }
                                }
                            }
                        }
                    },
                };
                if convert_member_at.is_some()
                    && !IrMhConstantPlan::convert_member_body_cannot_raise(&site, 1)
                {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-mh-convert-member {cn}.{mn}{desc} at pc={pc} in {}.{}{}: body may raise, kept as a call",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                let protected_allocation_chain = cached
                    .exception_table
                    .iter()
                    .any(|row| usize::from(row.start_pc) <= pc && pc < usize::from(row.end_pc))
                    && ir_splice_tree_has_allocation(&site);
                let fresh_array_argument =
                    ir_splice_site_takes_fresh_array_argument(&site, code, pc);
                if !ir::ir_splice_chain_fences_enabled()
                    && (protected_allocation_chain || fresh_array_argument)
                {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused caller-frame-allocation-boundary {}.{}{} at pc={pc} in {}.{}{}",
                            cn, mn, desc, cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                // al16: a factory body's class, recorded for its result local
                // once the site is planned (below).
                let site_factory_class = if ir_factory_receiver && pc < code_len {
                    ir_factory_new_class(&site)
                } else {
                    None
                };
                // Round nio20 wave 6 (lane L1): a byte-buffer access, for the
                // trap-miss rule below (`ir_buffer_trap_site`).
                let site_is_buffer = ir_buffer_trap_miss
                    && pc < code_len
                    && ir_buffer_trap_site(&cn, &site.class_name);
                let site_is_buffer_put = site_is_buffer && site.method_name.starts_with("put");
                // A callee the single-pass backend lowers with a pre-header the
                // IR tier cannot match (a vectorised reduction, element-wise
                // SIMD, a matrix dot, the bulk byte loops) stays a call. The
                // admission chain already refuses that method as a compile unit
                // (`single_pass_only_lowering_for`), and splicing it lowers the
                // same loop scalar: 10.5 ms -> 171 ms on `review8a/LoopPerf2`
                // (round 9 wave 8b). The resolver refuses every callee with an
                // exception table (`callee-exception-table`), so the empty table
                // is exact here. Honours `CRATONVM_JIT_C1_VECTOR_VETO` through
                // the callee. A switch dispatcher's tail-exit loop kernel too
                // (`ir_splice_order::switch_tail_loop_site`).
                if x64::single_pass_only_lowering(&site.callee_code, site.callee_code_len, &[])
                    .is_some()
                    || ir_splice_order::switch_tail_loop_site(code, code_len, pc, &site)
                {
                    continue;
                }
                // Strings round (`str16`): a callee whose body makes a
                // String/StringBuilder call the single-pass backend serves
                // inline and this tier cannot (`hashCode`, `equals`,
                // `indexOf`, the builder appends: everything but the three
                // field accessors, and not even those inside a splice) stays
                // a call -- the splice twin of the method-level String pin.
                // Spliced, `R16StringsSplit.toStrHash`'s `hashCode()` became
                // a call chain ending in the `vectorizedHashCode` native in
                // whichever process the IR tier compiled `row` first: 108
                // ns/op against ~1 250 for the same row (`docs/feature-designs/jit-ir-optimizations.md
                // performance/r16-strings-stringbuilder-tostring-is-a-
                // microsecond-native-call-FIXED-20261006.md`).
                let string_intrinsic_nested =
                    ir_splice_strip_string_intrinsic_nested(&mut site, resolved_string_layout);
                if string_intrinsic_nested > 0 && x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-splice-string-intrinsic-call {string_intrinsic_nested} nested body/bodies kept as calls in {cn}.{mn}{desc} at pc={pc}"
                    );
                }
                if splice_loses_string_intrinsic(&site, resolved_string_layout) {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused string-intrinsic-callee {cn}.{mn}{desc} at pc={pc} in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    continue;
                }
                if let Some(l) = nio_charset_layout {
                    let is_nio_intrinsic = |c: &str, m: &str, d: &str| -> bool {
                        (c == "java/nio/ByteBuffer"
                            && m == "wrap"
                            && d == "([BII)Ljava/nio/ByteBuffer;")
                            || (l.hcb_class != 0
                                && l.iso_class != 0
                                && (c == "java/nio/charset/Charset"
                                    || c == "sun/nio/cs/ISO_8859_1")
                                && m == "decode"
                                && d == "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;")
                    };
                    if is_nio_intrinsic(&cn, &mn, &desc) {
                        continue;
                    }
                    fn strip_nio_nested(
                        site: &mut InlineSite,
                        pred: &dyn Fn(&str, &str, &str) -> bool,
                    ) {
                        let nested = std::mem::take(&mut site.nested_sites);
                        let mut kept = Vec::with_capacity(nested.len());
                        for mut n in nested {
                            let drop_here = site
                                .invoke_targets
                                .iter()
                                .find(|(npc, _)| *npc == n.callee_pc)
                                .is_some_and(|(_, t)| {
                                    pred(&t.class_name, &t.method_name, &t.descriptor)
                                });
                            if drop_here {
                                continue;
                            }
                            strip_nio_nested(&mut n.site, pred);
                            if let Some(second) = &mut n.bimorphic_second {
                                strip_nio_nested(&mut second.site, pred);
                            }
                            kept.push(n);
                        }
                        site.nested_sites = kept.into();
                    }
                    strip_nio_nested(&mut site, &is_nio_intrinsic);
                }
                // Round 14 wave 2 (lane syncsplice, SR-1): a `synchronized`
                // body is spliced ONLY as a synchronized splice, registered
                // with the builder before it is appended; any other answer
                // keeps the site its call (the caller-held route or dispatch).
                if site.callee_is_synchronized {
                    // A method with monitor bytecode of its own is left out
                    // whole unless the splice may sit inside its regions
                    // (SS-3, `ir::ir_sync_splice_in_region_enabled`): there
                    // the builder would keep the call
                    // (`IrBuilder::sync_splice_plan`), and a kept call at a
                    // planned pc has lost its direct entry and inline cache.
                    let top_level = pc < code_len
                        && exact_receiver.is_none()
                        && (ir::ir_sync_splice_in_region_enabled()
                            || !bytecode_holds_monitor(code, code_len.min(code.len())));
                    // Round 14 wave 3 (lane sync, SS-2): a static callee with
                    // no caller-held row (its published body self-locks under
                    // `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`, or the row's
                    // other gates refused) takes its class mirror from the
                    // lookup's monitor half instead.
                    if site.callee_is_static
                        && top_level
                        && !builder.sync_splice_row_admits(pc, true)
                        && ir_sync_splice_body_scan(&site).is_ok()
                    {
                        if let Some(t) = req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx)) {
                            if let Some(mirror) =
                                ir_sync_splice_static_mirror(cached.declaring_class_id.as_u32(), &t)
                            {
                                builder.add_sync_splice_mirror(pc, mirror);
                            }
                        }
                    }
                    let refusal = ir_sync_splice_site_refusal(
                        &site,
                        top_level,
                        !site.callee_is_static || builder.sync_splice_static_monitor_known(pc),
                    );
                    if let Some(why) = refusal {
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-splice-refused {why} {cn}.{mn}{desc} \
                                 at pc={pc} in {}.{}{}",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        continue;
                    }
                    builder.add_sync_splice_site(
                        pc,
                        if site.callee_is_static {
                            site.class_id
                        } else {
                            0
                        },
                    );
                }
                // JIT round 23 wave 3d (lane L1): a receiver-carrying direct
                // self-call stays its call. Splicing the method into itself
                // would nest copies whose own self-calls have no row (the
                // bounded self-recursion plan is static-only).
                if ir_instance_self_call_pcs.contains(&pc) {
                    continue;
                }
                let site_self_rec: Option<ir::IrSelfRecursion> = match ir_self_rec.as_ref() {
                    None => None,
                    Some(plan) => match plan.classify(
                        &site.class_name,
                        &site.method_name,
                        &site.descriptor,
                        site.class_id,
                        site.callee_is_static,
                    ) {
                        ir::IrSelfSplice::NotSelf => Some(plan.clone()),
                        ir::IrSelfSplice::Splice(deeper) => Some(deeper),
                        // Only with a zero copy budget: the caller's own call
                        // already IS the direct self-call.
                        ir::IrSelfSplice::Leaf(_) => continue,
                    },
                };
                // Per-site all-or-nothing. `combined` is truncated and the
                // budget restored on refusal, and the site's rows go into a
                // scratch table that is simply dropped — so a body that does
                // not fit leaves the plan exactly as it was, rather than
                // leaving `sites` naming a body with no `field_info`.
                let mark = combined.len();
                let budget_mark = budget;
                // JIT round 20 (lane splice): the rolled-back site's keep-alive
                // rows too, as the nested degrade path already drops its own
                // (`append_ir_inline_site`, `entries_mark`): a pinned callee
                // this artifact never calls only ties its invalidation, and
                // its publication's entry check, to a body it does not hold.
                let entries_mark = ir_direct_callee_entries.len();
                let mut sub = ir::IrInlineTables::default();
                let mut sub_vcalls: Vec<(usize, usize)> = Vec::new();
                let mut sub_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
                // Per-site, and merged only on success, for the same
                // reason `sub` itself is: a rolled-back body must not
                // leave this artifact owing a `<clinit>` for a class its
                // code never reads.
                let mut sub_static_init: Vec<u32> = Vec::new();
                let mut sub_direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
                let mut sub_cycle_cells: Vec<(usize, Arc<JitCycleEdgeCell>)> = Vec::new();
                let mut sub_checkcast_seen = false;
                let mut sub_volatile: Vec<usize> = Vec::new();
                let mut sub_exact_nested: Vec<(usize, u32)> = Vec::new();
                // The own-site ladder's `Unsafe` arm, verbatim in its gates:
                // IR direct calls on, the op's own switch, a wired entry, and
                // the VM's bind admission (the capability question). Counted
                // at the optimizing door like an own-site bind.
                // Wave c: a box call is bound only in a loop-free tree.
                let site_box_calls =
                    ir_splice_box_thin_enabled() && !ir_splice_tree_has_loop(&site);
                let spliced_thin_helper = |cn: &str, mn: &str, desc: &str| -> Option<usize> {
                    if !ir_direct_calls_enabled() {
                        return None;
                    }
                    if cn == "java/lang/invoke/VarHandle" {
                        if let Some(entry) = varhandle_ir_access_entry(
                            mn,
                            desc,
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                        ) {
                            return Some(entry);
                        }
                    }
                    // JIT round 16 (lane callbind): the own-site ladder's
                    // `Integer.valueOf` / `Long.valueOf` / `Integer.intValue`
                    // arms, verbatim in their gates (`direct_native_helper`
                    // asks the same policy question), for a spliced body's
                    // surviving call.
                    if let Some(cell) =
                        ir_splice_box_thin_helper_cell(direct_helpers, cn, mn, desc, site_box_calls)
                    {
                        let entry = direct_native_helper(
                            cell,
                            jdk_only,
                            direct_bind_admission,
                            cn,
                            mn,
                            desc,
                        );
                        return (entry != 0).then_some(entry);
                    }
                    // Strings round 2 (`str17`): `byte[].clone()`, the
                    // whole-array arm of `Arrays.copyOfRange`, which the IR
                    // tier splices into `String.<init>(AbstractStringBuilder,
                    // Void)`: the own-site binds never see it there (the
                    // census read 858 k inline-cache slow paths either way).
                    let clone = byte_array_clone_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        cn,
                        mn,
                        desc,
                    );
                    if clone != 0 {
                        return Some(clone);
                    }
                    // `super.clone()` to `Object.clone` in a spliced body
                    // (`TimeZone.clone` spliced into `ZoneInfo.clone`): asked
                    // for `invokespecial` rows only (`append_ir_inline_site`).
                    let clone = object_clone_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        cn,
                        mn,
                        desc,
                    );
                    if clone != 0 {
                        return Some(clone);
                    }
                    // j27 wave 3 (lane CLONE): an array's virtual `clone()` in a
                    // spliced body (no receiver counts here; an instance site
                    // is bound after the splice from the callee's counts,
                    // `clone_thin_bind::bind_served_spliced_clone_calls`).
                    let clone = clone_thin_bind::virtual_clone_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        None,
                        (cn, mn, desc),
                        "splice",
                    );
                    if clone != 0 {
                        return Some(clone);
                    }
                    // JIT round 21 (lane tl2, `CRATONVM_JIT_IR_SPLICE_TL_THIN`):
                    // the own-site ladder's `ThreadLocal.get()` / `set(Object)`
                    // arms, verbatim in their gates, for a spliced body's
                    // surviving call (the expansion's miss edge, or a site it
                    // refused). Without it the call took the inline-cache
                    // cascade into the generic native funnel (~400 ns a get).
                    // Virtual sites only: `append_ir_inline_site` asks this for
                    // `invokespecial` rows too, and the helper's decline edge
                    // re-dispatches virtually (`thread_local_thin_row`).
                    if let Some(cell) = spliced_thread_local_thin_cell(direct_helpers, cn, mn, desc)
                    {
                        if (jdk_only || mn == "get") && thread_local_get_direct_enabled() {
                            let entry = direct_native_helper(
                                cell,
                                jdk_only,
                                direct_bind_admission,
                                cn,
                                mn,
                                desc,
                            );
                            if entry != 0 {
                                note_thread_local_get_direct_site();
                                return Some(entry);
                            }
                        }
                        return None;
                    }
                    let op =
                        is_unsafe_accessor_op(cn, mn, desc).filter(|op| op.direct_enabled())?;
                    let entry = op.helper_entry(direct_helpers);
                    if entry == 0
                        || !direct_bind_admission.map_or(true, |admits| admits(cn, mn, desc))
                    {
                        return None;
                    }
                    op.note_site(1);
                    Some(entry)
                };
                // Round 13 wave 5 (lane hashsplice): a nested body the append
                // would refuse becomes the call its resolver row describes,
                // BEFORE the tree's relaxation and the site's own fences are
                // judged (`ir_splice_prune_refused_nested`). Not under a
                // self-recursion plan, whose copies and leaves the prune does
                // not model.
                // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2,
                // `CRATONVM_JIT_IR_LAMBDA_IMPL_IN_SPLICE`): a SAM call inside
                // this tree on a lambda its own body built gets the lambda's
                // static impl spliced at the call, BEFORE the prunes and fences
                // below judge the tree (each sees the impl as one more nested
                // body). Withheld with the method-own plan on the door's retry.
                if site_self_rec.is_none()
                    && !lambda_plan_withheld
                    && ir_lambda_impl_call_enabled()
                    && ir_lambda_impl_in_splice_enabled()
                {
                    if let Some(impls) = req.indy_lambda_impl_resolver {
                        let grafted = ir_graft_spliced_lambda_impls(
                            &mut site,
                            impls,
                            ir_resolver,
                            ir_lambda_impl_capturing_enabled(),
                            (
                                &*cached.class_name,
                                &*cached.method_name,
                                &*cached.method_descriptor,
                            ),
                            ir_lambda_impl_loop_slot_enabled(),
                            0,
                        );
                        if grafted > 0 {
                            *lambda_plan_built = true;
                            if ir_stage_reporting() || x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[ir] lambda-impl-in-splice {}.{}{}: {grafted} SAM call(s) \
                                     in the body of {cn}.{mn}{desc} at pc={pc} planned as \
                                     their impl's splice",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                        }
                    }
                }
                // Round nio21 wave 4 (lane L2): a tree with a body admitted
                // with its handlers (`ir::ir_splice_callee_handlers_enabled`)
                // is spliced only where this compile can cover them -- the
                // call-site edges' own gate, plus the chain fences the handler
                // entry resumes through -- and with every nested row at an
                // `invoke` inside a covered `try` range kept as its call.
                let handler_refusal: Option<&'static str> = if ir_site_tree_has_handler_rows(&site)
                {
                    // JIT r23 wave 11 (lane L5): the family's two switches, or
                    // `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLER_TRAPS` alone
                    // (`ir::ir_splice_callee_handler_edges_admitted`).
                    let handlers_ok = ir::ir_splice_callee_handler_edges_admitted()
                        && ir::ir_local_throw_edges_enabled()
                        && ir_chain_fences
                        // JIT round 23 wave 3c (lane L1, L2W3-1;
                        // `CRATONVM_JIT_IR_OSR_SPLICE_CALLEE_HANDLERS`, default
                        // ON): the optimizing OSR route too, where its spliced
                        // throw rows are handed over
                        // (`ir::ir_spliced_throw_edges_enabled`, the same gate
                        // that gives the OSR walk the method's own rows). See
                        // `ir::ir_osr_splice_callee_handlers_enabled`.
                        && ir::ir_splice_callee_handlers_admit_compile(
                            req.osr_mode_exit_polls
                                || req.osr_optimizing_route
                                || req.osr_requested_bci.is_some(),
                        )
                        && !req.debugger_observes_locals
                        // JIT round 23 wave 4 (lane L1): under
                        // `CRATONVM_JIT_IR_SPLICE_HANDLERS_POSTING_MODE` only a
                        // listener armed NOW refuses, not the posting mode
                        // (`ir::ir_splice_callee_handlers_admit_events`).
                        // JIT r23 wave 11 (lane L5): the trap switch takes the
                        // posting-mode rule (`ir::ir_splice_callee_handler_events_admitted`).
                        && ir::ir_splice_callee_handler_events_admitted(
                            req.exception_events_armed_at_compile,
                            req.exception_listener_armed_at_compile,
                        );
                    if !handlers_ok {
                        Some("ir-splice-callee-handlers-not-covered")
                    } else {
                        match ir_splice_strip_handler_covered_nested(&mut site) {
                            Some(dropped) if dropped > 0 && site.nested_sites.is_empty() => {
                                Some("ir-splice-callee-handler-all-nested-stripped")
                            }
                            Some(dropped) => {
                                builder.set_splice_handler_edges(true);
                                if dropped > 0 && x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] ir-splice-handler-covered-nested {dropped} \
                                         row(s) of {cn}.{mn}{desc} at pc={pc} kept as calls"
                                    );
                                }
                                None
                            }
                            None => Some("ir-splice-callee-handler-covered-nested-body"),
                        }
                    }
                } else {
                    None
                };
                // Round rx27 wave 1 (lane chain, `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`):
                // the tree's in-loop rows before this compile's nested strips,
                // for the loop-root rule judged after the append. Not for a
                // synchronized body, whose sync splice row the builder already
                // holds.
                let loop_root_resolved = if pc < code_len
                    && site_self_rec.is_none()
                    && !site.callee_is_synchronized
                    && rx27_loop_root::enabled()
                {
                    rx27_loop_root::resolved_in_loop_rows(&site, ir_fence_flags, ir_chain_fences)
                } else {
                    None
                };
                // Round 13 wave 10 (lane iropt8, HS-2): nested exact-receiver
                // rows this compile does not admit are dropped first, at every
                // depth, so every rule below sees the tree the resolver built
                // before HS-2 (each such pc keeps the call row it always had).
                // JIT round 20 wave 2 (lane splice): the nested hot cap only
                // under a top-level site the plan calls hot.
                if !site.nested_sites.is_empty()
                    && !(pc < code_len && ir_guarded_site_is_hot(ir_ranked.as_ref(), profile, pc))
                    // JIT r25 wave 10 (W10-3b): a body whose own loop runs a
                    // chain link is hot for its nested rows however often the
                    // top-level site itself ran.
                    && !r25_w10_3b::tree_has_loop_rooted_chain(&site)
                {
                    let dropped = ir_splice_strip_cold_nested_hot_rows(&mut site);
                    if dropped > 0 && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-nested-hot-cap-cold-site {dropped} bod{} of \
                             {cn}.{mn}{desc} at pc={pc} in {}.{}{} (site not hot: kept as calls)",
                            if dropped == 1 { "y" } else { "ies" },
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                }
                if !site.nested_sites.is_empty() {
                    ir_splice_strip_unadmitted_exact_nested(&mut site, ir_chain_fences);
                }
                // JIT r25 wave 7 (W7-B fix): the chain links' cost rule (loop
                // rooted, tree byte cap); a no-op with the switch off.
                if !site.nested_sites.is_empty()
                    && r25_w7b_chain_splice::ir_nested_chain_splice_enabled()
                {
                    let top_rooted = pc < code_len && bytecode_pc_in_loop(code, code_len, pc);
                    let mut keys: Vec<String> = Vec::new();
                    let report =
                        r25_w7b_chain_splice::strip_chain_tree(&mut site, top_rooted, &mut keys);
                    ir_chain_link_keys.extend(keys);
                    ir_chain_census.note(&report);
                    // W8-5: printed only for a tree the rule judged a link of.
                    if x64::dbg_jitc_enabled()
                        && report.kept + report.unrooted + report.capped + report.costly > 0
                    {
                        eprintln!(
                            "[cratonvm-jitc] ir-chain-tree {cn}.{mn}{desc} at pc={pc} in {}.{}{}: \
                             kept={} unrooted={} capped={} costly={} link_bytes={} \
                             added_locals={} max_depth={} top_rooted={top_rooted} \
                             min_entry_permille={} est_frame_before={} est_frame_after={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            report.kept,
                            report.unrooted,
                            report.capped,
                            report.costly,
                            report.bytes,
                            report.added_locals,
                            report.max_depth,
                            report
                                .min_entry_permille
                                .map_or("-".to_string(), |m| m.to_string()),
                            report.est_frame_before,
                            report.est_frame_after,
                        );
                    }
                }
                // nio20 wave 4 (lane L5): a NESTED String search /
                // `StringBuilder.append(C)` site the inline prefix answers keeps
                // its call, as the top-level carve-out below does for the
                // compiling method's own sites.
                if !site.nested_sites.is_empty() {
                    let dropped = ir_splice_strip_string_prefix_nested(&mut site);
                    if dropped > 0 && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-nested-string-prefix {dropped} row(s) of \
                             {cn}.{mn}{desc} at pc={pc} kept as calls"
                        );
                    }
                }
                // Round nio20 wave 4 (lane L3, W3L4-4): a nested trap row whose
                // own named speculation the chain sink withdrew at this call
                // site keeps its call; the rest of the splice stays.
                if !site.nested_sites.is_empty() && ir_nested_trap_own_charge_enabled() {
                    if let (Some(registry), Some(key)) =
                        (req.despec.map(|r| &**r), ir_despec_splice_key.as_deref())
                    {
                        let dropped =
                            ir_splice_strip_withdrawn_nested_traps(&mut site, registry, key, pc);
                        if dropped > 0 && x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-splice-nested-trap-withdrawn {dropped} row(s) \
                                 of {cn}.{mn}{desc} at pc={pc} in {key} (kept as calls)"
                            );
                        }
                    }
                }
                // Round 13 wave 8 (lane iropt7): under chain fences a large
                // nested body behind a branch of its enclosing body stays its
                // call (`ir_splice_prune_cold_nested`), before the refusal
                // prune below sees the tree.
                if ir_chain_fences && ir_self_rec.is_none() && !site.nested_sites.is_empty() {
                    let top_in_loop = pc < code_len && bytecode_pc_in_loop(code, code_len, pc);
                    let dropped = ir_splice_prune_cold_nested_in(&mut site, top_in_loop);
                    if dropped > 0 && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-cold-nested {dropped} bod{} of \
                             {cn}.{mn}{desc} at pc={pc} in {}.{}{} (kept as calls)",
                            if dropped == 1 { "y" } else { "ies" },
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                }
                if ir_self_rec.is_none() && !site.nested_sites.is_empty() {
                    let dropped =
                        ir_splice_prune_refused_nested(&mut site, ir_fence_flags, ir_chain_fences);
                    if dropped > 0 && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-nested-pruned {dropped} bod{} of \
                             {cn}.{mn}{desc} at pc={pc} in {}.{}{} (kept as calls)",
                            if dropped == 1 { "y" } else { "ies" },
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                }
                // Round 12 wave 5 (lane iropt4): the replay-fence relaxations
                // apply to this site's whole tree or not at all (see
                // `ir_splice_tree_admits_fence_relaxation`).
                let fence_relax = if ir_splice_tree_admits_fence_relaxation(&site) {
                    ir_fence_flags
                } else {
                    ir::SpliceFenceRelax::NONE
                };
                // Round 12 wave 6 (lane iropt5, W5-1): a body whose committed
                // store the builder would fence into refusing the METHOD is
                // refused here as a SITE. Only the outermost body is walked.
                // Round 13 wave 3 (lane framestate, W5-3 stage 3): the store
                // fence is the builder's alone under chain fences (see
                // `append_ir_inline_site`).
                let store_fenced = ir_plan_store_fence
                    && !ir_chain_fences
                    && ir_splice_store_fence_refuses_site(&site, ir_committed_store_admitted);
                // Round rx27 wave 1 (lane chain): and after them.
                let loop_root_counts = loop_root_resolved
                    .map(|resolved| (resolved, rx27_loop_root::in_loop_rows(&site)));
                let cycle_cell_admitter = |target: &InlineInvokeTarget| {
                    ir_splice_static_cell_candidate(target)
                        && helpers.self_call_stack_guard != 0
                        && helpers.invoke_dispatch != 0
                        && !verdicts.direct_call_requires_dispatch_of(
                            &target.class_name,
                            &target.method_name,
                            &target.descriptor,
                            || Some(cratonvm_types::ClassId::new(target.target_class_id)),
                        )
                        && req.cycle_edge_admission.is_some_and(|admit| {
                            admit(
                                target.declaring_class_id,
                                &target.class_name,
                                &target.method_name,
                                &target.descriptor,
                            )
                        })
                };
                // The ordinary/wide budget can be exhausted by small helpers
                // before a later covered decoder body is visited. Give only
                // compiles that actually contain an admitted covered nested row
                // the existing maximum budget. Refusal below restores
                // `budget_mark`.
                if handler_refusal.is_none()
                    && !store_fenced
                    && ir::ir_splice_covered_nested_guarded_enabled()
                    && ir::ir_splice_handler_lower_proven_enabled()
                    && true
                    && ir_site_tree_has_covered_nested_candidate(&site, false)
                {
                    let raised_to = 8 * IR_INLINE_MAX_TOTAL_BYTES;
                    if budget < raised_to {
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] covered-nested-budget {} -> {} for {cn}.{mn}{desc} at pc={pc}",
                                budget, raised_to,
                            );
                        }
                        budget = raised_to;
                    }
                }
                let budget_spend_mark = budget;
                let appended = if let Some(why) = handler_refusal {
                    Err(why)
                } else if store_fenced {
                    Err("ir-splice-trap-after-store")
                } else {
                    append_ir_inline_site(
                        site,
                        pc,
                        &mut combined,
                        &mut sub,
                        &mut ir_call_strings,
                        &mut ir_call_infos,
                        &mut ir_direct_callee_entries,
                        &mut budget,
                        &mut sub_vcalls,
                        &mut sub_compact,
                        ir_emit_long,
                        ir_emit_fp,
                        &mut sub_static_init,
                        &mut sub_direct_calls,
                        &mut sub_checkcast_seen,
                        &mut sub_volatile,
                        &mut sub_cycle_cells,
                        Some(&cycle_cell_admitter),
                        &mut sub_exact_nested,
                        site_self_rec.as_ref(),
                        &spliced_thin_helper,
                        fence_relax,
                        ir_chain_fences,
                    )
                };
                // Round 14 wave 2 (lane guard, GS-1): a bimorphic site's second
                // body, appended right after the first (so the site's one
                // spliced range covers both) into its own table, whose row for
                // `pc` is re-keyed under `ir::bimorphic_second_site_key` (the
                // site table holds one body per caller pc). Same per-site
                // scratch outputs, so a refusal of either body rolls the whole
                // site back below.
                let mut bimorphic_second_class: Option<u32> = None;
                // Round 16 (lane mega): the polymorphic site's third and later
                // classes whose bodies were appended, in test order.
                let mut polymorphic_extra_classes: Vec<u32> = Vec::new();
                let appended = match (appended, ir_bimorphic_second.take()) {
                    (Ok(()), Some((second_site, second_class))) => {
                        let mut sub_second = ir::IrInlineTables::default();
                        let second = match ir_bimorphic_prepare_second_body(
                            second_site,
                            ir_fence_flags,
                            ir_chain_fences,
                            ir_plan_store_fence,
                            ir_committed_store_admitted,
                        ) {
                            Ok((second_site, second_relax)) => append_ir_inline_site(
                                second_site,
                                pc,
                                &mut combined,
                                &mut sub_second,
                                &mut ir_call_strings,
                                &mut ir_call_infos,
                                &mut ir_direct_callee_entries,
                                &mut budget,
                                &mut sub_vcalls,
                                &mut sub_compact,
                                ir_emit_long,
                                ir_emit_fp,
                                &mut sub_static_init,
                                &mut sub_direct_calls,
                                &mut sub_checkcast_seen,
                                &mut sub_volatile,
                                &mut sub_cycle_cells,
                                Some(&cycle_cell_admitter),
                                &mut sub_exact_nested,
                                None,
                                &spliced_thin_helper,
                                second_relax,
                                ir_chain_fences,
                            ),
                            Err(why) => Err(why),
                        };
                        if second.is_ok() {
                            if let Some(row) = sub_second.sites.remove(&pc) {
                                sub_second
                                    .sites
                                    .insert(ir::bimorphic_second_site_key(pc), row);
                            }
                            merge_ir_inline_tables(&mut sub, sub_second);
                            bimorphic_second_class = Some(second_class);
                        }
                        // Round 16 (lane mega): a polymorphic site's further
                        // bodies, appended after the second the same way, each
                        // re-keyed under `ir::polymorphic_site_key(pc, k)`. A
                        // refusal of any rolls the whole site back below, as
                        // the second body's does.
                        let mut second = second;
                        if second.is_ok() {
                            let extras = std::mem::take(&mut ir_polymorphic_extra);
                            for (k, (extra_site, extra_class)) in extras.into_iter().enumerate() {
                                let mut sub_extra = ir::IrInlineTables::default();
                                let extra = match ir_bimorphic_prepare_second_body(
                                    extra_site,
                                    ir_fence_flags,
                                    ir_chain_fences,
                                    ir_plan_store_fence,
                                    ir_committed_store_admitted,
                                ) {
                                    Ok((extra_site, extra_relax)) => append_ir_inline_site(
                                        extra_site,
                                        pc,
                                        &mut combined,
                                        &mut sub_extra,
                                        &mut ir_call_strings,
                                        &mut ir_call_infos,
                                        &mut ir_direct_callee_entries,
                                        &mut budget,
                                        &mut sub_vcalls,
                                        &mut sub_compact,
                                        ir_emit_long,
                                        ir_emit_fp,
                                        &mut sub_static_init,
                                        &mut sub_direct_calls,
                                        &mut sub_checkcast_seen,
                                        &mut sub_volatile,
                                        &mut sub_cycle_cells,
                                        Some(&cycle_cell_admitter),
                                        &mut sub_exact_nested,
                                        None,
                                        &spliced_thin_helper,
                                        extra_relax,
                                        ir_chain_fences,
                                    ),
                                    Err(why) => Err(why),
                                };
                                if extra.is_err() {
                                    second = extra;
                                    break;
                                }
                                if let Some(row) = sub_extra.sites.remove(&pc) {
                                    sub_extra
                                        .sites
                                        .insert(ir::polymorphic_site_key(pc, k + 2), row);
                                }
                                merge_ir_inline_tables(&mut sub, sub_extra);
                                polymorphic_extra_classes.push(extra_class);
                            }
                        }
                        second
                    }
                    (appended, _) => appended,
                };
                // Round 12 wave 5 (lane iropt4, proposal W4-4): say which
                // splice rule refused the site. The resolver's own refusals
                // print as `inline-resolve REFUSED`; these come after it.
                if let Err(why) = appended {
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-refused {why} {cn}.{mn}{desc} at pc={pc} \
                             in {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                }
                let mut ok = appended.is_ok();
                // Round rx27 wave 1 (lane chain, `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`):
                // a loop-rooted tree whose in-loop rows this compile could not
                // hold is rolled back below (the else arm) and stays a call,
                // compiled as its own root (`rx27_loop_root`). One-class sites
                // only.
                if ok && bimorphic_second_class.is_none() && polymorphic_extra_classes.is_empty() {
                    if let Some((resolved, planned)) = loop_root_counts {
                        let spliced =
                            rx27_loop_root::spliced_in_loop_rows(&sub.sites, &combined, pc);
                        let kept_call = rx27_loop_root::keeps_call(
                            resolved,
                            planned,
                            spliced,
                            rx27_loop_root::strips_counted(),
                        );
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "{}",
                                rx27_loop_root::census_line(
                                    &format!("{cn}.{mn}{desc}"),
                                    pc,
                                    &format!(
                                        "{}.{}{}",
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor
                                    ),
                                    resolved,
                                    planned,
                                    spliced,
                                    budget,
                                    kept_call,
                                )
                            );
                        }
                        ok = !kept_call;
                    }
                }
                // JIT round 20 wave 11 (lane pSplice, NIO20-L5-2's first
                // step): the bytes each planned site spent (its body and every
                // nested body), its rank and what is left, in visit order.
                if ok && x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-splice-bytes {cn}.{mn}{desc} at pc={pc} rank={:?} \
                         bytes={} left={budget} site#{} in {}.{}{}",
                        ir_ranked.as_ref().and_then(|r| r.class_of(pc)),
                        budget_spend_mark.saturating_sub(budget),
                        sites_planned + 1,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                    );
                }
                if ok {
                    if let Some(class_id) = site_factory_class {
                        let local = ir_factory_result_local(code, code_len, pc, opcode);
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir-factory-site {cn}.{mn}{desc} at pc={pc} in {}.{}{}: \
                                 allocates class id {class_id}, result local {local:?}",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        if let Some(k) = local {
                            ir_factory_locals.insert(k, class_id);
                            ir_factory_site_pcs.insert(pc);
                        }
                    }
                    merge_ir_inline_tables(&mut tables, sub);
                    if let Some(class_id) = exact_receiver {
                        ir_exact_sites.push((pc, class_id));
                        if site_cha_watermark != 0 && pc < code_len {
                            ir_cha_watermarks.insert(pc, site_cha_watermark);
                        }
                        // j27 wave 5 (lane PROXY): the builder judges the set
                        // (`IrBuilder::add_cha_class_set_site`).
                        if !site_cha_class_set.is_empty() && pc < code_len {
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-cha-class-set {cn}.{mn}{desc} at pc={pc} \
                                     in {}.{}{}: class ids {site_cha_class_set:?}",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                            ir_cha_class_sets.insert(pc, site_cha_class_set.clone());
                        }
                        // nio21 wave 6, lane L4: the builder's guarded plan
                        // takes this body under the hot cap.
                        if site_guarded_hot {
                            ir_guarded_hot_pcs.insert(pc);
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-guarded-hot-cap {cn}.{mn}{desc} at pc={pc} \
                                     in {}.{}{}: class id {class_id}",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                        }
                        if site_is_buffer && bimorphic_second_class.is_none() {
                            ir_buffer_site_pcs.insert(pc);
                            if site_is_buffer_put {
                                ir_buffer_put_site_pcs.insert(pc);
                            }
                        }
                    }
                    if let Some(second_class) = bimorphic_second_class {
                        ir_bimorphic_sites.push((pc, second_class));
                    }
                    if !polymorphic_extra_classes.is_empty() {
                        ir_polymorphic_sites.push((pc, polymorphic_extra_classes));
                    }
                    if !sub_exact_nested.is_empty() && x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-splice-nested-exact {} bod{} of {cn}.{mn}{desc} \
                             at pc={pc} in {}.{}{} (spliced where the receiver is proven)",
                            sub_exact_nested.len(),
                            if sub_exact_nested.len() == 1 {
                                "y"
                            } else {
                                "ies"
                            },
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                        );
                    }
                    ir_exact_nested_sites.append(&mut sub_exact_nested);
                    // The spliced bodies' volatile reads, merged only on
                    // success like every other per-site row.
                    ir_spliced_volatile_pcs.extend(sub_volatile);
                    // A spliced `checkcast` obliges the artifact exactly as
                    // one of the caller's own does. Merged only on success,
                    // like every other row: a rolled-back body must not
                    // leave the artifact carrying `has_dispatch` for a cast
                    // its code does not contain.
                    ir_needs_dispatch_for_checkcast |= sub_checkcast_seen;
                    // The surviving statically-bound calls in the spliced
                    // bodies, at their combined-buffer pcs. Merged only on
                    // success, and keyed past `code_len`, so the spliced-pc
                    // sweep below -- which drops the caller's own row at
                    // every pc it spliced OVER -- cannot reach them.
                    // j27 wave 2 (lane TCARRAY, `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE`):
                    // a spliced `ThreadLocal` thin row whose callee profile
                    // names an overriding class goes back to its inline cache.
                    thread_local_thin_bind::unbind_refused_spliced_thin_calls(
                        direct_helpers,
                        &mut sub_direct_calls,
                        &mut sub_vcalls,
                        &tables.spliced_receiver_seeds,
                        &tables.invoke_info,
                    );
                    // j27 wave 4 (lane CLONE2, `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND`):
                    // a spliced instance `clone()` whose callee profile names a
                    // served class takes the thin helper, as at an own site.
                    if ir_direct_calls_enabled() {
                        clone_thin_bind::bind_served_spliced_clone_calls(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &mut sub_direct_calls,
                            &mut sub_vcalls,
                            &tables.spliced_receiver_seeds,
                            &tables.invoke_info,
                        );
                        // REFLECT2-21-2 Residual 2: bind spliced Integer.valueOf / Long.valueOf
                        // calls to thin helpers so Lowerer inline box lowering engages.
                        {
                            for (&pc, &(info_addr, _, _)) in &tables.invoke_info {
                                if pc >= code_len && !sub_direct_calls.iter().any(|(p, _)| *p == pc)
                                {
                                    // SAFETY: info_addr is an owned JitInvokeInfo for this compile.
                                    let info = unsafe { &*(info_addr as *const JitInvokeInfo) };
                                    if info.invoke_kind == 3 {
                                        if info.class_name == "java/lang/Integer"
                                            && info.method_name == "valueOf"
                                            && info.descriptor == "(I)Ljava/lang/Integer;"
                                        {
                                            let entry = direct_native_helper(
                                                direct_helpers.integer_value_of,
                                                jdk_only,
                                                direct_bind_admission,
                                                &info.class_name,
                                                &info.method_name,
                                                &info.descriptor,
                                            );
                                            if entry != 0 {
                                                sub_direct_calls.push((pc, (entry, true)));
                                            }
                                        } else if long_box_direct_helpers_enabled()
                                            && info.class_name == "java/lang/Long"
                                            && info.method_name == "valueOf"
                                            && info.descriptor == "(J)Ljava/lang/Long;"
                                        {
                                            let entry = direct_native_helper(
                                                direct_helpers.long_value_of,
                                                jdk_only,
                                                direct_bind_admission,
                                                &info.class_name,
                                                &info.method_name,
                                                &info.descriptor,
                                            );
                                            if entry != 0 {
                                                sub_direct_calls.push((pc, (entry, true)));
                                            }
                                        } else if info.class_name == "java/lang/Thread"
                                            && info.descriptor == "()Ljava/lang/Thread;"
                                        {
                                            let cell = if info.method_name == "currentThread" {
                                                direct_helpers.thread_current_thread
                                            } else if info.method_name == "currentCarrierThread"
                                                && direct_helpers::carrier_thread_direct_enabled()
                                            {
                                                direct_helpers.thread_current_carrier_thread
                                            } else {
                                                0
                                            };
                                            let entry = direct_native_helper(
                                                cell,
                                                jdk_only,
                                                direct_bind_admission,
                                                &info.class_name,
                                                &info.method_name,
                                                &info.descriptor,
                                            );
                                            if entry != 0 {
                                                sub_direct_calls.push((pc, (entry, true)));
                                            }
                                        } else {
                                            let entry = system_clock_direct_helper(
                                                direct_helpers,
                                                jdk_only,
                                                direct_bind_admission,
                                                &info.class_name,
                                                &info.method_name,
                                                &info.descriptor,
                                            );
                                            if entry != 0 {
                                                sub_direct_calls.push((pc, (entry, true)));
                                            }
                                        }
                                    } else if (info.invoke_kind == 0 || info.invoke_kind == 1)
                                        && info.class_name == "java/lang/Object"
                                        && info.descriptor == "()V"
                                    {
                                        let cell = match info.method_name {
                                            "notify" => direct_helpers.object_notify,
                                            "notifyAll" => direct_helpers.object_notify_all,
                                            _ => 0,
                                        };
                                        let entry = direct_native_helper(
                                            cell,
                                            jdk_only,
                                            direct_bind_admission,
                                            "java/lang/Object",
                                            info.method_name,
                                            "()V",
                                        );
                                        if entry != 0 {
                                            sub_direct_calls.push((pc, (entry, true)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    ir_direct_calls.extend(sub_direct_calls);
                    for (cell_pc, cell) in sub_cycle_cells {
                        ir_cycle_cells.insert(cell_pc, Arc::as_ptr(&cell) as usize);
                        ir_owned_cycle_cells.push(cell);
                    }
                    // The ensure-init obligation the spliced statics add.
                    // Sorted and deduped where the vector is consumed.
                    ir_static_init_classes.append(&mut sub_static_init);
                    // Same gate the caller's own rows are behind, so
                    // the spliced bodies and the method around them cannot
                    // disagree about whether compact offsets are in play.
                    if cratonvm_types::compact_ref_fields_enabled() {
                        ir_compact_fields.extend(sub_compact);
                    }
                    // One inline cache per surviving virtual call. Keyed by
                    // the call's own combined-buffer pc, which is why the
                    // spliced nodes keep those pcs — see `ir::IrInlineSite`.
                    // No profile seed: receiver counts are recorded against
                    // the bci of the method that was EXECUTING, and a
                    // relocated pc is not one of those. The first dispatch
                    // rings the helper, which installs the target, and the
                    // guard hits from then on.
                    //
                    // JIT round 19 proposal wave 4 (lane megaosr4,
                    // MEGAOSR19-2, `CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED`):
                    // the CALLEE's own profile does hold counts for that bci,
                    // and the VM resolver carried them on the body
                    // (`InlineSite::callee_receiver_profile`), re-keyed at the
                    // combined pc in `tables.spliced_receiver_seeds`. With the
                    // switch on, the site is seeded from them as the caller's
                    // own sites are seeded from the caller's profile.
                    let seed_from_callee = ir_splice_callee_profile_seed_enabled();
                    for (vpc, num_args) in sub_vcalls {
                        if helpers.invoke_virtual_mic != 0
                            && crate::ir_lower::ir_ic_admits_arity(num_args)
                        {
                            let mic = Box::new(JitMICSlot::new_at(vpc));
                            let pic = Box::new(JitPICSlot::new_at(vpc));
                            // JIT round 21, lane dispatch: as the caller's own
                            // sites (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
                            if inline_cache_pic::ic_mic_plus_four_enabled() {
                                pic.note_mic_read_first();
                            }
                            if seed_from_callee {
                                let seed = tables.spliced_receiver_seeds.get(&vpc);
                                let outcome = ir_seed_spliced_inline_cache(&mic, &pic, seed);
                                if x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] ir-splice-callee-seed {} at combined pc={vpc} \
                                         (spliced at pc={pc}, {cn}.{mn}{desc}) in {}.{}{}: classes={} \
                                         dominant={} megamorphic={}",
                                        if seed.is_some() { "SEEDED" } else { "NO-PROFILE" },
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor,
                                        outcome.classes,
                                        outcome.dominant.unwrap_or(0),
                                        outcome.megamorphic,
                                    );
                                }
                            } else {
                                pic.seed_from_mic(&mic, false);
                            }
                            // Round 12 wave 6 (lane mega5, W5-6), applied in
                            // wave 7 (lane mega6): as the caller's own sites.
                            if crate::direct_jit_callee_calls_enabled() {
                                pic.note_inline_ways_probed();
                            }
                            let mic_addr = &*mic as *const JitMICSlot as usize;
                            let pic_addr = &*pic as *const JitPICSlot as usize;
                            ir_mic_boxes.push(mic);
                            ir_pic_boxes.push(pic);
                            ir_ic_slots.insert(vpc, (mic_addr, pic_addr));
                        }
                    }
                    ir_spliced_ranges.push((mark, combined.len(), pc));
                    sites_planned += 1;
                } else {
                    combined.truncate(mark);
                    budget = budget_mark;
                    ir_direct_callee_entries.truncate(entries_mark);
                }
            }
            if sites_planned > 0 {
                // The call at a spliced pc is not emitted, so its
                // direct-call entry and its inline-cache pair describe
                // nothing. Dropping them keeps `_direct_callee_entries` —
                // which `prepare_for_publication` pins and refuses to publish
                // an unresolvable entry from — free of targets this artifact
                // never calls.
                //
                // JIT round 21 lane mcs (routing fix, unconditional; page
                // `jr21-mcs-guarded-indy-call-lost-its-direct-row-20261014.md`):
                // except at a guarded `MutableCallSite` / `VolatileCallSite`
                // indy pc. Its `0xba` arm builds `guard ? Call : bridge`
                // (`IrBuilder::try_indy_mutable_guarded_call`), so the Call IS
                // emitted even though the planner registered a body at that
                // pc; without its row it was lowered to the blind
                // `jit_invoke_dispatch` (~170 ns per guard hit, the bridge's
                // cost), and a virtual target lost its inline cache.
                let ir_indy_guard_pcs = ir_indy_direct.mutable_guards();
                for &pc in tables.sites.keys() {
                    if pc < code_len
                        && !ir_exact_sites.iter().any(|&(p, _)| p == pc)
                        && !ir_indy_guard_pcs.contains_key(&pc)
                    {
                        ir_direct_calls.remove(&pc);
                        ir_ic_slots.remove(&pc);
                        // An owned cell nothing bakes is harmless.
                        ir_cycle_cells.remove(&pc);
                    }
                }
                // `resume_bci` scans these linearly and assumes increasing
                // starts. They are pushed in plan order, which is already
                // increasing, but sorting makes that a property of the data
                // rather than of the loop above.
                ir_spliced_ranges.sort_unstable();
                // Whether a whole-method replay may re-run this artifact's
                // abandoned attempt turns on what that attempt could have
                // committed, and half of that is the relocated bodies. Ask
                // the same predicate the interpreter's sink asks of the
                // caller's own bytecode, over each spliced region.
                ir_spliced_bodies_pure = ir_spliced_ranges
                    .iter()
                    .all(|&(s, e, _)| !bytecode_commits_side_effect(&combined[s..e], e - s));
                if ir_stage_reporting() {
                    eprintln!(
                        "[ir] inline-plan {}.{}{}: {} site(s), {} spliced bod{}, {} bytes appended",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        sites_planned,
                        tables.sites.len(),
                        if tables.sites.len() == 1 { "y" } else { "ies" },
                        combined.len() - code_len,
                    );
                }
                // Two sentinel bytes, exactly as a method's own bytecode
                // carries: the last relocated body ends in a 1-byte return,
                // but a reader that peeks past the final instruction must
                // land on zeroes rather than on whatever follows the Vec.
                combined.push(0);
                combined.push(0);
                // BEFORE the move: `apply_inline_tables` consumes
                // `tables`, and the resolver needs the same `sites` map.
                ir_inline_frame_sites = ir::IrInlineFrameSites::from_sites(&tables.sites)
                    .with_original_bodies(&tables.original_body_codes)
                    // JIT round 21 wave 3 (lane keepset): the chain-snapshot
                    // liveness models the admitted callee handlers' edges.
                    .with_handler_rows(&tables.splice_handler_rows)
                    .with_original_handler_rows(&tables.original_body_handler_rows);
                // The same condition as `scope_method_key` below: no chain
                // snapshot, no chain, no reader.
                if (ir::ir_splice_frame_states_enabled() || ir::ir_post_call_splice_exits_enabled())
                    && ir_sync_chains_ok
                {
                    ir_splice_scope_class_ids = ir::splice_scope_class_ids(&tables.sites);
                    // After every resolver read of this plan: kept only when
                    // no redefinition began since the compile's stamp.
                    if chain_inner_own_source_enabled() && splice_sources_read_at_compile_stamp() {
                        ir_splice_scope_sources = splice_scope_sources(
                            &tables.sites,
                            &combined,
                            &tables.original_body_codes,
                            &tables.original_body_handler_rows,
                        );
                    }
                    // t1 ARGFIX item 2: only chain snapshots read it.
                    if ir_lower::ir_keep_set_splice_local_liveness_enabled() {
                        ir_spliced_body_codes = spliced_body_codes(&tables, &combined);
                    }
                }
                // JIT round 16 wave 7 (lane callbind): a spliced body's
                // surviving `intValue` / `longValue` gets the own-site unbox
                // row (`CRATONVM_JIT_IR_SPLICE_UNBOX_INTRINSIC`, default off).
                // The class ids are asked only under the switch.
                if ir_splice_unbox_intrinsic_enabled() {
                    let added = ir_plan_spliced_unbox_intrinsics(
                        &mut tables,
                        code_len,
                        direct_helpers.integer_class_id(),
                        crate::ir_lower::ir_long_class_id(direct_helpers),
                    );
                    if added > 0 && ir_stage_reporting() {
                        eprintln!(
                            "[ir] splice-unbox-intrinsics {}.{}{}: {} spliced site(s) planned as a guarded field load",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            added,
                        );
                    }
                }
                // nio21 wave 1, lane L5 (L3W6-1): a spliced body's surviving
                // scalar-family static call (`Surrogate$Parser.parse`'s
                // `Character.isHighSurrogate` inside `encodeBufferLoop`) is the
                // own-site `Op::ScalarIntrinsic`, not a call.
                let scalar_rows = ir_plan_spliced_scalar_intrinsics(&tables, code_len);
                if !scalar_rows.is_empty() {
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] splice-scalar-intrinsics {}.{}{}: {} spliced site(s) lowered as arithmetic",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            scalar_rows.len(),
                        );
                    }
                    builder.add_scalar_intrinsics(scalar_rows);
                }
                let scoped_added = ir_plan_spliced_scoped_raw(&mut tables, code_len);
                if scoped_added > 0 && ir_stage_reporting() {
                    eprintln!(
                        "[ir] splice-scoped-raw {}.{}{}: {} spliced ScopedMemoryAccess site(s) planned as a raw access",
                        cached.class_name, cached.method_name, cached.method_descriptor, scoped_added,
                    );
                }
                let vh_added =
                    ir_plan_spliced_vh_field_reads(req, &mut tables, code_len, &mut builder);
                if vh_added > 0 && ir_stage_reporting() {
                    eprintln!(
                        "[ir] splice-vh-field-reads {}.{}{}: {} spliced VarHandle read site(s) planned as a guarded field load",
                        cached.class_name, cached.method_name, cached.method_descriptor, vh_added,
                    );
                }
                // JIT round 19 wave 3 (lane splice3, SPLICE2-2): the spliced
                // guards the de-spec registry names as expected throws; only
                // a chain guard can be marked, so only with chain snapshots.
                if ir::ir_splice_expected_throw_enabled() && ir::ir_splice_frame_states_enabled() {
                    if let Some(registry) = req.despec {
                        builder.graph.splice_expected_throw = ir::splice_expected_throw_rows(
                            registry,
                            &tables.sites,
                            &combined,
                            &tables.original_body_codes,
                        );
                        if x64::dbg_jitc_enabled()
                            && !builder.graph.splice_expected_throw.is_empty()
                        {
                            eprintln!(
                                "[cratonvm-jitc] ir-splice-expected-throw {}.{}{}: {} guard(s) [{}]",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                builder.graph.splice_expected_throw.len(),
                                builder
                                    .graph
                                    .splice_expected_throw
                                    .iter()
                                    .map(|(k, b, r)| format!("{k}@{b}:{r:?}"))
                                    .collect::<Vec<_>>()
                                    .join(","),
                            );
                        }
                    }
                }
                // Round nio20 wave 6 (lane L1): and every spliced body's
                // (`getstatic SCOPED_MEMORY_ACCESS` in each buffer access).
                builder.add_plain_static_reads(ir_plain_static_read_rows(
                    direct_helpers,
                    tables.static_field_info.values(),
                ));
                // Extend initialized static classes with spliced bodies' resolved rows.
                builder.add_static_initialized_classes(ir_spliced_initialized_static_classes(
                    direct_helpers,
                    tables.static_field_info.values(),
                ));
                // JIT round 20 (lane splice): the nested guarded rows the VM
                // admitted over the cold cap (`ir_nested_guarded_body_cap`),
                // within the hot-site count the top-level sites left.
                // JIT r25 wave 7 (W7-B fix): the kept chain links take slots
                // of their own; every other row the slots it always had.
                let ir_nested_hot_pcs = r25_w7b_chain_splice::nested_hot_cap_pcs(
                    &ir_exact_nested_sites,
                    |key| {
                        tables
                            .sites
                            .get(&key)
                            .map(|s| (s.code_len, ir_chain_link_keys.contains(&s.method_key)))
                    },
                    GUARDED_SPLICE_HOT_MAX_SITES.saturating_sub(ir_guarded_hot_pcs.len()),
                );
                if x64::dbg_jitc_enabled() {
                    for &hot_pc in &ir_nested_hot_pcs {
                        if let Some(body) = tables.sites.get(&hot_pc) {
                            eprintln!(
                                "[cratonvm-jitc] ir-nested-guarded-hot-cap body {} ({} bytes) at \
                                 combined pc={hot_pc} in {}.{}{}",
                                body.method_key,
                                body.code_len,
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                            );
                        }
                    }
                }
                // JIT r20 wave 19 (lane zLdc): a spliced String / Class `ldc`
                // row lowers to its slot probe with the shared helper as the
                // miss arm. A hand-built helper table leaves the helper 0;
                // then the row is dropped, exactly as the caller's own site is
                // omitted above, and the builder bails the method at the pc
                // rather than plan a CALL to address 0.
                if helpers.ldc_string_cp == 0 {
                    tables.ldc_ref_string.clear();
                }
                if helpers.ldc_class_cp == 0 {
                    tables.ldc_ref_class.clear();
                }
                // JIT round 23 wave 4 (lane L1, CF23-2;
                // `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`): the spliced
                // never-taken branches whose chain-framed guard has not been
                // withdrawn at its outermost call site (the VM's chain sinks
                // record `chain_predicate_speculation_id` there).
                if !tables.spliced_never_taken.is_empty() {
                    let method_key = format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                    let branches = splice_never_taken_admitted(
                        &tables.spliced_never_taken,
                        &ir_spliced_ranges,
                        req.despec.map(|r| &**r),
                        &method_key,
                    );
                    let dead: std::collections::HashSet<usize> =
                        tables.spliced_never_taken_dead.iter().copied().collect();
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] spliced never-taken rows in {method_key}: {} admitted of {}",
                            branches.len(),
                            tables.spliced_never_taken.len()
                        );
                    }
                    builder.set_splice_never_taken(branches, dead);
                }
                if !tables.spliced_always_taken.is_empty() {
                    let method_key = format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                    let branches = splice_never_taken_admitted(
                        &tables.spliced_always_taken,
                        &ir_spliced_ranges,
                        req.despec.map(|r| &**r),
                        &method_key,
                    );
                    let dead: std::collections::HashSet<usize> =
                        tables.spliced_always_taken_dead.iter().copied().collect();
                    builder.set_splice_always_taken(branches, dead);
                }
                if !tables.spliced_pruned_switch_edges.is_empty() {
                    let method_key = format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                    let edges = splice_pruned_switch_edges_admitted(
                        &tables.spliced_pruned_switch_edges,
                        &ir_spliced_ranges,
                        req.despec.map(|r| &**r),
                        &method_key,
                    );
                    let dead: std::collections::HashSet<usize> =
                        tables.spliced_pruned_switch_dead.iter().copied().collect();
                    builder.set_splice_pruned_switch_edges(edges, dead);
                }
                // JIT round 23 wave 8 (lane W8-L2, CF-W8-3;
                // `CRATONVM_JIT_IR_EVIDENCE_COLD_ARM_WEIGHT`): this plan's
                // rarely taken spliced arms, so the acceptance gate prices a
                // blind dispatch there at the arm's weight. Re-installed for a
                // re-planned body (the combined pcs move with the plan).
                ir_evidence::set_splice_cold_ranges(&tables.spliced_cold_ranges);
                // JIT round 24 B3 (`CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT`, opt-in):
                // the spliced bodies' `MemorySegment` element sites, expanded
                // inline inside their splice.
                if ir_splice_ffm_element_enabled() && ir::ir_ffm_element_inline_enabled() {
                    // j27 wave 4 (lane SPLICEFFM): the de-spec registry is
                    // asked under THIS method's key, where the VM charges an
                    // in-splice guard's chain trap.
                    let method_key = format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                    let (ffm_sites, ffm_facts) = plan_spliced_ffm_element_sites(
                        &tables,
                        &combined,
                        direct_helpers,
                        req.despec.map(|r| &**r),
                        &method_key,
                    );
                    if !ffm_sites.is_empty() {
                        // j27 wave 4 (lane SPLICEFFM): the `HeapSession` class
                        // for the loop-entry session proof, as the root arm
                        // sets it. Keyed where the VM records its withdrawal:
                        // `ffm_field_hoist` plants that guard only with a FLAT
                        // frame of this method at one of its own loop headers
                        // (`loop_entry_frame`; a spliced loop's header has only
                        // a chain frame and gets none), so a failure is a flat
                        // trap at `(this method, header bci)`, exactly the
                        // record its `predicate_withdrawn` reads next compile.
                        builder.set_ffm_heap_session_class(ir_lower::scoped_heap_session_class_id(
                            direct_helpers,
                        ));
                        if x64::dbg_jitc_enabled() {
                            let mut pcs: Vec<usize> = ffm_sites.keys().copied().collect();
                            pcs.sort_unstable();
                            eprintln!(
                                "[cratonvm-jitc] ir-ffm-element-inline PLANNED in splice at combined pc(s) \
                                 {pcs:?} in {}.{}{}",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        builder.add_ffm_inline_sites(ffm_sites);
                        builder.add_ffm_layout_facts(ffm_facts);
                    }
                }
                builder.apply_inline_tables(tables);
                // Round 13 wave 3 (lane framestate): the outermost scope of a
                // deopt chain inside a spliced body names THIS method (see
                // `ir::Graph::scope_method_key`). Only read under
                // `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`. Not for an
                // `ACC_SYNCHRONIZED` method: its door hands the method monitor
                // to ONE resumed frame, and which frame of a pushed chain that
                // is has not been established, so it keeps the flat frames.
                // Round 15 wave 3 (lane sync2): nor, under the census switch,
                // a self-locking body (`ir_self_lock_requested`).
                if (ir::ir_splice_frame_states_enabled() || ir::ir_post_call_splice_exits_enabled())
                    && ir_sync_chains_ok
                {
                    builder.graph.scope_method_key = format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                    // Interpreter round i2 wave 54, lane L2: the kept spliced
                    // bodies a METHOD-ENTRY compile's post-call chain exits
                    // may rely on (`ir::Graph::entry_chain_source_keys`).
                    // Not for a synchronized method: its door hands the
                    // method monitor to one resumed frame, and no
                    // method-entry chain exit is admitted for it.
                    if !cached.is_synchronized {
                        builder.graph.entry_chain_source_keys = ir_splice_scope_sources
                            .iter()
                            .map(|row| row.method_key.clone())
                            .collect();
                    }
                }
                // No hand-off may outlive the planner.
                let _ = ir::take_splice_arg_classes();
                ir::set_ir_root_site_in_try(false);
                for &(pc, class_id) in &ir_exact_sites {
                    builder.add_exact_receiver_site(pc, class_id);
                    // j27 wave 5 (lane PROXY, `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`):
                    // the set covers the receivers that select this body, so
                    // the site is guarded by it whatever share the profile
                    // gives its first class.
                    if ir_cha_class_sets
                        .get(&pc)
                        .is_some_and(|set| builder.add_cha_class_set_site(pc, class_id, set))
                    {
                        builder.add_guarded_receiver_site(pc);
                    }
                    // JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`):
                    // every loaded subtype selects this body, so the site is
                    // guarded by the class-id watermark whatever the receiver
                    // profile says (the constant-pool class is often abstract,
                    // and the profile then rejects its exact test).
                    if let Some(&watermark) = ir_cha_watermarks.get(&pc) {
                        builder.add_cha_watermark_site(pc, watermark);
                        builder.add_guarded_receiver_site(pc);
                    }
                    // Round 12 wave 7 (lane iropt6, W6-1): the same site may
                    // splice behind a class test when the builder cannot prove
                    // the receiver exact, unless the receiver profile says the
                    // class almost never arrives there. The call stays the
                    // miss edge, so its row and inline cache are kept above.
                    if !ir::guarded_receiver_profile_rejects(profile, pc, class_id) {
                        builder.add_guarded_receiver_site(pc);
                        // nio21 wave 6, lane L4: the hot cap, for the one
                        // body the planner admitted under it.
                        if ir_guarded_hot_pcs.contains(&pc) {
                            builder.add_guarded_hot_cap_site(pc);
                        }
                        if let Ok(bci) = u32::try_from(pc) {
                            if pc < code_len {
                                ir_guarded_splice_bcis.push(bci);
                                if class_id != 0 {
                                    ir_guarded_splice_classes.push((bci, class_id));
                                }
                            }
                        }
                        // Round 14 wave 2 (lane guard, CH5-5): a site whose
                        // profile names this class alone may deopt on a miss
                        // instead of calling, unless that trap has already
                        // been withdrawn at this bci (the `Op::Guard` deopt's
                        // key: `(bci, UncommonTrap)`, as `ir_lower` stamps it).
                        // The for-each residual round: at a factory site, with
                        // its own switch and profile rule
                        // (`ir_factory_trap_miss_enabled`).
                        let factory_trap = ir_factory_site_pcs.contains(&pc)
                            && ir_factory_trap_miss_enabled()
                            && ir_factory_trap_profile_admits(profile, pc, class_id);
                        // Round nio20 wave 6 (lane L1): a byte-buffer site, under
                        // the general rule's profile bar
                        // (`ir_buffer_trap_miss_enabled`).
                        // nio21 wave 3 (lane L3): except a lone `put*`
                        // (`ir_buffer_trap_lone_put_call_enabled`).
                        let buffer_trap = ir_buffer_trap_admits_site(
                            &ir_buffer_site_pcs,
                            &ir_buffer_put_site_pcs,
                            pc,
                            ir_buffer_lone_put_call,
                        ) && ir::profile_admits_trap_miss(profile, pc, class_id);
                        if x64::dbg_jitc_enabled()
                            && ir_buffer_site_pcs.contains(&pc)
                            && !buffer_trap
                        {
                            eprintln!(
                                "[cratonvm-jitc] ir-buffer-trap-miss at pc={pc} in {}.{}{}: call \
                                 edge kept ({})",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                if ir_buffer_put_site_pcs.contains(&pc)
                                    && ir_buffer_site_pcs.len() == 1
                                    && ir_buffer_lone_put_call
                                {
                                    "lone put"
                                } else {
                                    "profile"
                                },
                            );
                        }
                        if factory_trap
                            || buffer_trap
                            || (ir::ir_guarded_splice_trap_miss_enabled()
                                && ir::profile_admits_trap_miss(profile, pc, class_id))
                        {
                            let bci = pc as u32;
                            let withdrawn = req.despec.is_some_and(|registry| {
                                registry.contains_speculation(
                                    &format!(
                                        "{}.{}:{}",
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor
                                    ),
                                    bci,
                                    crate::deopt::speculation_id(
                                        bci,
                                        crate::deopt::DeoptReason::UncommonTrap,
                                    ),
                                )
                            });
                            if !withdrawn {
                                builder.add_guarded_trap_site(pc);
                            }
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-guard-trap-miss at pc={pc} in {}.{}{}: \
                                     receiver class id {class_id} {}",
                                    cached.class_name,
                                    cached.method_name,
                                    cached.method_descriptor,
                                    if withdrawn {
                                        "withdrawn (de-spec)"
                                    } else {
                                        "planned"
                                    },
                                );
                            }
                        }
                    }
                }
                super::splice_args::reg_receiver_sites(
                    &mut builder,
                    &ir_bimorphic_sites,
                    &mut ir_polymorphic_sites,
                    profile,
                    req.despec,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
                // Round 13 wave 10 (lane iropt8, HS-2): nested exact-receiver
                // bodies. Registered with the builder's exact-receiver proof,
                // which splices one only for a receiver proven exactly that
                // class and otherwise lowers the call row the planner kept.
                // JIT round 19 (lane splice19, CH6W5-1): and, under
                // `CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE`, behind a class test
                // when the receiver is not proven (the call row stays the miss
                // edge).
                let nested_guarded = ir::ir_nested_guarded_splice_enabled();
                for &(pc, class_id) in &ir_exact_nested_sites {
                    // JIT round 19 wave 2 (lane splice2, SPLICE19-4): a tagged
                    // row is the second class of the nested site at its
                    // caller pc (its body is under the same tagged key).
                    let caller_pc = ir::inline_site_caller_pc(pc);
                    if caller_pc != pc {
                        builder.add_bimorphic_receiver_site(caller_pc, class_id);
                        continue;
                    }
                    builder.add_exact_receiver_site(pc, class_id);
                    if nested_guarded {
                        builder.add_nested_guarded_receiver_site(pc);
                        // JIT round 20 (lane splice): the hot cap for a body
                        // the VM admitted under it.
                        // A frozen object's class takes no slot: the planner admitted
                        // its body over the cold cap only for a receiver the snapshot
                        // names, guarded once by identity.
                        // shortcut: keyed by the classes of the snapshot objects this
                        // compile planned rows on, so an unproven receiver of such a
                        // class gets the hot cap behind its class test too; key by
                        // planner row if that shows up in code size.
                        let frozen_class = frozen_facts::compile_has_class(class_id);
                        if ir_nested_hot_pcs.contains(&pc) || frozen_class {
                            builder.add_guarded_hot_cap_site(pc);
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-nested-guarded-hot-cap at combined pc={pc} \
                                     in {}.{}{}: class id {class_id}",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                        }
                    }
                }
                builder.add_spliced_volatile_field_pcs(ir_spliced_volatile_pcs.iter().copied());
                ir_combined = Some(combined);
            }
        }
    }

    // Phase 2 (build). `nodes_built` is recorded straight after, so the
    // report can separate "the front end refused this bytecode" (build
    // returned None, `nodes_built` stays unmeasured) from "the graph was
    // built and then rejected for size".
    // The branches whose cold arm this compile may speculate away.
    //
    // Strictly stronger than `ir_branch_hints` above, and the
    // difference is the whole point: a hint says which edge to lay
    // out first and is free to be wrong, while this DELETES an arm
    // and pays a deopt when it is wrong. So the test is exact —
    // `not_taken == 0`, not "usually" — over a sample large enough
    // that zero means something (`ir::MIN_OBSERVATIONS_TO_PRUNE`).
    //
    // Empty without a profile, which is every run that has not
    // asked for one; `IrBuilder::prune_always_taken_branch` then
    // never fires and the graph is byte-identical.
    //
    // `speculatable_forward_branch` is the second clause, and it is
    // about the profile rather than about the builder — see that
    // function for why an unobserved loop EXIT is not evidence that
    // the loop never exits.
    // nio21 wave 17 (lane L4): both prune feeds skip a pc this VM's de-spec
    // registry withdrew (the guard trapped there `PER_BCI_DESPEC_LIMIT` times).
    // The always-taken feed never asked it, so a recompile re-planted the same
    // failing guard until the whole-method escalation retired the method.
    builder.set_pruned_branches(ir_pruned_branches);
    // nio21 wave 17 (lane L4): the never-taken mirror, default ON
    // (`ir_never_taken_branches`).
    if let Some(never_taken) = ir_never_taken_set {
        builder.set_never_taken_branches(never_taken);
    }
    note_jit_pipeline_stage(JIT_STAGE_BUILD);
    let metrics_build = metrics.phase(metrics::Phase::Build);
    // `code_len` stays the COMPILING method's length whichever buffer this
    // is: everything past it is relocated callee code, unreachable from pc
    // 0 and walked only through a splice.
    // The builder needs this BEFORE the walk, not after it: it is one of
    // the three clauses `IrBuilder::trap_replay_is_safe` asks before it
    // will plant an uncommon trap, and a trap is planted mid-walk. The
    // same value is stamped onto the artifact below
    // (`compiled.spliced_bodies_side_effect_free`), which is where the
    // interpreter reads it — producer and consumer now read one number.
    builder.set_spliced_bodies_pure(ir_spliced_bodies_pure);
    // Round 11 wave 14 (lane osrmerge): the opaque OSR entry merge
    // (`r11w11-irexc-osr-entry-as-a-real-predecessor`), behind
    // `CRATONVM_JIT_OSR_OPAQUE_ENTRY` (default OFF; unset, the builder is never
    // handed this and builds exactly what it built before). The per-pc live
    // locals WITH the method's handler edges decide which locals an OSR arm
    // reads: a local a handler reads must reach it on the OSR path too.
    //
    // Round 11 wave 17 (lane tier, blocker 1 of
    // `r11w16-irback-osr-opaque-entry-scope-and-door-bit-patch`): only when
    // the lowerer can emit an entry at all. With `CRATONVM_JIT_IR_OSR_ENTRY=0`
    // `emit_osr_entry_stubs` never looks at the arms, and the method-entry
    // body would pay the lost header facts for nothing. Deliberately NOT
    // narrowed to handler-reached headers: the guard-exit relaxation needs
    // EVERY emitted entry to be opaque.
    //
    // Round 13 wave 8 (lane irexc2): with that switch off, the OSR door's own
    // compile still gets arms at the headers the lowerer would otherwise
    // withhold (`Lowerer::osr_entry_closed_by_a_handler`: a handler reaches
    // the header and the body has rethrow pads, which exist only under
    // `precise_exception_frames`). That population has no optimizing OSR
    // entry at all today, and the door's body is never entered by a call
    // (`compile_optimizing_artifact(.., true)` never publishes through
    // `JitCache`), so the method-entry body pays nothing. A body that mixes
    // entry kinds publishes no relaxed guard fact (the fact is asked only
    // under the full switch), which is exactly today's fact.
    // An unloaded call can resolve and commit arbitrary effects in the
    // interpreter. Import the resulting locals and memory at every re-entry;
    // the ordinary header facts cannot represent that cold continuation.
    // An OSR-only artifact may resume after any speculative guard, including
    // atomic class/layout guards. Its headers must merge arbitrary live
    // interpreter locals and memory, rather than retain facts from pc zero.
    // Ordinary call-entry artifacts keep their existing header policy.
    //
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): under
    // `CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR` the door's clause is withheld on the
    // first build (`osr_narrow_opaque`). That build's guard exits then enter
    // the door only through the STRICT guard fact (exact / value-equivalent
    // guards, whose continuations falsify no header fact); a body that needs
    // the relaxed fact is dropped by `ir_tier` and rebuilt with this clause on.
    // The full argument is on `osr_narrow_opaque_door_enabled`.
    let osr_opaque_all = osr_exit::osr_opaque_entry_enabled()
        || (req.osr_mode_exit_polls && !osr_narrow_opaque)
        || builder.has_unloaded_static_splice_calls();
    let osr_opaque_handler_headers = !osr_opaque_all
        && precise_exception_frames
        && req.osr_mode_exit_polls
        && !cached.exception_table.is_empty()
        && osr_opaque_handler_headers_enabled();
    // Did this build get handler-header arms? Such a build's evidence is not
    // the method-entry build's, so an acceptance refusal of it is not written
    // into the per-method refusal memo (which the method-entry compile reads).
    let mut osr_handler_arms_handed = false;
    if (osr_opaque_all || osr_opaque_handler_headers) && ir_lower::ir_osr_entry_enabled() {
        let handlers: Vec<(usize, usize, usize)> = cached
            .exception_table
            .iter()
            .map(|row| {
                (
                    usize::from(row.start_pc),
                    usize::from(row.end_pc),
                    usize::from(row.handler_pc),
                )
            })
            .collect();
        let (rows, mut covered, words) = regalloc::live_locals_per_pc_all(
            code,
            code_len,
            num_params,
            &[],
            &handlers,
            cached.max_locals as usize,
        );
        if osr_opaque_handler_headers {
            // Only the handler-reached headers stay covered;
            // `IrBuilder::build_opaque_osr_merge` builds no arm at an
            // uncovered header. Unanalysable code builds none (the lowerer
            // then withholds every such entry, as before).
            let handler_pcs: Vec<u32> = cached
                .exception_table
                .iter()
                .map(|row| u32::from(row.handler_pc))
                .collect();
            let reach = deopt::pcs_reachable_from_handlers(code, &handler_pcs);
            mask_osr_arm_coverage_to_handler_reach(&mut covered, reach.as_deref());
        }
        if covered.iter().any(|&c| c) {
            builder.set_osr_opaque_liveness(rows, covered, words);
            builder.set_osr_requested_header(req.osr_requested_bci);
            // OSR round (2026-10-06): the requested header's arm tests the
            // imported locals the walk proves exact, unless that header's bci
            // was de-specialised (its declined entries were charged there).
            let arm_checks = req.osr_requested_bci.is_some_and(|bci| {
                let key = format!(
                    "{}.{}:{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
                !u32::try_from(bci).is_ok_and(|b| req.despec.is_some_and(|d| d.contains(&key, b)))
            });
            builder.set_osr_arm_exact_checks(arm_checks);
            // `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`: the caller locals a
            // call's one-class receiver profile names, by the operand that
            // loads its receiver (the builder keeps the write-once ones).
            if arm_checks && ir::ir_osr_profiled_slot_check_enabled() {
                if let Some(names) = cp_invoke_resolver {
                    let handler_pcs: Vec<usize> = cached
                        .exception_table
                        .iter()
                        .map(|e| usize::from(e.handler_pc))
                        .collect();
                    let mut classes: HashMap<usize, u32> = HashMap::new();
                    let mut conflicted: Vec<usize> = Vec::new();
                    for &(pc, cp_idx, opcode) in ir_invoke_ops.iter() {
                        if pc >= code_len || !matches!(opcode, 0xb6 | 0xb9) {
                            continue;
                        }
                        let Some(class_id) = ir::profile_guarded_receiver_class(profile, pc) else {
                            continue;
                        };
                        let Some((_, _, desc)) = names(cp_idx) else {
                            continue;
                        };
                        let Some(slots) = ir::descriptor_operand_slots(&desc, true) else {
                            continue;
                        };
                        let Some(sources) = ir::splice_call_operand_sources(
                            code,
                            code_len,
                            pc,
                            slots.len(),
                            &handler_pcs,
                        ) else {
                            continue;
                        };
                        let Some(ir::SpliceOperandSource::Local(k)) = sources.first().copied()
                        else {
                            continue;
                        };
                        if let Some(prev) = classes.insert(k, class_id) {
                            if prev != class_id {
                                conflicted.push(k);
                            }
                        }
                    }
                    let conflicted_slots = conflicted.clone();
                    for k in conflicted {
                        classes.remove(&k);
                    }
                    // `CRATONVM_JIT_IR_OSR_NEW_LOCAL_CLASSES`: the write-once
                    // locals holding a `new` / a one-class factory's result
                    // are tested at the OSR entry too, so the loop sees them
                    // exact and its field loads are not walled off by a
                    // class-miss call arm.
                    {
                        for (&k, &c) in ir_osr_new_local_classes.iter() {
                            if !conflicted_slots.contains(&k) {
                                classes.entry(k).or_insert(c);
                            }
                        }
                    }
                    builder.set_osr_profiled_slot_classes(classes);
                }
            }
            // JIT round 18 wave 9 (lane osrpoly;
            // `r18-osr-door-opaque-refusal-poisons-the-method-entry-memo-FIXED-20261006.md`):
            // an OSR-door build whose opaque arms the door alone asked for
            // (`req.osr_mode_exit_polls` above, not the process switch the
            // method-entry build also obeys) lost header facts the method-entry
            // build keeps, so its evidence is not that build's either. Default
            // ON; `CRATONVM_JIT_OSR_DOOR_OWN_REFUSAL_MEMO=0` restores the shared
            // memo write.
            // Round 18 wave 11 (lane osrfollow): a narrowed build
            // (`osr_narrow_opaque`) asked for no door-only arms; any arms it
            // has are the handler headers' (handed above) or the unloaded
            // static splices' (which the method-entry build has too).
            let door_only_arms = req.osr_mode_exit_polls
                && !osr_narrow_opaque
                && !osr_exit::osr_opaque_entry_enabled()
                && osr_door_own_refusal_memo_enabled();
            osr_handler_arms_handed = osr_opaque_handler_headers || door_only_arms;
        }
    }
    // Clears the builder's per-build results now and when this compile
    // leaves the scope, however it leaves.
    let ir_build_results = ir::IrBuildResultsScope::enter();
    // Round 12 wave 6 (lane iropt5, W5-2): when this build splices anything,
    // ask it which splice a refusal belongs to (see `ir_tier`).
    let ir_splice_refusal =
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(ir::SPLICE_REFUSAL_NONE));
    let ir_rebuild_armed = ir_combined.is_some() && ir_splice_skip.len() < IR_SPLICE_REBUILD_MAX;
    if ir_rebuild_armed {
        builder.set_splice_refusal_sink(std::sync::Arc::clone(&ir_splice_refusal));
    }
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the exception table the
    // builder plans compiled handler entries from, under
    // `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY` (`catch_arms` is cleared by
    // `ir_tier` for the one retry after a refused attempt that had it).
    if catch_arms && !cached.exception_table.is_empty() {
        if let Some(rows) = ir_catch_arm_rows(cached, cp_class_name_resolver) {
            builder.set_catch_arm_table(rows, cached.declaring_class_id.as_u32());
            *catch_arms_staged = true;
        }
    }
    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the filed classes of the
    // trusted `static final` references this build reads, its own sites and
    // the spliced bodies' alike (the site tables are merged by now).
    if ir::ir_static_final_exact_class_enabled() {
        if let Some(class_of) = req.static_final_ref_class_resolver {
            let classes: HashMap<(u32, u32), u32> = builder
                .static_reference_field_keys()
                .into_iter()
                .filter_map(|key| Some((key, class_of(key.0, key.1)?)))
                .collect();
            // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: whether each of those
            // classes is caught by a typed row, as the VM's handler search
            // answers it, so a spliced throw of one needs no type test.
            if ir::ir_spliced_throw_edges_enabled() && !classes.is_empty() {
                if let Some(is_subclass) = req.class_is_subclass_resolver {
                    let catches = builder.local_throw_catch_classes();
                    let mut facts: HashMap<(u32, u32), bool> = HashMap::new();
                    for &thrown in classes.values().filter(|&&c| c != 0) {
                        for &c in &catches {
                            if thrown != c && !facts.contains_key(&(thrown, c)) {
                                if let Some(answer) = is_subclass(thrown, c) {
                                    facts.insert((thrown, c), answer);
                                }
                            }
                        }
                    }
                    builder.add_local_throw_subclass_facts(facts);
                }
            }
            builder.set_static_final_exact_classes(classes);
        }
    }
    builder.set_static_guess_despec(
        req.despec.cloned(),
        format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        ),
    );
    // `CRATONVM_FROZEN_PAGES`: after the feed above, whose table names the
    // roots a fold may start from.
    if let Some(facts) = req.frozen_facts {
        builder.set_frozen_facts(std::sync::Arc::clone(facts));
        builder.set_frozen_receivers(
            req.frozen_receivers,
            req.despec.cloned(),
            format!(
                "{}.{}:{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            ),
        );
    }
    // JIT r25 wave 2 (`CRATONVM_JIT_PIN_BAKED_CALLEES_WHILE_COMPILING`, default ON):
    // keep every baked direct-call target alive across the build and lowering,
    // so the publication can still pin it (the artifact takes these over).
    let mut ir_baked_callee_pins: Vec<RetainedCode> = Vec::new();
    if pin_baked_callees_while_compiling_enabled() {
        for &(entry, artifact) in &ir_direct_callee_entries {
            let Some(owner) = resolve_jit_entry_owner(entry) else {
                continue;
            };
            let pin = RetainedCode::new(owner);
            if artifact == 0 || pin.artifact_id == artifact {
                ir_baked_callee_pins.push(pin);
            }
            // A mismatched owner is released here, through the queue.
        }
        if x64::dbg_jitc_enabled() && !ir_baked_callee_pins.is_empty() {
            eprintln!(
                "[cratonvm-jitc] pin-baked-callees {}.{}{}: {} of {} baked target(s) pinned for the compile",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                ir_baked_callee_pins.len(),
                ir_direct_callee_entries.len(),
            );
        }
    }
    let mut built = builder.build(ir_combined.as_deref().unwrap_or(code), code_len);
    // JIT r23 wave 6 (lane L8, `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE_UNSERVED_REFUSE`,
    // correctness of the plan, default ON): a planned FFM element inline site
    // with no other route (`bail_on_decline`) that the build left a plain
    // `MemorySegment` CALL refuses the optimizing body, so the method keeps
    // the single-pass intrinsic route the plan exempted it from. Measured on
    // n23-w5: the builder's `invokeinterface` arm never offered the site to the
    // expansion, the call stayed, and every access paid the MIC dispatch into
    // the JDK accessor and its generic `VarHandle` road (`R23W4SegMono`
    // heapget 30 us/op against 1.9 on the single-pass route).
    if !ir_ffm_inline_sites.is_empty() {
        let unserved = built.as_ref().and_then(|graph| {
            graph.nodes.iter().find_map(|node| {
                let ir::Op::Call { info_ptr } = node.op else {
                    return None;
                };
                let pc = node.bytecode_pc?;
                if !ir_ffm_inline_sites
                    .get(&pc)
                    .is_some_and(|s| s.bail_on_decline)
                {
                    return None;
                }
                // SAFETY: the builder takes only this compile's owned invoke rows.
                let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                (info.class_name == "java/lang/foreign/MemorySegment").then_some(pc)
            })
        });
        if let Some(pc) = unserved {
            if x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-ffm-element-inline UNSERVED at pc={pc} in {}.{}{}: \
                     the optimizing body is refused (single-pass route)",
                    cached.class_name, cached.method_name, cached.method_descriptor,
                );
            }
            built = None;
        }
    }
    // j27 wave 4 (lane SPLICEFFM2, `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT`,
    // opt-in): a `MemorySegment` element call the build left INSIDE a splice
    // is a dispatched accessor, never published. The attempt is refused and
    // names the top-level splices holding such calls; `ir_tier` re-plans once
    // without them (`ffm_element_inline::spliced_ffm_declined_sites`).
    let ffm_declined = built.as_ref().and_then(|graph| {
        ffm_element_inline::spliced_ffm_declined_sites(
            ir_graph_call_targets(graph),
            code_len,
            &ir_spliced_ranges,
        )
    });
    if let Some(sites) = ffm_declined {
        if x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-ffm-element-inline DECLINED in splice under site(s) {sites:?} \
                 in {}.{}{}: re-planned without them (a dispatched accessor is never published)",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
        *ffm_splice_declined = sites;
        built = None;
    }
    // JIT r25 (`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`, default ON): every
    // `instanceof` with no seen-class cell of its own gets a lambda-proxy cell
    // ([`ir_lower::INSTANCEOF_PROXY_CELL_TAG`]), one per target name in this
    // body, so two tests of one target stay one op for value numbering. The
    // artifact owns the cells (`_jit_typecheck_seen_cells`), whose addresses
    // the lowering bakes.
    //
    // Round rx27 (lane compile, `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`,
    // opt-in): a `checkcast` against a target shares that target's cell
    // (its hit ways mean the same thing: the one helper predicate,
    // `jit_typecheck_resolve`, answered `true` for that proxy class). Listed
    // by name in `Graph::checkcast_proxy_cells`, since `Op::CheckCast` has no
    // cell field and the optimizer may copy the node.
    let instanceof_proxy_ways = ir_lower::ir_instanceof_proxy_ways_enabled();
    let checkcast_proxy_ways = ir_lower::ir_checkcast_proxy_ways_enabled();
    if instanceof_proxy_ways || checkcast_proxy_ways {
        if let Some(graph) = built.as_mut() {
            let mut by_target: HashMap<usize, usize> = HashMap::new();
            let mut cast_targets: Vec<(usize, usize)> = Vec::new();
            let mut cell_for = |name_ptr: usize| -> usize {
                *by_target.entry(name_ptr).or_insert_with(|| {
                    let cell: Box<TypecheckSeenCell> = Box::default();
                    let addr = cell.as_ptr() as usize;
                    ir_typecheck_seen_cells.push(cell);
                    addr
                })
            };
            let mut instanceof_targets = 0usize;
            for node in graph.nodes.iter_mut() {
                match &mut node.op {
                    ir::Op::InstanceOf {
                        name_ptr,
                        seen_cell,
                        ..
                    } if instanceof_proxy_ways && *seen_cell == 0 && *name_ptr != 0 => {
                        *seen_cell = cell_for(*name_ptr) | ir_lower::INSTANCEOF_PROXY_CELL_TAG;
                        instanceof_targets += 1;
                    }
                    ir::Op::CheckCast { name_ptr, .. }
                        if checkcast_proxy_ways && *name_ptr != 0 =>
                    {
                        let name_ptr = *name_ptr;
                        if !cast_targets.iter().any(|&(n, _)| n == name_ptr) {
                            cast_targets.push((name_ptr, cell_for(name_ptr)));
                        }
                    }
                    _ => {}
                }
            }
            if x64::dbg_jitc_enabled() && (instanceof_targets > 0 || !cast_targets.is_empty()) {
                eprintln!(
                    "[cratonvm-jitc] ir instanceof proxy ways {}.{}{}: {} target(s), \
                     {} checkcast target(s), hit_ways={}",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    by_target.len(),
                    cast_targets.len(),
                    if ir_lower::ir_proxy_two_hit_ways_enabled() {
                        2
                    } else {
                        1
                    },
                );
            }
            graph.checkcast_proxy_cells = cast_targets;
        }
    }
    // Bind after the graph contains both own and relocated calls. Key the
    // context by invoke info, so optimizer copies retain the judged descriptor
    // while bytecode PCs continue to identify the existing direct-call rows.
    // Every Call info is owned by ir_call_infos for this compile/artifact.
    if let Some(graph) = built.as_mut() {
        for node in &graph.nodes {
            let ir::Op::Call { info_ptr } = node.op else {
                continue;
            };
            let Some(pc) = node.bytecode_pc else { continue };
            // SAFETY: the builder takes only this compile's owned invoke rows.
            let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
            if info.invoke_kind == 3 {
                if let Some(entry) = ir_arraycopy_entry(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    direct_helpers,
                    jdk_only,
                    direct_bind_admission,
                ) {
                    graph
                        .native_call_contexts
                        .insert(info_ptr, direct_helpers.static_base_resolver_ctx);
                    ir_direct_calls.insert(pc, (entry, true));
                    continue;
                }
                // JIT round 23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`):
                // an `Array.newArray` left behind by a SPLICED `Array.newInstance`
                // binds to the same thin helper as a caller's own site (and so
                // takes the optimizing tier's inline allocation prefix), as the
                // arraycopy row above; a site the caller already bound is kept.
                if direct_helpers.static_base_resolver_ctx != 0
                    && ir_direct_calls_enabled()
                    && !ir_direct_calls.contains_key(&pc)
                {
                    let entry = reflect_new_array_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                    );
                    if entry != 0 {
                        graph
                            .native_call_contexts
                            .insert(info_ptr, direct_helpers.static_base_resolver_ctx);
                        ir_direct_calls.insert(pc, (entry, true));
                        if x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] reflect-new-array-direct bind (ir spliced) @pc={pc}"
                            );
                        }
                        continue;
                    }
                }
            }
            // JIT round 23 wave 5, lane L5 (`CRATONVM_JIT_IR_SPLICED_STRING_REGEX_DIRECT`):
            // a `String.matches` / `replaceAll` / `replaceFirst` call left
            // behind in a SPLICED body (the caller's own sites never reach
            // this tier: `string_site_bails_ir_planning` keeps such a method
            // single-pass, which binds them) binds to the same thin helper.
            // Measured on n23-w4: `RxLoop.matches()` spliced into the
            // OSR-compiled `main` left its `String.matches` on the MIC +
            // native funnel (`jit_invoke_virtual_mic`, `try_lean_native_door`,
            // `safe_native_call_impl`), 5.9 us per eight calls.
            if matches!(info.invoke_kind, 0 | 1)
                && direct_helpers.static_base_resolver_ctx != 0
                && !ir_direct_calls.contains_key(&pc)
                && ir_spliced_string_regex_direct_enabled()
            {
                if let Some((entry, _, _)) = string_regex_helper(
                    direct_helpers,
                    jdk_only,
                    direct_bind_admission,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                ) {
                    graph
                        .native_call_contexts
                        .insert(info_ptr, direct_helpers.static_base_resolver_ctx);
                    ir_direct_calls.insert(pc, (entry, true));
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] string-regex-direct bind (ir spliced) {}.{} @pc={pc}",
                            info.class_name, info.method_name
                        );
                    }
                    continue;
                }
            }
            // JIT round 23 wave 3, lane L8 (`CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT`):
            // a `MemorySegment` element accessor admitted above binds to its
            // judged thin helper, which serves the FFM element door's cached
            // road and dispatches this row on every decline.
            if matches!(info.invoke_kind, 0 | 2) {
                if let Some(entry) = ir_ffm_element_direct_entry(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    direct_helpers,
                ) {
                    let record = VarHandleJudgedSite::new(
                        direct_helpers.static_base_resolver_ctx,
                        entry,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        info.num_jit_args,
                        info.return_type,
                        info.invoke_kind,
                        info.declaring_class_id,
                        info.owner_class_id,
                    );
                    let context = (&*record as *const VarHandleJudgedSite as usize)
                        | VARHANDLE_JUDGED_SITE_TAG;
                    graph.native_call_contexts.insert(info_ptr, context);
                    ir_direct_calls.insert(pc, (entry, true));
                    ir_vh_judged_sites.push(record);
                    if x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-ffm-element-direct BOUND {}{} @pc={pc}",
                            info.method_name, info.descriptor
                        );
                    }
                    continue;
                }
            }
            if info.invoke_kind != 0 || info.class_name != "java/lang/invoke/VarHandle" {
                continue;
            }
            // nio21 wave 12 L2: and, under `CRATONVM_JIT_IR_VH_CONST_ADMIT`,
            // every primitive read / write / CAS site the single-pass ladder
            // would bind (`varhandle_ir_const_bind_entry`): the fallback behind
            // the constant-handle prefix of a method this tier now admits.
            let Some(entry) = varhandle_ir_access_entry(
                info.method_name,
                info.descriptor,
                direct_helpers,
                jdk_only,
                direct_bind_admission,
            )
            .or_else(|| {
                varhandle_ir_const_bind_entry(
                    info.method_name,
                    info.descriptor,
                    direct_helpers,
                    jdk_only,
                    direct_bind_admission,
                )
            }) else {
                continue;
            };
            let record = VarHandleJudgedSite::new(
                direct_helpers.static_base_resolver_ctx,
                entry,
                info.class_name,
                info.method_name,
                info.descriptor,
                info.num_jit_args,
                info.return_type,
                info.invoke_kind,
                info.declaring_class_id,
                info.owner_class_id,
            );
            let context =
                (&*record as *const VarHandleJudgedSite as usize) | VARHANDLE_JUDGED_SITE_TAG;
            graph.native_call_contexts.insert(info_ptr, context);
            ir_direct_calls.insert(pc, (entry, true));
            ir_vh_judged_sites.push(record);
            if x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-varhandle-read BOUND {}{} @pc={pc}",
                    info.method_name, info.descriptor
                );
            }
        }
    }
    if built.is_none() && ir_rebuild_armed {
        *ir_rebuild_without = ir_splice_rebuild_site(
            ir_splice_refusal.load(std::sync::atomic::Ordering::Relaxed),
            code_len,
            &ir_spliced_ranges,
        );
    }
    // Record that this method's optimizing body carries a site trap, so a
    // trap TAKEN at runtime can be told apart from genuinely unreachable
    // code. `build` consumes the builder, so the count comes back through
    // `ir_build_results`, the scope opened above.
    if ir_build_results.site_traps_planted() > 0 {
        req.verdicts()
            .register_site_trap_method(ir_method_hash, cached.declaring_class_id);
        // JIT round 19 proposal wave 7 (lane interp7, EAGER3-19-1 /
        // OSRENTRY6-1): and each planted trap by its bci, so the sink can ask
        // about the bci that trapped (`CRATONVM_JIT_SITE_TRAP_PER_BCI`).
        req.verdicts().register_site_trap_at(
            ir_method_hash,
            cached.declaring_class_id,
            &ir_build_results.site_trap_pcs_planted(),
        );
    }
    drop(metrics_build);
    // ── the String-access expansion's two loads get their compact rows ──
    //
    // `ir::try_string_access_intrinsic` expands `length`/`isEmpty`/`charAt`
    // into a `coder` (Int) and a `value` (Ref) `Op::Load` — both at the
    // INVOKE pc. The table built above is keyed by GETFIELD pc, so neither
    // load has a row, and `ir_lower::emit_inline_compact_getfield` declines
    // both with `no-compact-slot-for-pc`. That decline is a `jit_getfield`
    // helper CALL per character: `probes/CharAtCostCurve.java` on the arm
    // that reaches the emitter measured **917,203,334** of them in one run
    // (2026-09-02), against `IR-tier inline-getfield refusals:
    // no-compact-slot-for-pc=8` — i.e. eight sites, every character.
    //
    // Offsets come from `resolved_string_layout`, the layout THIS compile
    // resolved and the one the single-pass backend is handed — the same
    // layout the builder's expander read its slot indices from
    // (`set_string_layout` above). Never a process-wide published layout:
    // one latched early can predate `java/lang/String`'s `CompactLayout`, and
    // one latched by another VM names that VM's slots.
    //
    // Refused, so a wrong row is never installed rather than installed and
    // guarded downstream:
    //   * no `coder` field (legacy `char[]` String) — the expansion cannot
    //     run at all, so a row would describe nothing;
    //   * a NARROW compact `value` — the inline arm emits an unconditional
    //     8-byte load. `narrow_oops_block_inline_fields` already refuses
    //     the whole site under compressed oops, so this is the second of
    //     two independent refusals, kept because a row installed on the
    //     strength of the other one is a wrong-width load if that one ever
    //     moves;
    //   * a negative body offset, which would mean the layout's address is
    //     below the object header — impossible by construction, and the
    //     `as u32` below is why it is checked rather than assumed.
    if let Some(layout) = resolved_string_layout {
        let sites = ir_build_results.string_access_site_pcs();
        // `CRATONVM_JIT_NO_STRING_ACCESS_INLINE_ROWS=1` is the B arm:
        // without the rows both loads fall back to the checked
        // `jit_getfield` helper, exactly as they did before 2026-09-02,
        // so the cost of that fallback is measurable inside one binary.
        if !sites.is_empty()
            && layout.has_coder
            && !layout.value_compact_is_narrow
            && cratonvm_types::compact_ref_fields_enabled()
            && !false
        {
            // A compact row is an ABSOLUTE displacement, which is exactly what
            // `StringFieldLayout`'s compact offsets already are.
            let value_body = layout.value_compact_offset;
            let coder_body = layout.coder_compact_offset;
            // `coder` is a `byte` field. A registered `CompactLayout`
            // stores it at its natural one-byte width; the no-registered-
            // layout fallback points at the legacy 4-byte cell payload
            // instead, and the two need different loads — which is the
            // whole reason `coder_compact_is_byte` exists.
            let coder_tag = if layout.coder_compact_is_byte {
                b'B'
            } else {
                b'I'
            };
            if value_body >= 0 && coder_body >= 0 {
                for pc in sites {
                    ir_compact_fields.insert((pc, true), (value_body as u32, true, b'['));
                    ir_compact_fields.insert((pc, false), (coder_body as u32, false, coder_tag));
                    STRING_ACCESS_COMPACT_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    if let Some(g) = built.as_ref() {
        metrics.set_nodes_built(g.nodes.len());
        metrics.phase_nodes(metrics::Phase::Build, 0, g.nodes.len());
    }
    // Phase 2b (verify the FRONT END's own output).
    //
    // `ir_verify`'s module header states the requirement as "run after
    // parsing and every mutating pass in stress builds", and parsing was the
    // one place it did not run: the first hook was after
    // `ir_optimize::optimize`. That ordering put the pass most likely to
    // *panic* on a malformed graph — `ir_optimize` indexes `graph.nodes` by
    // raw `NodeId` — in front of the check that would have named the defect,
    // and attributed anything that survived to `"post-optimize"`, i.e. to the
    // wrong pass.
    //
    // Deliberately BEFORE the `IR_MAX_GRAPH_NODES` gate below, and not after
    // it: a front-end defect in an over-large graph is the same front-end
    // defect, and the gate is about compile *cost*, not about validity. This
    // is one linear pass with no allocation over the smallest graph the
    // pipeline ever holds.
    //
    // A rejection here takes the pipeline's existing failure path — `None`,
    // and the single-pass backend below compiles the method — which is the
    // same response the three later hooks' `ir_verify_bail` produces. The
    // lanes that run are the structural one plus anything explicitly forced;
    // see `ir_verify::PHASE_POST_BUILD` for why the type lane is staged
    // behind `CRATONVM_JIT_VERIFY_TYPES=1` here rather than defaulted on.
    // Spelled as `filter` rather than a `match`: a rejection's response IS
    // `None`, which is exactly what `filter` produces, and the `match` spelling
    // is what clippy's `manual_filter` refuses. Behaviour is unchanged,
    // including that `ir_verify_reject` still runs only when verification is
    // enabled and only on a graph that was built.
    let built = built.filter(|g| {
        if !ir_verify::verify_enabled() {
            return true;
        }
        !ir_verify_reject(
            g,
            ir_verify::PHASE_POST_BUILD,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    });
    // jit-inlining-and-ir-calls — tier-4 compile-time guard.
    // `ir_compatible`'s bytecode budget rose from 200 to HotSpot's 8000-byte
    // HugeMethodLimit, which is the right *admission* rule but a poor proxy
    // for compile COST: `ir_optimize`'s GVN, the escape-analysis connection
    // graph and the scheduler are all super-linear in NODE count, and an
    // 8000-byte straight-line arithmetic method builds a far larger graph
    // than an 8000-byte call-heavy one. Check the built graph — the one
    // input that reflects actual complexity — before any optimization runs,
    // so an over-large method costs exactly one linear build and then takes
    // the single-pass backend. Its runtime companion is
    // `tiered::MAX_C2_COMPILE_TIME_MS`, which catches whatever slips past.
    let built = match built {
        Some(g) if g.nodes.len() > ir::IR_MAX_GRAPH_NODES => {
            ir_evidence::mark_ir_exit(ir_evidence::AdmissionExit::NodeCap);
            if false {
                eprintln!(
                    "[cratonvm-ircall] {}.{}{}: IR graph {} nodes > IR_MAX_GRAPH_NODES {} — single-pass",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    g.nodes.len(),
                    ir::IR_MAX_GRAPH_NODES,
                );
            }
            // This *is* `BailoutReason::GraphTooLarge`; the check predates
            // `bailout.rs` and still signals with a bare `None`. Recording
            // it on the report (and not through `bailout::record_bailout`)
            // names the reason without changing what the process-wide
            // counters count — converting the signal itself belongs to the
            // owner of this gate.
            metrics.note_bailout_reason(
                bailout::BailoutReason::GraphTooLarge {
                    nodes: g.nodes.len(),
                    limit: ir::IR_MAX_GRAPH_NODES,
                },
                "post-build",
            );
            None
        }
        other => other,
    };
    // Soak diagnostic (CRATONVM_DBG_SCALAR_NEW): an allocation-bearing method
    // that bailed the IR builder went single-pass, so `new` scalar
    // replacement could not fire on it — the signal that the IR builder is
    // missing an opcode the method uses (this is how the `astore` gap, which
    // silently disabled scalar-new on ALL real javac allocations, surfaced).
    // Remembered for ONE retry, and only now that the build has actually
    // lost the method: a deferred site the builder never reached (dead
    // code) would otherwise have cost a wasted C2 attempt.
    // `take_deferred_new_retry` clears it.
    // Not when the build is about to be retried without a splice (W5-2): the
    // retry may build, and a noted bail would cost a wasted C2 attempt.
    // Nor before the SPLICEFFM2 re-plan (`ffm_splice_declined`), for the same
    // reason.
    if built.is_none()
        && any_deferred_new
        && ir_rebuild_without.is_none()
        && ffm_splice_declined.is_empty()
    {
        // The calling VM's memo (interpreter round i1 wave 10), a required
        // input since wave 18 (`CompileRealm`): no silent process fallback.
        req.deferred_new_retries.note_bail(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            &deferred_new_sites,
        );
    }
    if built.is_none() && ir_stage_reporting() {
        eprintln!(
            "[ir] IrBuilder::build returned None for {}.{}{} — no IR body",
            cached.class_name, cached.method_name, cached.method_descriptor,
        );
    }
    if built.is_none() && !scan.new_ops.is_empty() && false {
        eprintln!(
            "[cratonvm-scalarnew] IR builder bailed (single-pass) for allocation method {}.{}{}",
            cached.class_name, cached.method_name, cached.method_descriptor,
        );
    }
    if let Some(mut graph) = built {
        graph.direct_static_fields = std::mem::take(&mut ir_direct_static_fields);
        if crate::x64::dbg_jitc_enabled() {
            graph.dbg_label = format!(
                "{}.{}{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        // JIT round 23 wave 13 (lane L7, `CRATONVM_JIT_IR_DIRECT_STATIC_SPLICED_FEED`):
        // the spliced bodies' static reads too (`ir_direct_static_spliced_rows`).
        if ir_direct_static_spliced_feed_enabled() {
            let extra =
                ir_direct_static_spliced_rows(&graph, &graph.direct_static_fields, |c, i| {
                    direct_helpers.resolve_static_base(c, i).is_some()
                });
            graph.direct_static_fields.extend(extra);
        }
        // JIT round 19 wave 2 (lane irops2, IROPS19-1), opt-in: direct
        // static accesses become plain reads / writes of their own cell in
        // `Graph::memory_effect`. Read once per compile, after the build, so
        // the builder's own effect queries are unchanged.
        graph.static_alias_class = ir::ir_static_alias_class_enabled();
        // JIT round 19 wave 4 (lane irops3, IROPS2-5), opt-in and inert
        // without the alias class: a reference `putstatic` of an initialised
        // class is a write of its own cell plus a safepoint.
        graph.static_ref_store_effect =
            graph.static_alias_class && ir::ir_static_ref_store_effect_enabled();
        graph.trusted_final_static_fields = std::mem::take(&mut ir_trusted_final_static_fields);
        // JIT round 20 wave 11 (lane pIR; L3W6-2 / W6L4-5): the lengths of the
        // trusted `static final` arrays this graph reads -- its own sites and
        // every spliced body's -- for `ir_optimize`'s `arraylength` fold. The
        // VM files a length only for a trusted final of an initialised class;
        // the direct-read answer (`resolve_static_base`: initialised now, never
        // `java/lang/System`) is required here too. A volatile read is never
        // folded (a final cannot be volatile; belt and braces).
        super::splice_args::feed_trusted_arrays(
            &mut graph,
            |c, i| direct_helpers.resolve_static_base(c, i).is_some(),
            req.static_final_array_length_resolver,
        );
        // History: the IR backend used to miscompile a *pure* (call-free)
        // method containing a conditional branch / φ merge — a tiny leaf
        // predicate like `static boolean f(int m){ return (m & K) != 0; }`
        // (e.g. `java.lang.reflect.Modifier.isStatic`) SIGSEGV'd with a write
        // through a near-null base. ROOT CAUSE (fixed): the Op::Cmp SETcc was
        // emitted as `0F 9x` without its ModRM byte, desyncing the stream
        // into a stray `SETL [rdi]` (near-null write) and leaving the boolean
        // unset. A second blocker — loop-carried phis dropped on the
        // back-edge — was also fixed (loop-header eager phis + back-patch).
        // The IR path now compiles if/else and loops (while/do-while/nested)
        // correctly, so branchy call-free integer methods take the IR
        // pipeline by default. `CRATONVM_NO_IR_BRANCHY` is the emergency
        // opt-out (restores single-pass-only routing for this shape); methods
        // the IR builder can't fully build still return None → single-pass.
        // (Bisected originally from keycloak JsonParserTest /
        // SkeletonKeyTokenTest SIGSEGVs + a standalone `Modifier.isStatic`.)
        let has_conditional_branch = graph.nodes.iter().any(|n| matches!(n.op, ir::Op::If));
        if has_conditional_branch
            && scan.invoke_ops.is_empty()
            && !ir_optimize::ir_branchy_enabled()
            && !ir_optimize::reassoc_enabled()
        {
            // Branchy-IR explicitly disabled (CRATONVM_NO_IR_BRANCHY) and
            // reassoc off → fall through to the single-pass backend below.
        } else {
            // P0 "JIT correctness" (deep-research-vm-c2.md):
            // the IR verifier runs after every mutating pass when
            // `ir_verify::verify_enabled()` (debug builds, or
            // `CRATONVM_JIT_VERIFY_IR=1`), and unconditionally immediately
            // before lowering. `ir_verify_bail` latches the first rejection
            // and steers the method down the pipeline's existing failure
            // path — falling through to the single-pass backend below.
            // Nothing on the success path changes.
            let mut ir_verify_bail = false;

            // #49, before any pass reads the snapshots as DCE roots: cut the
            // snapshot locals the bytecode can never read again. Round 13
            // wave 12 (lane snapliv): with the exception table's edges
            // modelled (a handler reads locals the normal flow does not), and
            // per scope in a chain snapshot, against the bodies this build
            // spliced. See `ir_prune_dead_snapshot_locals_scoped`.
            {
                let handlers: Vec<(usize, usize, usize)> = cached
                    .exception_table
                    .iter()
                    .map(|row| {
                        (
                            usize::from(row.start_pc),
                            usize::from(row.end_pc),
                            usize::from(row.handler_pc),
                        )
                    })
                    .collect();
                ir_prune_dead_snapshot_locals_scoped(
                    &mut graph,
                    ir_combined.as_deref().unwrap_or(code),
                    code_len,
                    &handlers,
                    &ir_inline_frame_sites,
                    // Round 13 wave 13 (lane jitfix): the debugger discard check
                    // below asks every chain scope now, so the chain prune may
                    // run under a debugger too.
                    true,
                );
            }
            // #49: fold `o == null` on fresh allocations (including spliced
            // Integer/Long boxes identified by VM class ID) before optimizing.
            graph.box_class_ids = (
                direct_helpers.integer_class_id(),
                crate::ir_lower::ir_long_class_id(direct_helpers),
            );
            ir_fold_null_checks_on_fresh_allocations_with(&mut graph, &ldc_string_hashes);
            // Round 14 wave 4 (lane calls2, C14W3-4).
            ir_fold_string_literal_queries(&mut graph, &ldc_string_texts);

            // Phase 3 (optimize). `ir_optimize::optimize` is opaque — GVN,
            // DCE, reassociation, LICM and unrolling all run inside it and
            // it publishes no per-pass boundary — so this is one row, not
            // one row per pass. The before/after node counts are still the
            // useful number, but they are no longer a frame-size figure:
            // `ir_lower::estimate_frame_bytes` multiplies the
            // liveness-COLOURED slot count (`SlotPlan::slots`) by 8, not the
            // node count. Shrinking the graph therefore shrinks the frame
            // only when it also shrinks peak simultaneous liveness — which
            // is what the report's `peak_live_values` is for.
            note_jit_pipeline_stage(JIT_STAGE_OPTIMIZE);
            let metrics_optimize = metrics.phase(metrics::Phase::Optimize);
            let metrics_nodes_before_optimize = graph.nodes.len();
            // This VM's per-bci de-spec registry, so a guard `optimize` plants
            // from a structural fact (the range-check predicate) is withdrawn
            // once it keeps failing, instead of being re-planted on every
            // recompile (round 11 wave 7,
            // `r11w6-mid-ir-tier-guards-never-read-the-despec-registry`). Same
            // key form as the deopt log's and the single-pass tier's.
            let despec_method_key = format!(
                "{}.{}:{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
            // JIT round 19 wave 6 (lane osr6, OSR5-2): the retry's promotion
            // admits no loop holding a deopt point.
            graph.promote_pending_withheld = promote_pending_withheld;
            // JIT round 25 wave 7 (lane W7-D, W6C-P4): the compiling method's
            // own `charAt` sites' receiver counts, for the predicate's profile
            // gate (`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE`; nothing
            // with it off).
            ir_optimize::seed_seq_charat_own_receiver_rows(&mut graph, profile, code_len);
            ir_optimize::optimize_with(
                &mut graph,
                ir_optimize::OptimizeContext {
                    despec: req.despec.map(|r| &**r),
                    method_key: &despec_method_key,
                    // Round 16 (lane matrix): the OSR route's body is entered
                    // at a loop header, past every pre-header hoist.
                    osr_route: req.osr_optimizing_route,
                },
            );
            drop(metrics_optimize);
            metrics.phase_nodes(
                metrics::Phase::Optimize,
                metrics_nodes_before_optimize,
                graph.nodes.len(),
            );
            // Round 11 wave 16 (lane irback): in release builds too, unless
            // `CRATONVM_JIT_VERIFY_POST_OPTIMIZE=0` or `CRATONVM_JIT_VERIFY_IR=0`
            // (`r11-irback-release-builds-verify-structure-only`, item 1).
            if ir_verify::post_optimize_verify_enabled() {
                ir_verify_bail |= ir_verify_reject(
                    &graph,
                    // The one hook that runs BEFORE `apply_ea_to_ir`, which
                    // is what makes its extra lanes safe; the name is the
                    // constant `VerifyOptions::for_phase` matches on, so the
                    // two cannot drift.
                    ir_verify::PHASE_POST_OPTIMIZE,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
            }

            // Guard-surviving scalar replacement (Front 3.2): metadata for
            // scalar-replaced objects so the IR lowerer can emit a
            // `FrameValue::VirtualObject` at a deopt point. Populated from EA
            // below whenever `scalar_deopt_descriptor_available()` (precise
            // resume on, the default); otherwise stays `None`.
            let mut sr_map: Option<ir_lower::ScalarReplacementMap> = None;

            // wire-tiered-manager Step 4 (PGO handoff C1 → C2): hand the
            // optimizing IR (C2) lowerer the profiled branch bias so it can
            // pick each `Op::If`'s fall-through edge from the C1/interpreter
            // profile — the IR analogue of the single-pass backend's
            // `branch_hints`. Keyed by the branch instruction's bytecode PC
            // (matching `Op::If::bytecode_pc` and the interpreter's
            // `record_branch` PC). Empty when there is no profile (profiling
            // off, the default) → byte-identical codegen.
            //
            // HOISTED above escape analysis (it used to be computed just
            // before `lower_inner`) because EA is its second consumer: it is
            // the cold-path producer `escape_analysis::Graph::cold_nodes`
            // never had — see `ea_cold_control_nodes`. Same expression, same
            // value; only the position moved, and it reads nothing the
            // passes below mutate (`profile` is a parameter).
            let ir_branch_hints: std::collections::HashMap<usize, bool> = profile
                .map(|prof| {
                    prof.branches
                        .iter()
                        .filter_map(|(&pc, counts)| {
                            if counts.is_usually_taken() {
                                Some((pc, true))
                            } else if counts.is_usually_not_taken() {
                                Some((pc, false))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            // No "had monitors" latch any more. It existed because frame
            // states carried no monitor stack, so a guard deopt inside
            // `synchronized (new Object()) { ... }` whose monitors lock
            // elision had deleted resumed precisely with no lock held and the
            // interpreter's `monitorexit` threw
            // IllegalMonitorStateException. Snapshots now record the monitor
            // stack, and `ir_lower::Lowerer::resolve_monitors` marks an
            // elided lock `relock`, which the resume re-acquires on the
            // materialized object. See
            // `ir-frame-states-carry-no-monitor-stack-FIXED-20260912.md`.

            // --- Escape analysis (Phase 41 + G46 wiring) ---
            // Convert IR graph to escape analysis graph, run analysis,
            // and apply scalar replacement / lock elision to the IR graph.
            {
                // Phase 4 (escape analysis). Dropped explicitly before the
                // post-EA verifier run below so the two phases do not
                // double-charge each other; on the path where EA changes
                // nothing it drops at the end of this block instead, which
                // is still EA-only work.
                note_jit_pipeline_stage(JIT_STAGE_EA);
                // Round 16 lane alloc: a `new` whose only escape is one edge
                // of the branch ending its block is re-created on that edge
                // and its loads forwarded, before escape analysis looks
                // (`ea_ir_bridge::ir_sink_allocations_to_escaping_edge`;
                // `CRATONVM_JIT_IR_ALLOC_SINK_TO_EDGE=1`, default OFF).
                if ir_alloc_sink_to_edge_enabled()
                    && ir_sink_allocations_to_escaping_edge(&mut graph, &ir_spliced_ranges) > 0
                    && ir_verify::verify_enabled()
                {
                    ir_verify_bail |= ir_verify_reject(
                        &graph,
                        "post-escape-analysis",
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                    );
                }
                let metrics_ea = metrics.phase(metrics::Phase::EscapeAnalysis);
                let metrics_nodes_before_ea = graph.nodes.len();
                // ── ESCAPE ANALYSIS IS ITERATED ──────────────────────
                //
                // Scalar replacement EXPOSES scalar replacement. The shape
                // that forced this is a wrapper object holding a small
                // array: the array's only non-element use is the `putfield`
                // that publishes it into the wrapper, so round 1 refuses it
                // (`StoredIntoAnotherObject`) while replacing the wrapper.
                // Round 1's applier then marks that store dead and forwards
                // every read of the field to the array node itself -- so in
                // round 2 the array has nothing but element accesses left
                // and is replaceable. One pass could only ever delete one of
                // the two allocations `new Short2()` makes.
                //
                // Each round is the same sound analysis run on a graph the
                // previous round left sound, so nothing here relaxes a
                // proof; the loop only gives the existing proof a second
                // look at a smaller problem. Bounded by `MAX_EA_ROUNDS`,
                // and it stops as soon as a round applies nothing -- which
                // is the overwhelmingly common case on the first round, so
                // a method with no replaceable allocation pays exactly one
                // analysis, as before.
                const MAX_EA_ROUNDS: usize = 4;
                // Allocations an already-built descriptor names as another
                // object's field value. A later round must not delete one:
                // the recipe would be left naming a node the lowerer cannot
                // resolve. Only ever grows.
                let mut descriptor_pinned: std::collections::HashSet<ir::NodeId> =
                    std::collections::HashSet::new();
                // JIT round 21 wave 3 (lane ea, EA21-2; `CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD`,
                // default ON since r22 A/B, `=0` kills): boxed accumulators carried through φs
                // become primitives; a deopt rebuilds a named box through `valueOf` (auto-box
                // recipe in `sr_map`). Its recipe values are pinned for the rounds below.
                // t1 wave 3, lane M: a folded box is a scalar-replaced object
                // too; none while an object observer may exist.
                if ir_box_phi_web_fold_enabled()
                    && !(req.scalar_observer_at_compile
                        && escape_analysis::ea_agent_no_scalar_replacement_enabled())
                {
                    let webs =
                        ir_fold_box_phi_webs(&mut graph, &mut sr_map, &mut descriptor_pinned);
                    if webs > 0 {
                        ir_optimize::post_box_fold_cleanup(&mut graph, &mut 0);
                        if ir_verify::verify_enabled() {
                            ir_verify_bail |= ir_verify_reject(
                                &graph,
                                "post-escape-analysis",
                                &cached.class_name,
                                &cached.method_name,
                                &cached.method_descriptor,
                            );
                        }
                    }
                }
                for _ea_round in 0..MAX_EA_ROUNDS {
                    let applied = {
                        // JIT round 21 wave 3 (lane ea, EA21-4;
                        // `CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD`, default OFF):
                        // an `instanceof` / passing `checkcast` of an exact
                        // allocation folds before the round looks, so it does
                        // not escape the object. Counted as applied work.
                        let typecheck_folds = ir_fold_typechecks_of_fresh_allocations(
                            &mut graph,
                            req.class_is_subclass_resolver,
                        );
                        // t1 wave 8, lane EA (`CRATONVM_JIT_EA_ROUND_NULL_COMPARE_FOLD`,
                        // default ON since j27 2026-10-28): a null compare the previous round
                        // forwarded onto a fresh allocation folds before this
                        // round looks, so it does not escape the object.
                        let typecheck_folds = typecheck_folds
                            + ir_fold_null_compares_of_fresh_allocations_in_ea_round(&mut graph);
                        // JIT round 23 wave 2b, lane L1 (churn page J3;
                        // `CRATONVM_JIT_EA_DEAD_ARRAY_ELIM`, default ON): a
                        // length read the previous round forwarded to a fresh
                        // array folds to its constant, so this round's question
                        // sees an array nothing uses and deletes it. Counted as
                        // applied work.
                        let fresh_length_folds =
                            ir_optimize::fold_fresh_array_lengths_in_ea_round(&mut graph);
                        // Rebuilt every round: the previous round marked nodes
                        // dead and rewired consumers, so last round's EA graph
                        // and id_map describe a graph that no longer exists.
                        let (mut ea_graph, id_map) = escape_analysis_from_ir(&graph);
                        let mut round_applied = 0usize;
                        // Cold-path information (docs/jit/escape-analysis.md §6.3).
                        // Without this `EscapeAnalysisResult::partial_escapes` is
                        // always empty and `EscapeState::PartialEscape` is dead. It
                        // is additive and reported-only: no acting consumer reads a
                        // refined state, so an empty `ir_branch_hints` (profiling
                        // off, the default) leaves the analysis byte-identical.
                        ea_mark_cold_from_branch_hints(
                            &mut ea_graph,
                            &graph,
                            &id_map,
                            &ir_branch_hints,
                        );
                        let mut ea_result = escape_analysis::analyze_escapes(&ea_graph);
                        // t1 wave 2, lane M (`CRATONVM_JIT_EA_MONITOR_AGENT_KEEPS_LOCKS`,
                        // default ON): a monitor observer may exist, so no lock
                        // of a non-escaping object is elided (its owned-monitor
                        // answers must list it, as HotSpot's escape barrier does).
                        if req.monitor_observer_at_compile
                            && escape_analysis::ea_monitor_agent_keeps_locks_enabled()
                        {
                            escape_analysis::withhold_lock_elisions(&mut ea_result);
                        }
                        // t1 wave 3, lane M (`CRATONVM_JIT_EA_AGENT_NO_SCALAR_REPLACEMENT`,
                        // default ON): an object observer (JDWP, or a C agent
                        // reading locals, owned monitors or walking the heap)
                        // must see every allocation, as HotSpot's escape
                        // barrier reallocates it.
                        if req.scalar_observer_at_compile
                            && escape_analysis::ea_agent_no_scalar_replacement_enabled()
                        {
                            escape_analysis::withhold_scalar_replacements(&mut ea_result);
                        }
                        // JIT round 21 wave 2 (lane ea, EA21-1;
                        // `CRATONVM_JIT_EA_REF_COMPARE_FOLD`, default ON since 2026-10-25): fold
                        // each `acmp` of a confined allocation against a
                        // reference that cannot be it. Counted as applied work,
                        // so the next round sees the object without the
                        // identity observation. Empty with the switch off.
                        let ref_compare_folds = ir_fold_confined_ref_compares(
                            &mut graph,
                            &id_map,
                            &ea_result.ref_compare_folds,
                        );
                        // Live-fire soak diagnostic (CRATONVM_DBG_SCALAR_NEW): for an
                        // allocation-bearing method, report how many of its `new`s
                        // escape analysis scalar-replaced. This proves the path is
                        // actually exercised on real bytecode (a non-vacuous soak):
                        // `scalar_replaceable < ir_news` means some `new` escaped and
                        // the method will bail to single-pass via the surviving-New
                        // gate below.
                        if false {
                            let ir_news = graph
                                .nodes
                                .iter()
                                .filter(|n| {
                                    matches!(n.op, ir::Op::New { .. } | ir::Op::NewArray { .. })
                                })
                                .count();
                            if ir_news > 0 {
                                eprintln!(
                                    "[cratonvm-scalarnew] {}.{}{}: scalar-replaced {}/{} alloc(s)",
                                    cached.class_name,
                                    cached.method_name,
                                    cached.method_descriptor,
                                    ea_result.scalar_replaceable.len(),
                                    ir_news,
                                );
                                // A count says how many objects survived; it does
                                // not say which fact kept them. Report EVERY
                                // allocation with its verdict, so a `0/N` is a lead
                                // rather than the start of a guessing round -- and
                                // so an allocation the analysis never even
                                // CONSIDERED is visible rather than absent.
                                let replaced: std::collections::HashSet<usize> = ea_result
                                    .scalar_replaceable
                                    .iter()
                                    .map(|i| i.alloc_node)
                                    .collect();
                                for (ea_id, ea_node) in ea_graph.nodes.iter().enumerate() {
                                    let kind = match ea_node.op {
                                        escape_analysis::Op::New { .. } => "new",
                                        escape_analysis::Op::NewArray { .. } => "newarray",
                                        _ => continue,
                                    };
                                    if replaced.contains(&ea_id) {
                                        // Value φs: loop-carried field answers
                                        // (`ea_loop_carried_fields_enabled`).
                                        let phis = ea_result
                                            .scalar_replaceable
                                            .iter()
                                            .find(|i| i.alloc_node == ea_id)
                                            .map_or(0, |i| i.value_phis.len());
                                        eprintln!(
                                            "[cratonvm-scalarnew]   {} node {}: REPLACED value_phis={}",
                                            kind, ea_id, phis,
                                        );
                                    } else if let Some((_, reason)) =
                                        ea_result.scalar_refusals.iter().find(|(a, _)| *a == ea_id)
                                    {
                                        eprintln!(
                                            "[cratonvm-scalarnew]   {} node {}: refused {:?}",
                                            kind, ea_id, reason,
                                        );
                                        // al16: the uses that hold it, with
                                        // their IR op and bytecode pc.
                                        for &u in &ea_node.uses {
                                            let ir = id_map.iter().position(|&e| e == u);
                                            let what = ir
                                                .and_then(|i| graph.nodes.get(i))
                                                .map(|n| {
                                                    // JIT r23 wave 9 (lane W9-L2): name a
                                                    // call's callee, so an escape through a
                                                    // call reads without a disassembly.
                                                    let callee = match &n.op {
                                                        ir::Op::Call { info_ptr, .. }
                                                            if *info_ptr != 0 =>
                                                        {
                                                            // SAFETY: as every other read of an
                                                            // `Op::Call`'s `info_ptr`: a live
                                                            // `JitInvokeInfo` owned by this
                                                            // compile's `ir_call_infos`.
                                                            let i: &JitInvokeInfo = unsafe {
                                                                &*(*info_ptr
                                                                    as *const JitInvokeInfo)
                                                            };
                                                            format!(
                                                                " -> {}.{}{}",
                                                                i.class_name,
                                                                i.method_name,
                                                                i.descriptor
                                                            )
                                                        }
                                                        _ => String::new(),
                                                    };
                                                    format!(
                                                        "{:?} pc={:?}{callee}",
                                                        n.op, n.bytecode_pc
                                                    )
                                                })
                                                .unwrap_or_default();
                                            let ea_op = ea_graph.nodes.get(u).map(|n| &n.op);
                                            eprintln!(
                                                "[cratonvm-scalarnew]     use {u} {ea_op:?}: ir {ir:?} {what}"
                                            );
                                        }
                                    } else {
                                        eprintln!(
                                            "[cratonvm-scalarnew]   {} node {}: NOT CONSIDERED",
                                            kind, ea_id,
                                        );
                                    }
                                }
                            }
                        }
                        // Gated on `lock_elisions`, not the flat `elide_locks`, for
                        // the same reason `apply_ea_to_ir` reads the grouped view:
                        // one source of truth. The two are the same offers (the flat
                        // list is their union), so this is not a behaviour change.
                        // A round whose only offer is a coarsening plan must reach
                        // the applier too, or `CRATONVM_JIT_LOCK_COARSEN=1` computes
                        // plans and drops them; with the switch off the list is
                        // always empty (r11w4-mid-lock-coarsening-alone).
                        if !ea_result.scalar_replaceable.is_empty()
                            || !ea_result.lock_elisions.is_empty()
                            || !ea_result.lock_coarsening.is_empty()
                        {
                            // Capture guard-surviving-SR metadata (gated) BEFORE
                            // `apply_ea_to_ir` marks the News/stores dead and clears
                            // their (control) inputs — the dominance gate needs them.
                            //
                            // The builder's plans go to the applier as they are
                            // (round 11 wave 7), so each candidate is planned once
                            // per round rather than twice; see
                            // `apply_ea_to_ir_planned`. `None` (descriptors off, or
                            // nothing to describe) lets the applier plan.
                            let mut round_plans = None;
                            if scalar_deopt_descriptor_available()
                                && !ea_result.scalar_replaceable.is_empty()
                            {
                                let (round_map, plans) = build_scalar_replacement_map_with_plans(
                                    &graph,
                                    &id_map,
                                    &ea_result,
                                    &descriptor_pinned,
                                );
                                round_plans = Some(plans);
                                // Every node a recipe names as a field value is
                                // pinned for every later round -- see
                                // `plan_scalar_replacement`'s pin check.
                                for vo in round_map.objects.values() {
                                    for fv in vo.field_values.iter().flatten() {
                                        descriptor_pinned.insert(*fv);
                                    }
                                    // Per-store values too: a deopt may name one
                                    // (r11w5-lower-virtual-object-store-fields-patch).
                                    for v in
                                        vo.store_fields.iter().flatten().filter_map(|(_, v)| *v)
                                    {
                                        descriptor_pinned.insert(v);
                                    }
                                }
                                // MERGE, not replace: a round only describes the
                                // objects IT replaced, and the lowerer needs
                                // every round's.
                                match sr_map.as_mut() {
                                    Some(acc) => acc.objects.extend(round_map.objects),
                                    None => sr_map = Some(round_map),
                                }
                            }
                            round_applied = apply_ea_to_ir_planned_described(
                                &mut graph,
                                &id_map,
                                &ea_result,
                                &descriptor_pinned,
                                round_plans,
                                sr_map.as_mut(),
                            );
                            // `apply_ea_to_ir` is a mutating pass: it kills the
                            // scalar-replaced allocation, its stores and its loads,
                            // and rewires their consumers. Verify the result under
                            // the same per-pass gate as `ir_optimize`.
                            if ir_verify::verify_enabled() {
                                ir_verify_bail |= ir_verify_reject(
                                    &graph,
                                    "post-escape-analysis",
                                    &cached.class_name,
                                    &cached.method_name,
                                    &cached.method_descriptor,
                                );
                            }
                        }
                        round_applied + ref_compare_folds + typecheck_folds + fresh_length_folds
                    };
                    if applied == 0 {
                        break;
                    }
                }
                // Again after the rounds: a box an object escape analysis
                // removed was holding (a varargs `Object[]` element) is read
                // back only by its unbox now, and the array's recipe names it.
                if ir_box_phi_web_fold_enabled()
                    && !(req.scalar_observer_at_compile
                        && escape_analysis::ea_agent_no_scalar_replacement_enabled())
                {
                    let webs =
                        ir_fold_box_phi_webs(&mut graph, &mut sr_map, &mut descriptor_pinned);
                    if webs > 0 && ir_verify::verify_enabled() {
                        ir_verify_bail |= ir_verify_reject(
                            &graph,
                            "post-escape-analysis",
                            &cached.class_name,
                            &cached.method_name,
                            &cached.method_descriptor,
                        );
                    }
                }
                // Stop charging EA here: the verifier runs below are their
                // own phases and must not be billed to escape analysis.
                drop(metrics_ea);
                // Recorded on BOTH paths — EA that scalar-replaced nothing
                // still ran, and `nodes_before == nodes_after` is the
                // finding, not a missing measurement.
                metrics.phase_nodes(
                    metrics::Phase::EscapeAnalysis,
                    metrics_nodes_before_ea,
                    graph.nodes.len(),
                );
            }
            // #49: escape analysis was the last mutator, so which snapshots
            // a deopt can consult is now final. Drop the others — they would
            // still become deopt points naming the allocations the planner
            // just removed. See `ir_prune_unconsumable_snapshots`.
            // Round 13 wave 6 (lane chain2): told which removed allocations
            // the lowerer can describe, so a chain naming only those survives
            // under `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS`.
            ir_prune_unconsumable_snapshots_described(
                &mut graph,
                &ir_spliced_ranges,
                sr_map.as_ref(),
            );
            // JIT round 19 (lane splice19, CH16-4): what the builder recorded
            // and what survived the prune, for the census line below.
            let chain_census = chain_census_t0.map(|t0| {
                let kept = graph
                    .safepoints
                    .iter()
                    .filter(|s| s.is_guard_splice_state())
                    .count();
                (t0, graph.chain_snapshot_census, kept)
            });

            // cov-06: array allocations are now supported directly by the
            // optimizing tier through the shared `emit_new_array_stub`,
            // the same way escaping object allocations already are —
            // a live `Op::NewArray` no longer forces a fall-through to
            // the single-pass backend. A SURVIVING one, that is: a small
            // constant-length primitive array can now be scalar-replaced
            // like an object and is `Op::Dead` by this point, exactly as a
            // replaced `Op::New` is — see `escape_analysis.rs`.
            //
            // Unconditional pre-lowering verification. This is the gate the
            // review's exit criterion names: "invalid IR or ABI state
            // causes a deterministic compilation bailout, never silent
            // wrong code, panic, or native crash". It runs BEFORE
            // `ir_schedule::schedule`, not just before `lower_inner`,
            // because the scheduler indexes `graph.nodes` by raw `NodeId`
            // and would itself panic on the dangling edge the verifier is
            // there to catch. `CRATONVM_JIT_VERIFY_IR=0` is the kill switch
            // — see `ir_verify::pre_lower_verify_disabled`.
            if !ir_verify_bail && !ir_verify::pre_lower_verify_disabled() {
                // With `sr_map`'s keys, so the frame-state lane can accept an
                // eliminated allocation's descriptor slot and name every other
                // removed node a snapshot still reads (round 11 wave 18,
                // `r11-irback-release-builds-verify-structure-only`).
                ir_verify_bail |= ir_verify_reject_pre_lower(
                    &graph,
                    sr_map.as_ref(),
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
            }
            if !ir_verify_bail {
                // The graph the lowerer will actually see. `live_nodes` is
                // the non-`Op::Dead` count: the gap against `nodes` is dead
                // arena the optimizer left behind. That gap no longer costs
                // frame bytes — `ir_lower::plan_slots` colours by live range
                // and only nodes that actually take a slot get one — so it is
                // now a statement about the optimizer's leftovers alone.
                metrics.set_graph_at_lower(
                    graph.nodes.len(),
                    graph
                        .nodes
                        .iter()
                        .filter(|n| !matches!(n.op, ir::Op::Dead))
                        .count(),
                    graph.safepoints.len(),
                );
                // Phase 6 (schedule).
                note_jit_pipeline_stage(JIT_STAGE_SCHEDULE);
                let metrics_schedule = metrics.phase(metrics::Phase::Schedule);
                // Hand the scheduler the PROFILE, not just the static
                // heuristics. `production_schedule_options` turns on
                // frequency-driven block layout and critical-path list
                // scheduling; both work from `static_branch_probs` when
                // `branch_counts` is empty, and both work BETTER when it is
                // not. The map is keyed by the `Op::If`'s bytecode pc,
                // which is the same key `ir_branch_hints` above is built
                // from, so the two cannot disagree about a site.
                //
                // Empty in a default run today, because branch recording
                // sits behind `CRATONVM_TIER_PGO` -- see
                // `arm_branch_profiling_for_c2`, which is what fills it.
                let mut sched_opts = ir_schedule::production_schedule_options();
                if let Some(prof) = profile {
                    for (&pc, counts) in prof.branches.iter() {
                        sched_opts.branch_counts.insert(
                            pc,
                            ir_schedule::BranchBias {
                                // `BranchCounts` is `u32` (an interpreter
                                // counter); `BranchBias` is `u64` (a
                                // scheduler weight). Widening, never
                                // truncating.
                                taken: u64::from(counts.taken),
                                not_taken: u64::from(counts.not_taken),
                            },
                        );
                    }
                }
                // Values a scalar-replaced object's materialization reads from
                // their homes although no frame names them (the eliminated
                // stores' values): keep them ahead of the deopt that reads
                // them (round 11 wave 7,
                // `r11w6-lower-sr-recipe-values-scheduled-after-the-deopt`;
                // patch page `r11w7-ircore-lib-rs-sr-recipe-values-patch`).
                if let Some(sr) = sr_map.as_ref() {
                    sched_opts
                        .extra_frame_named
                        .extend(ir_lower::scalar_replacement_field_values(sr));
                }
                // JIT round 23 wave 3b (lane L1, L1W3-1;
                // `CRATONVM_JIT_IR_LOOP_CARRIED_REGS`, default ON): a loop's
                // carried integers stay in registers across the back edge.
                graph.loop_carried_regs = ir_lower::ir_loop_carried_regs_enabled();
                // JIT r25 wave 10 (W10-2b): the single-pass body's context ABI
                // as a floor, so a supersede can forward its baked callers.
                graph.context_abi_floor = r25_w10_2_steady_c1::context_abi_floor(scan.needs_heap);
                // t1 wave 9 (lane LCAP): and the context form a string/class
                // `ldc` gives it (`CRATONVM_JIT_IR_CONTEXT_ABI_FLOOR_COUNTS_LDC`).
                graph.context_abi_floor |=
                    r25_w10_2_steady_c1::ldc_context_abi_floor(ir_ldc_takes_context);
                // t1 wave 10 (lane LCAP2): and the published body's real ABI,
                // every driver reason at once (`CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED`).
                graph.context_abi_floor |= r25_w10_2_steady_c1::installed_context_abi_floor(
                    req.installed_body_needs_context,
                );
                // JIT round 23 wave 3d (lane L1): the VM-facing twins of the
                // receiver-carrying direct self-calls (`Op::Call`'s kind-4 arm).
                graph.self_call_service_infos = ir_self_call_service_infos.clone();
                let schedule = ir_schedule::schedule_with_options(&graph, &sched_opts);
                drop(metrics_schedule);
                // SSA's one structural invariant, checked on the FINISHED
                // schedule: every read's definition dominates it (terminators
                // and φ edges included). `ir_lower::verify_data_locations`
                // compares EMISSION order only, so a use in a join of a value
                // one arm computed passes it whenever that arm is laid out
                // first -- and the lowered code then reads a slot the other
                // path never wrote. The scheduler's own incomparable-input
                // fallback produces exactly that shape. Refused here, with the
                // first offending node named, before anything is lowered or
                // published; the single-pass backend compiles the method.
                // One linear pass with O(1) dominance queries. Shares the
                // pre-lower verifier's kill switch (`CRATONVM_JIT_VERIFY_IR=0`,
                // `ir_verify::pre_lower_verify_disabled`) rather than adding one.
                let schedule_misplaced = if ir_verify::pre_lower_verify_disabled() {
                    None
                } else {
                    schedule.misplaced_reads(&graph).first().copied()
                };
                if let Some(read) = schedule_misplaced {
                    // Name the READ, not only the node (irback proposal W7-2):
                    // which input, which definition and, for a phi, which edge.
                    let describe = |id: ir::NodeId| match graph.nodes.get(id as usize) {
                        Some(n) => {
                            format!("n{id} {:?}:{:?} @bci {:?}", n.op, n.ty, n.bytecode_pc)
                        }
                        None => format!("n{id}"),
                    };
                    let edge = read
                        .edge
                        .map(|e| format!(" on the edge from n{e}"))
                        .unwrap_or_default();
                    let b = bailout::Bailout::with_context(
                        bailout::BailoutReason::IrVerification(format!(
                            "post-schedule: {} input[{}] = {}{edge} is read where its \
                             definition does not dominate",
                            describe(read.node),
                            read.slot,
                            describe(read.def),
                        )),
                        "post-schedule",
                    );
                    bailout::record_bailout(&b);
                    metrics::note_current_bailout(&b, "post-schedule");
                    if ir_stage_reporting() {
                        eprintln!(
                            "[ir] schedule rejected {}.{}{}: {b}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                }
                // Supply BOTH the profiled branch hints (Step 4) and the
                // guard-surviving scalar-replacement map (Front 3.2) to the
                // shared lowering body. `sr_map` is `None` when precise resume
                // is off (`scalar_deopt_descriptor_available`).
                // Phase 7 (lower). `lower_inner` selects instructions,
                // encodes them and installs the executable buffer in one
                // call, which is why `Phase::Encode` and `Phase::Install`
                // report "not measured" for this path rather than 0.
                // Hoisted out of the `if let` (same call, same arguments,
                // same control flow) purely so the timer can stop before
                // the post-lowering bookkeeping below.
                // The drift witness, optimizing half. `lower_inner`
                // installs an executable buffer itself and never goes
                // through `x64::compile_with_param_slots`, so without this
                // the witness covered only the single-pass backend — a
                // future door reaching the IR lowerer directly would have
                // produced a body with no admission and no count. This path
                // is reachable only from the gated
                // `try_compile_with_invokespecial_resolver`, so it is
                // expected to add nothing to `ungated_backend_entries()`;
                // the point is that it would stop being true if that
                // changed.
                compile_gate::note_backend_entry();
                // The graph the lowerer is about to see, node by node
                // (`CRATONVM_DBG_IR_GRAPH=1`). This tier had node COUNTS
                // (`CRATONVM_DBG_IR_SLOTS`) and per-pass verdicts, and no way
                // to see the graph itself — so every question of the form
                // "which node does this consumer actually read" was answered
                // by adding a temporary `eprintln` and rebuilding.
                //
                // Printed after every mutating pass (`ir_optimize`, the
                // escape-analysis rounds, the scheduler), i.e. at the last
                // moment the graph is still the compiler's rather than the
                // backend's — which is where a forwarding or elision defect
                // is visible and a disassembly no longer separates it from a
                // lowering one.
                if false {
                    // Built whole and written once (backlog MX-B1, round 16
                    // matrix): one `eprintln!` per line let another compile
                    // thread's listing interleave with this one.
                    use std::fmt::Write as _;
                    let mut out = String::new();
                    let _ = writeln!(
                        out,
                        "[ir-graph] {}.{}{} — {} node(s), entry={} exit={}",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        graph.nodes.len(),
                        graph.entry,
                        graph.exit,
                    );
                    for (id, n) in graph.nodes.iter().enumerate() {
                        if n.op == ir::Op::Dead {
                            continue;
                        }
                        let _ = writeln!(
                            out,
                            "[ir-graph]   {id:3}: {:?} : {:?} <- {:?}  bci={:?}",
                            n.op,
                            n.ty,
                            n.inputs.as_slice(),
                            n.bytecode_pc,
                        );
                    }
                    for (i, sp) in graph.safepoints.iter().enumerate() {
                        let _ = writeln!(
                            out,
                            "[ir-graph]   safepoint[{i}] bci={} locals={:?} stack={:?}",
                            sp.bci, sp.locals, sp.stack,
                        );
                    }
                    eprint!("{out}");
                }
                // JIT round 21 (lane lambda3): how each planned lambda SAM call
                // was built, and whether its proxy's allocation survived.
                if !ir_lambda_impl.is_empty() && (ir_stage_reporting() || x64::dbg_jitc_enabled()) {
                    for line in
                        ir_lambda_impl.census_lines(&graph, &ir_direct_calls, &ir_cycle_cells)
                    {
                        eprintln!(
                            "[ir] lambda-impl-site {}.{}{} {line}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                }
                // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2 /
                // LAMBDA321-1): the same question for the SAM calls built as
                // their impl's splice inside a spliced body.
                if !graph.lambda_impl_splice_rows.is_empty()
                    && (ir_stage_reporting() || x64::dbg_jitc_enabled())
                {
                    for &(pc, indy_pc, captures) in &graph.lambda_impl_splice_rows {
                        let kept = graph.nodes.iter().any(|n| {
                            n.bytecode_pc == Some(indy_pc)
                                && matches!(n.op, ir::Op::New { class_id, .. }
                                    if graph.lambda_proxy_alloc_classes.contains(&class_id))
                        });
                        let proxy = match (captures, kept) {
                            (0, _) => "singleton",
                            (_, true) => "kept",
                            (_, false) => "elided",
                        };
                        eprintln!(
                            "[ir] lambda-impl-in-splice-site {}.{}{} pc={pc} indy_pc={indy_pc} \
                             captures={captures} call=spliced proxy={proxy}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                }
                note_jit_pipeline_stage(JIT_STAGE_LOWER);
                let metrics_lower = metrics.phase(metrics::Phase::Lower);
                ir_lower::clear_lower_bail();
                // Round 9 wave 9 (irl9 cross-lane request 2, applied by the
                // `lib.rs` owner, `evict9`): THIS compile's ArrayList layout,
                // which arms the IR tier's ARRAYLIST_ACCESS prefix. Since
                // nio20 wave 3 (L2) the pin is released by default
                // (`ir_tier_lowers_arraylist_prefix`,
                // `CRATONVM_JIT_IR_ARRAYLIST_PREFIX=0` restores it), so prefix
                // sites reach this lowering.
                // Round 11 wave 10 (lane irexc): the method's protected ranges,
                // handed to the lowerer ONLY on the precise-frame route. This
                // tier is reached with `precise_exception_frames` set only when
                // `ir_precise_frames` admitted it, and the lowerer then puts a
                // reason-9 rethrow pad on every call-exception exit inside
                // them. Empty everywhere else, which is byte-identical to the
                // lowering before the pads existed.
                let ir_protected_ranges: Vec<(u32, u32)> = if precise_exception_frames {
                    cached
                        .exception_table
                        .iter()
                        .map(|row| (u32::from(row.start_pc), u32::from(row.end_pc)))
                        .collect()
                } else {
                    Vec::new()
                };
                // Round 11 wave 11 (lane irexc): where the method's handlers
                // lead, so the lowerer withholds an OSR entry a handler can
                // reach from a body with a rethrow pad
                // (`Lowerer::osr_entry_closed_by_a_handler`). The whole
                // (padded) buffer: its two trailing zero bytes decode as
                // unreachable `nop`s. Empty off the precise-frame route.
                let ir_osr_handler_reachable: Option<Vec<u32>> = if precise_exception_frames {
                    let handlers: Vec<u32> = cached
                        .exception_table
                        .iter()
                        .map(|row| u32::from(row.handler_pc))
                        .collect();
                    deopt::pcs_reachable_from_handlers(code, &handlers)
                } else {
                    Some(Vec::new())
                };
                let lowered = if schedule_misplaced.is_some() {
                    None
                } else {
                    // Interpreter round i1 wave 15 (lane L2): the optimizing
                    // OSR door's compile lets its unconditional back-edge
                    // polls leave for the interpreter
                    // (`CompileRequest::osr_mode_exit_polls`). Wave 18 (lane
                    // L2): so does every other compile of a method whose exit
                    // each stash sink resumes as a new frame
                    // (`ir_lower::ir_entry_mode_exits_admitted`). Wave 22
                    // (lane L2): an `ACC_SYNCHRONIZED` method too, unless its
                    // graph can neither trap nor call, the one body shape a
                    // caller-held sync-direct site binds
                    // (`ir_lower::ir_entry_poll_mode_exits`).
                    let poll_mode_exits = if req.osr_mode_exit_polls {
                        ir_lower::IrPollModeExits::OsrDoor
                    } else {
                        ir_lower::ir_entry_poll_mode_exits(
                            code,
                            code_len,
                            cached.is_synchronized,
                            &graph,
                        )
                    };
                    // Wave 18 (lane L2): the method's own bytecode and every
                    // handler edge, so a mode exit can call a local dead at
                    // its header (`ir_lower::PollExitBytecode`). The liveness
                    // itself is computed only if an exit asks.
                    let poll_exit_handlers: Vec<(usize, usize, usize)> =
                        if poll_mode_exits == ir_lower::IrPollModeExits::Off {
                            Vec::new()
                        } else {
                            cached
                                .exception_table
                                .iter()
                                .map(|row| {
                                    (
                                        usize::from(row.start_pc),
                                        usize::from(row.end_pc),
                                        usize::from(row.handler_pc),
                                    )
                                })
                                .collect()
                        };
                    // Not while a debugger may read locals (interpreter round
                    // i1 wave 19, lane L1): a dead local the body no longer
                    // holds would resume as `0` where the debugger reads the
                    // value, so its exit refuses instead
                    // (`CompileRequest::debugger_observes_locals`).
                    let poll_exit_bytecode = (poll_mode_exits != ir_lower::IrPollModeExits::Off
                        && !req.debugger_observes_locals)
                        .then_some(ir_lower::PollExitBytecode {
                            code,
                            code_len,
                            num_params,
                            num_locals: cached.max_locals as usize,
                            handlers: &poll_exit_handlers,
                        });
                    // gen r5w6/oomjit10: every exception-table row, whatever
                    // the poll mode, for the dead-reference rule's local
                    // liveness (a local only a handler reads is live).
                    let dead_ref_handlers: Vec<(usize, usize, usize)> =
                        if req.debugger_observes_locals {
                            Vec::new()
                        } else {
                            cached
                                .exception_table
                                .iter()
                                .map(|row| {
                                    (
                                        usize::from(row.start_pc),
                                        usize::from(row.end_pc),
                                        usize::from(row.handler_pc),
                                    )
                                })
                                .collect()
                        };
                    // gcd d5/u: name the method the `[ir-slots]` lines that
                    // follow describe (they printed only node and colour
                    // numbers, so a run could not tell one compile's plan
                    // from another's).
                    if false {
                        eprintln!(
                            "[ir-slots] method {}.{}{}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                    // nio20 wave 3 (L2): THIS compile's String layout, for the
                    // optimizing tier's inline String search prefix
                    // (`Lowerer::try_emit_ir_string_search_prefix`). Scoped to
                    // this one lowering call (both of its attempts) and
                    // restored on drop; see `CompileStringLayoutScope`.
                    let _ir_string_layout =
                        ir_lower::CompileStringLayoutScope::enter(resolved_string_layout);
                    // JIT round 21 wave 24 (lane flipprep, flip review item
                    // 2): whether a debugger or a JVMTI agent may read this
                    // compile's locals, for the fresh-allocation volatile-store
                    // fence elision (`CompileLocalsObservedScope`); same scope
                    // discipline as the String layout above.
                    let _ir_locals_observed = ir_lower::CompileLocalsObservedScope::enter(
                        req.debugger_observes_locals || req.jvmti_locals_accessible,
                    );
                    ir_lower::lower_inner_with_mode_exit_polls(
                        direct_helpers,
                        &graph,
                        &schedule,
                        num_params,
                        cached.max_locals as usize,
                        helpers,
                        &ir_branch_hints,
                        &ir_spliced_ranges,
                        sr_map.as_ref(),
                        &ir_direct_calls,
                        &ir_ic_slots,
                        &ir_compact_fields,
                        &ir_inline_frame_sites,
                        &ir_cycle_cells,
                        &ir_tlab_announce_classes,
                        resolved_string_layout.and_then(|l| l.array_list),
                        &ir_protected_ranges,
                        ir_osr_handler_reachable.as_deref(),
                        poll_mode_exits,
                        poll_exit_bytecode,
                        // gen r5w2/oomjit6: the method's own bytecode for the
                        // opt-in dead-reference-home clearing
                        // (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`), its
                        // store-free twin (gen r5w3/live7,
                        // `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`) and the
                        // default-on dead-home clears (gen r5w5/oomjit9,
                        // `CRATONVM_JIT_IR_DEAD_HOME_CLEARS=0` to disable),
                        // which the lowerer asks only when one of them is on.
                        // This door is the only caller that passes it, so every
                        // other lowering (the unit tests' included) is
                        // unchanged. Withheld
                        // while a debugger may read locals: it reads a frame
                        // through the same snapshots, and nothing is gained by
                        // arguing that case. gen r5w6/oomjit10: with the
                        // handler rows and the parameter / local counts the
                        // rule's bytecode local liveness needs.
                        (!req.debugger_observes_locals).then_some(ir_lower::DeadRefClearBytecode {
                            code: &code[..code_len],
                            method_descriptor: &cached.method_descriptor,
                            method_synchronized: cached.is_synchronized,
                            num_params,
                            num_locals: cached.max_locals as usize,
                            handlers: &dead_ref_handlers,
                            spliced_bodies: &ir_spliced_body_codes,
                        }),
                    )
                };
                drop(metrics_lower);
                // Round 14 wave 3 (lane fixup, RV3-1 of
                // `r14w3-review3-wave2-review-findings-FIXED-20260929.md`): a
                // synchronized splice's window the optimizer or the scheduler
                // made unsound (`Lowerer::sync_splice_window_refusal`) costs
                // that splice and one rebuild, not the method's optimizing
                // compile -- the W5-2 route a builder refusal already takes.
                if lowered.is_none() && ir_rebuild_armed {
                    if let Some(pc) = ir_lower::last_sync_window_refused_pc() {
                        *ir_rebuild_without =
                            ir_splice_rebuild_site(pc, code_len, &ir_spliced_ranges);
                    }
                }
                // r11w6-tier-optimizing-synchronized-body-trap-reruns-from-entry
                // (patch page `r11w7-lower-sync-body-vouch-lib-patch`): the
                // lowerer vouches for bodies with monitor BLOCKS only; it
                // cannot see ACC_SYNCHRONIZED. Every door that runs a wrapped
                // body holds the method monitor and hands it to the resumed
                // frame (`hand_monitor_to_resumed_frame`, `execute()`'s
                // `sync_monitor`), the promise the single-pass backend already
                // relies on for the same methods.
                let lowered = lowered.map(|mut cm| {
                    if cached.is_synchronized && !cm._deopt_point_boxes.is_empty() {
                        cm.can_deopt_resume = true;
                    }
                    cm
                });
                // Interpreter round i1 wave 22 (lane L2;
                // `i19-L1-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger`):
                // while a debugger may read locals, an artifact one of whose
                // resumable frames publishes a definitely assigned local
                // `Undefined` (resumed as `0`, where HotSpot's debugger reads
                // the value) is discarded, and the single-pass body, which keeps
                // and describes every assigned local under a debugger since
                // wave 21, runs instead. Same shape as the #49 backstop below;
                // nothing changes for a compile without a debugger.
                let lowered = match lowered {
                    Some(cm) if req.debugger_observes_locals => {
                        let handlers: Vec<(usize, usize, usize)> = cached
                            .exception_table
                            .iter()
                            .map(|row| {
                                (
                                    usize::from(row.start_pc),
                                    usize::from(row.end_pc),
                                    usize::from(row.handler_pc),
                                )
                            })
                            .collect();
                        let hides = ir_lower::ir_artifact_hides_an_assigned_local_scoped(
                            &cm,
                            ir_lower::PollExitBytecode {
                                code,
                                code_len,
                                num_params,
                                num_locals: cached.max_locals as usize,
                                handlers: &handlers,
                            },
                            Some((
                                ir_combined.as_deref().unwrap_or(code),
                                &ir_inline_frame_sites,
                            )),
                        );
                        if hides {
                            if ir_stage_reporting() {
                                eprintln!(
                                    "[ir] {}.{}{}: a resumable frame would show an assigned \
                                     local as 0 to the debugger -- keeping the single-pass body",
                                    cached.class_name, cached.method_name, cached.method_descriptor,
                                );
                            }
                            drop(cm);
                            None
                        } else {
                            Some(cm)
                        }
                    }
                    other => other,
                };
                // #49 backstop. The planner elides an allocation a
                // consultable snapshot names only when a recipe will exist,
                // and the lowerer can still refuse the recipe at one point
                // (a same-block order it cannot prove, an ambiguous deopt
                // block). That point is unresumable, and its sink re-runs the
                // method from entry — harmless for a body that commits no
                // side effect, a duplicated effect for one that does. For
                // that one, discard this artifact: the single-pass backend
                // keeps the allocation.
                let lowered = match lowered {
                    Some(cm)
                        if ir_artifact_names_an_undescribable_elided_object(&cm)
                            && bytecode_commits_side_effect(code, code_len) =>
                    {
                        if ir_stage_reporting() {
                            eprintln!(
                                "[ir] {}.{}{}: a deopt point names an elided allocation \
                                     it cannot describe in a side-effecting body -- keeping \
                                     the single-pass body",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        drop(cm);
                        None
                    }
                    other => other,
                };
                // Round 12 wave 3 (lane replay,
                // `r12w3-replay-lib-replay-fallback-post-check-patch`): a
                // point no sink resumes precisely falls back to a replay from
                // entry, which for a conditional deopt in a loop that stores
                // after its guard commits the store twice (or, at the
                // interpreter's sink, raises `InternalError`). Such a body is
                // discarded here and the single-pass backend keeps the method.
                // `sink_resumes` is the body-wide half of both sinks' gate,
                // taken in its weaker form (`jit_bridge` also needs
                // `CRATONVM_DEOPT_REAL`); the per-point half is checked inside.
                let lowered = match lowered {
                    Some(cm) => {
                        let sink_resumes = (cm.can_deopt_resume && deopt_real_enabled())
                            || (deopt_sink_resume_enabled()
                                && !cached.is_synchronized
                                && !bytecode_holds_monitor(code, code_len));
                        // Round 12 wave 7 (lane replay4): with the method's
                        // own monitor, so a frame the VM builder refuses for
                        // it (a synchronized frame naming a scalar-replaced
                        // object, `deopt::synchronized_frame_refuses_virtuals`)
                        // is asked the replay question instead of being taken
                        // as resumed.
                        match deopt::first_point_needing_unsound_replay_of_method(
                            cm._deopt_point_boxes.iter().map(|p| p.as_ref()),
                            code,
                            code_len,
                            ir_spliced_bodies_pure,
                            sink_resumes,
                            deopt::SinkMethodMonitor::of(cached.is_synchronized, cached.is_static),
                        ) {
                            Some(bci) => {
                                if ir_stage_reporting() {
                                    eprintln!(
                                        "[ir] {}.{}{}: the deopt at bci {bci} can only be \
                                         replayed from entry and the replay would commit a \
                                         side effect again -- keeping the single-pass body",
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor,
                                    );
                                }
                                drop(cm);
                                None
                            }
                            None => Some(cm),
                        }
                    }
                    None => None,
                };
                // Round 11 wave 10 (lane irexc): the post-lowering half of the
                // unresumable-trap waiver the precise-frame admission took (see
                // the admission conjunct and `ir_precise_frames_trap_post_check`).
                // Same shape as the #49 backstop above: an artifact one of
                // whose guard exits would not resume precisely is discarded,
                // and the single-pass backend keeps the method.
                let lowered = match lowered {
                    Some(cm)
                        if precise_exception_frames
                            && !ir_precise_frames_trap_post_check(
                                &cm,
                                code,
                                code_len,
                                cached.is_synchronized,
                            ) =>
                    {
                        if ir_stage_reporting() {
                            eprintln!(
                                "[ir] {}.{}{}: a guard exit of this precise-frame body \
                                 would not resume precisely -- keeping the single-pass body",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        drop(cm);
                        None
                    }
                    other => other,
                };
                // JIT round 23 wave 8 (lane W8-L1, proxy-define page;
                // `CRATONVM_JIT_IR_IN_PLACE_TRAP_FRAME_CHECK`, default ON): an
                // in-place unresolved type-check trap whose PUBLISHED frame
                // the precise sink cannot rebuild directly would fall back to
                // the replay rule, which refuses it with an `InternalError`;
                // such a body is discarded and the single-pass backend keeps
                // the method, as the builder's refusal would have.
                let in_place_refusal = match &lowered {
                    Some(cm)
                        if (ir::ir_unresolved_typecheck_in_place_enabled()
                            || ir::ir_unresolved_field_in_place_enabled())
                            && ir_in_place_trap_frame_check_enabled() =>
                    {
                        first_unrebuildable_in_place_trap(
                            cm,
                            &ir_unresolved_typecheck_trap_pcs(&graph),
                        )
                    }
                    _ => None,
                };
                let lowered = match lowered {
                    Some(cm) if in_place_refusal.is_some() => {
                        if ir_stage_reporting() {
                            // Wave 8c: which trap, and which rule.
                            let (bci, why) = in_place_refusal.unwrap_or_default();
                            eprintln!(
                                "[ir] {}.{}{}: an in-place type-check trap publishes a frame \
                                 the precise sink cannot rebuild (trap bci {bci}: {why}) -- \
                                 keeping the single-pass body",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        drop(cm);
                        None
                    }
                    other => other,
                };
                // Round 14 wave 1 (lane sync, W11-2 under
                // `ir_precise_frames_synchronized_enabled`): a compiled
                // caller's door enters a synchronized method's WRAPPED
                // optimizing body only when every exit that publishes no
                // frame lies outside the exception table
                // (`jit_bridge.rs` `optimizing_exits_route_exactly`); any
                // other body it refuses, and then INTERPRETS the template --
                // slower than the single-pass body this one would replace. So
                // such a body is discarded here and the single-pass backend
                // keeps the method. Unreachable with the switch off (a
                // synchronized method reaches this tier with precise frames
                // only through it).
                let lowered = match lowered {
                    Some(cm)
                        if cached.is_synchronized
                            && precise_exception_frames
                            && !cached.exception_table.is_empty()
                            && !cm.ir_osr_sentinel_free
                            && !cm
                                .ir_osr_exception_exit_bcis
                                .as_deref()
                                .is_some_and(|bcis| {
                                    bcis.iter().all(|&bci| {
                                        cached.exception_table.iter().all(|row| {
                                            !(u32::from(row.start_pc) <= bci
                                                && bci < u32::from(row.end_pc))
                                        })
                                    })
                                }) =>
                    {
                        if ir_stage_reporting() {
                            eprintln!(
                                "[ir] {}.{}{}: the synchronized door could not enter this \
                                 precise-frame body -- keeping the single-pass body",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
                        drop(cm);
                        None
                    }
                    other => other,
                };
                // The C1->C2 acceptance gate. A body that lowered but
                // applied no transform the baseline tier lacks is a
                // differently-emitted version of the same computation
                // carrying this tier's weaker register model -- so it is
                // DISCARDED here and the caller falls back to the
                // single-pass backend, exactly as it does for any other
                // refusal. See `ir_evidence` for the measurement that
                // motivates the default and for why the list is a judgment.
                let evidence = ir_evidence::take();
                // EXEMPTION, and it is not a special case so much as the
                // gate's own premise failing: "if C2 applied nothing C1
                // lacks, C1's body is at least as good" assumes there IS a
                // C1 body. `promote_scalar_selfrec_to_ir` reaches this tier
                // WITHOUT a predecessor -- deliberately, because compiling
                // the narrow `static int f(int)` self-recursion shape as C1
                // first strands recursive frames in the slower body -- and
                // `fib` is pure arithmetic, so it produces no evidence at
                // all. Refusing it would send it to single-pass, which is
                // the exact outcome that door exists to prevent.
                //
                // Asked with the SAME predicate the VM used to reach the
                // door (`scalar_selfrec_ir_would_engage`), so the two
                // cannot disagree about which methods it covers.
                let selfrec_no_predecessor =
                    scalar_selfrec_ir_would_engage(code, code_len, &cached.method_descriptor);
                // Publish the verdict for the VM's compile-task path,
                // which is the only caller that can act on it. See
                // `ir_evidence::take_last_verdict`.
                // Only a body that LOWERED can be accepted. A refusal inside
                // `lower_inner` (a verifier or balance check, buffer
                // exhaustion) used to publish `accepted` from the evidence
                // alone, so the VM went ahead with the supersede: the
                // single-pass fall-through replaced an equal C1 body and
                // bumped the process-wide supersede epoch for nothing.
                // JIT round 23 wave 4 (lane L1,
                // `CRATONVM_JIT_IR_SELFREC_FRAME_EVIDENCE`): a body with a
                // direct self-call whose frame is materially smaller than the
                // published single-pass body's -- deeper compiled recursion is
                // what it adds.
                let selfrec_frame = lowered.as_ref().is_some_and(|cm| {
                    ir_selfrec_frame_evidence(
                        // The rows are attached to the artifact only after
                        // the gate; read the compile's own arena.
                        ir_call_infos.iter().any(|i| i.invoke_kind == 4),
                        cm.osr_frame_size,
                        req.verdicts().sp_frame_size(ir_refusal_key),
                    )
                });
                let accepted = lowered.is_some()
                    && (selfrec_no_predecessor || selfrec_frame || ir_evidence::accept(evidence));
                // Round 16 (lane fieldguard): a body that LOWERED and that
                // this gate then discards is not a `lower_inner` refusal; the
                // closing report line below said "lower_inner refused
                // (unlabelled)" for it (`R16SyncSyncM$C.plainStaticStep`).
                let acceptance_discarded = lowered.is_some() && !accepted;
                // JIT r25 wave 9 (W9-3): a compile the chain rule spliced links
                // into says what became of it -- published, or discarded for
                // the single-pass body -- with its REAL frame beside the
                // estimate and the gate's own inputs.
                if ir_chain_census.links > 0 && x64::dbg_jitc_enabled() {
                    eprintln!(
                        "{}",
                        ir_chain_census.line(
                            &format!(
                                "{}.{}{}",
                                cached.class_name, cached.method_name, cached.method_descriptor
                            ),
                            lowered.is_some(),
                            accepted,
                            lowered.as_ref().map(|cm| cm.osr_frame_size),
                            evidence,
                        )
                    );
                }
                // JIT round 17 wave 3 (lane irlower, IRFEED17-1 / MON3R17-3):
                // an accepted body named why, beside the refused line below.
                // Diagnostics only (`CRATONVM_DBG_IR_COMPILES` /
                // `CRATONVM_DBG_JITC`).
                if accepted && ir_stage_reporting() {
                    eprintln!(
                        "[ir] acceptance {}.{}{}: accepted (evidence: {}{}) [{}]",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        ir_evidence::describe_record(evidence),
                        if selfrec_no_predecessor {
                            "; selfrec exemption"
                        } else if selfrec_frame {
                            "; selfrec frame smaller than single-pass"
                        } else {
                            ""
                        },
                        // t1 w11 JACC (ACCEPT21-5 first step): the in-loop
                        // guards' frame-state live set. Diagnostic only.
                        ir_evidence::guard_frame_census(&graph, &schedule).describe(),
                    );
                }
                ir_evidence::publish_verdict(accepted);
                // JIT round 21 lane tiermgr (ACCEPT21-1 first step,
                // `CRATONVM_DBG_ACCEPT_CENSUS`): what the evidence gate would
                // answer under every policy, beside what happened to the body.
                // Diagnostic only.
                if ir_evidence::accept_census_enabled() {
                    if let Some(cm) = lowered.as_ref() {
                        eprintln!(
                            "{}",
                            ir_evidence::accept_census_line(
                                &format!(
                                    "{}.{}{}",
                                    cached.class_name, cached.method_name, cached.method_descriptor
                                ),
                                evidence,
                                accepted,
                                selfrec_no_predecessor || selfrec_frame,
                                cm.code_bytes().len(),
                                req.osr_optimizing_route,
                            )
                        );
                    }
                }
                let lowered = match lowered {
                    Some(cm) if !accepted => {
                        ir_evidence::mark_ir_exit(ir_evidence::AdmissionExit::Acceptance);
                        // Round 15 wave 4 (lane sync3, SY2-6): a self-locking
                        // body the closed-exits check passed, declined here.
                        if cm.self_locks_monitor {
                            ir::note_sync_splice_census(ir::SYNC_CENSUS_IR_SELF_LOCK_GATE_DECLINED);
                        }
                        if ir_stage_reporting() {
                            eprintln!(
                                "[ir] acceptance {}.{}{}: REFUSED (evidence: {}) -- keeping the single-pass body [{}]",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                // JIT round 17 wave 3 (lane irlower, IRFEED17-1):
                                // the record, not the bitset alone, so a fold a
                                // growing pass masked (and a full unroll) is
                                // named on the method's own line.
                                ir_evidence::describe_record(evidence),
                                // t1 w11 JACC (ACCEPT21-5 first step).
                                ir_evidence::guard_frame_census(&graph, &schedule).describe(),
                            );
                        }
                        if ir_acceptance_refusal_writes_memo(
                            osr_handler_arms_handed,
                            req.osr_optimizing_route,
                        ) {
                            // ACCEPT21-3 (opt-in): the OSR route writes its own key.
                            // Otherwise (r21 accept, re-landed t1 w8, default ON)
                            // it writes nothing: `osr_route_own_refusal_memo_enabled`.
                            // t1 w8 ACCEPT (`CRATONVM_JIT_IR_REFUSAL_STAMP_AT_ARM`,
                            // default ON): stamped with the generation the build judged.
                            let memo_key =
                                ir_refusal_memo_route_key(ir_refusal_key, req.osr_optimizing_route);
                            // INDYLATE21-1 (opt-in): the door's ticket wins on
                            // the shared key (the method stamps live there).
                            match evidence.and_then(|r| r.inputs_generation_at_arm) {
                                _ if ir_refusal_ticket.at_door() && memo_key == ir_refusal_key => {
                                    req.verdicts()
                                        .ir_refusals()
                                        .note_refused_for_ticket(ir_refusal_ticket)
                                }
                                Some(g) if ir_evidence::refusal_stamp_at_arm_enabled() => req
                                    .verdicts()
                                    .ir_refusals()
                                    .note_refused_for_inputs_at(memo_key, g),
                                _ => req
                                    .verdicts()
                                    .ir_refusals()
                                    .note_refused_for_inputs(memo_key),
                            }
                        }
                        // JIT r22 wave 2, lane inline (INLINE22-1): the
                        // fall-through's cause, for the acceptance re-offer.
                        note_ir_acceptance_refused();
                        drop(cm);
                        None
                    }
                    other => other,
                };
                if let Some(mut compiled) = lowered {
                    // cov-06 residual: a surviving `Op::New` or
                    // `Op::NewArray` allocation call can fail (OOM, or a
                    // negative length for an array) and stash a pending
                    // exception through the SAME `JIT_PENDING_EXCEPTION`
                    // channel `getstatic`/an invoke uses — see the
                    // `ir_static_init_classes` `has_dispatch` arm below
                    // ("the `jit-clinit-gap-has-dispatch` defect") for the
                    // identical shape. Without `has_dispatch`, the VM's
                    // fast call entry never drains that pending exception,
                    // so the allocation helper's `i64::MIN` failure
                    // sentinel is NOT recognised as a deopt/exception —
                    // `execute_jit_call`'s `b'[' | b'L'` return arm only
                    // checks `result == 0`, so `i64::MIN` (`!= 0`) is
                    // pushed as `Value::Object(Some(ObjectRef::from_raw(
                    // 0x8000000000000000)))`, an address no live heap
                    // region contains. Reading it back later degrades
                    // through the NaN-box plausibility gate to
                    // `Value::Long` (see `CompactValue::to_value`), and a
                    // subsequent array/field access on it then reads as
                    // silently null instead of throwing the real
                    // OutOfMemoryError/NegativeArraySizeException.
                    //
                    // Reached in practice: a hot method that `newarray`s
                    // in a tight loop under GC/heap pressure eventually
                    // hits this path (`vm/tests/jit_cov06_array_allocation.rs`
                    // reproduced it deterministically once the surrounding
                    // program's memory footprint was large enough to
                    // trigger it before `--Xmx 32m` was exhausted).
                    // `Op::New` has the identical gap for plain object
                    // allocation — same fix, same reasoning.
                    if graph
                        .nodes
                        .iter()
                        .any(|n| matches!(n.op, ir::Op::New { .. } | ir::Op::NewArray { .. }))
                    {
                        compiled.has_dispatch = true;
                    }
                    // `Op::MonitorEnter` and `Op::MonitorExit` require `set_jit_thread` via
                    // the dispatch-aware entry so `jit_monitor_enter`/`jit_monitor_exit` lock.
                    if graph
                        .nodes
                        .iter()
                        .any(|n| matches!(n.op, ir::Op::MonitorEnter | ir::Op::MonitorExit))
                    {
                        compiled.has_dispatch = true;
                    }
                    // `Op::Throw` requires the dispatch-aware slow entry so `sig.exception`
                    // is drained (`vm/tests/jit_ir_athrow_dispatch.rs`).
                    if graph.nodes.iter().any(|n| matches!(n.op, ir::Op::Throw)) {
                        compiled.has_dispatch = true;
                    }
                    // `Op::ArrayStore(Ref)` and `Op::IndyBridge` require `jit_thread_mut()` TLS.
                    if graph
                        .nodes
                        .iter()
                        .any(|n| matches!(n.op, ir::Op::ArrayStore(ir::MemKind::Ref)))
                    {
                        compiled.has_dispatch = true;
                    }
                    if graph
                        .nodes
                        .iter()
                        .any(|n| matches!(n.op, ir::Op::IndyBridge { .. }))
                    {
                        compiled.has_dispatch = true;
                    }
                    // Attach baked `JitInvokeInfo`/string boxes and mark dispatch-aware entry.
                    if !ir_call_infos.is_empty() {
                        compiled._jit_invoke_infos = ir_call_infos;
                        compiled._jit_strings = ir_call_strings;
                        compiled._jit_vh_judged_sites = ir_vh_judged_sites;
                        compiled.has_dispatch = true;
                    }
                    compiled._jit_class_init_cells = ir_class_init_cells;
                    // cov-01's `ldc <String>` keep-alive stood here and is
                    // GONE: the site is CP-indexed now, so the body bakes a
                    // `(class id, cp index)` pair rather than the literal's
                    // address, and there are no bytes for the artifact to
                    // outlive.
                    // cov-05: `instanceof`/`checkcast` target class names
                    // deliberately do NOT appear here — they are interned
                    // for the life of the process instead, so the
                    // `(ptr, len)` pair the type-check helpers memoize on
                    // cannot be recycled for another class. See
                    // `intern_typecheck_class_name`.
                    // cov-05: see `ir_needs_dispatch_for_checkcast`'s
                    // declaration above for why.
                    if ir_needs_dispatch_for_checkcast {
                        compiled.has_dispatch = true;
                    }
                    // Record static-field declaring classes for pre-entry `<clinit>` (`append`).
                    if !ir_static_init_classes.is_empty() {
                        compiled
                            .static_init_classes
                            .append(&mut ir_static_init_classes);
                        compiled.static_init_classes.sort_unstable();
                        compiled.static_init_classes.dedup();
                        compiled.has_dispatch = true;
                    }
                    // Keep slots alive for the life of the artifact (`extend`, never overwrite).
                    if !ir_mic_boxes.is_empty() {
                        compiled._jit_mic_slots.extend(ir_mic_boxes);
                        compiled._jit_pic_slots.extend(ir_pic_boxes);
                    }
                    compiled
                        ._jit_typecheck_seen_cells
                        .extend(std::mem::take(&mut ir_typecheck_seen_cells));
                    // Artifact owns baked cycle-edge cells before `JitCache::put` registers them.
                    if !ir_owned_cycle_cells.is_empty() {
                        compiled
                            ._jit_cycle_cells
                            .extend(std::mem::take(&mut ir_owned_cycle_cells));
                    }
                    if !ir_direct_callee_entries.is_empty() {
                        // Append so lowerer-recorded callees stay pinned in `prepare_for_publication`.
                        compiled
                            ._direct_callee_entries
                            .extend(ir_direct_callee_entries.iter().map(|(e, _)| *e));
                        compiled
                            ._direct_callee_expected
                            .append(&mut ir_direct_callee_entries);
                        compiled._direct_callee_entries.sort_unstable();
                        compiled._direct_callee_entries.dedup();
                    }
                    // Keep compile-time callee pins alive until `prepare_for_publication`.
                    compiled
                        ._direct_callee_roots
                        .extend(std::mem::take(&mut ir_baked_callee_pins));
                    for dep in &graph.frozen_cha_deps {
                        if !compiled.inlined_methods.iter().any(|(cn, _, _)| cn == dep) {
                            compiled.inlined_methods.push((
                                dep.clone(),
                                "<cha-field>".into(),
                                "()V".into(),
                            ));
                        }
                    }
                    if let (Some(_), Some(state)) = (req.frozen_facts, req.frozen_state) {
                        let mkey = (
                            Arc::clone(&cached.class_name),
                            Arc::clone(&cached.method_name),
                            Arc::clone(&cached.method_descriptor),
                        );
                        state.note_misses(mkey, &graph.frozen_misses);
                        state.record_compile(
                            &graph.frozen_folded_classes,
                            &graph.frozen_failed_classes,
                        );
                        if graph.frozen_identity_guards != 0 {
                            state.identity_guards.fetch_add(
                                graph.frozen_identity_guards,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                    }
                    // Caller fills frame state identity via `CompiledMethod::stamp_deopt_method_key`.
                    super::splice_args::finalize_ir_compiled_telemetry(
                        &mut compiled,
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        ir_spliced_bodies_pure,
                        &ir_guarded_splice_bcis,
                        &ir_guarded_splice_classes,
                        &ir_splice_scope_class_ids,
                        &ir_splice_scope_sources,
                        chain_census,
                        ir_emit_long,
                        code,
                        code_len,
                        metrics,
                    );
                    return Some(compiled);
                }
                super::splice_args::report_ir_tier_decline(
                    acceptance_discarded,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
            }
        } // end else (IR-lowering path)
    }
    None
}
