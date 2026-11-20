#[allow(unused_imports)]
use super::super::*;

pub(crate) fn build_single_pass_tables(
    req: &CompileRequest<'_>,
    backend_attempted: &mut bool,
    self_call_identity_stable: bool,
    local_handlers_disarmed: bool,
    metrics: &metrics::CompileRecorder,
    code: &[u8],
    code_len: usize,
    scan: x64::JitScanResult,
    prologue_param_slots: usize,
    precise_exception_frames: bool,
    resolved_string_layout: Option<StringFieldLayout>,
) -> Option<(SinglePassTables, SinglePassParts)> {
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
        inline_resolver,
        string_layout_resolver,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        cp_invokedynamic_descriptor_resolver,
        class_id_name_resolver,
        receiver_inline_resolver,
        unique_concrete_resolver,
        jdk_only,
        direct_bind_admission,
        despec,
        cp_invoke_declaring_class_resolver,
        direct_helpers,
        ..
    } = *req;
    // The compiling VM's verdict registry (cycle sets), read by the bind ladders.
    let verdicts = req.verdicts();

    // Diagnostic for the "hot method never compiles" shape: every constant-pool
    // resolver below can come back `None`, and each such miss silently bails the
    // whole compile with `backend_attempted = false` (a *transient* bail, retried
    // until `MAX_TIER_FAIL_RETRIES`, after which the method interprets forever).
    // `CRATONVM_DBG_JITC=1` previously reported only that the bail happened;
    // naming the resolver is what turns that into an actionable report.
    macro_rules! jitc_bail {
        ($site:expr) => {
            return {
                crate::note_jit_bail_site($site);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] resolver-bail site={} {}.{}{}",
                        $site, cached.class_name, cached.method_name, cached.method_descriptor
                    );
                }
                None
            }
        };
    }

    /// A bail the compiler will never take back: the offending property is
    /// fixed by the class file, so retrying cannot change the answer. Sets
    /// `backend_attempted`, which is what routes the method onto the permanent
    /// bail-list — the flag's real meaning, despite its name.
    ///
    /// Named exactly like [`jitc_bail`] so the two read the same in a trace;
    /// the `permanent-bail` prefix is what tells them apart.
    macro_rules! jitc_permanent_bail {
        ($site:expr) => {
            return {
                *backend_attempted = true;
                crate::note_jit_bail_site($site);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] permanent-bail site={} {}.{}{}",
                        $site, cached.class_name, cached.method_name, cached.method_descriptor
                    );
                }
                None
            }
        };
    }

    // Control reaches here either because the optimizing pipeline was never
    // admitted, or because it was entered and declined (an unbuildable graph,
    // a verifier rejection, a lowerer bail). The recorder already knows which
    // — `enter_single_pass` flags the second case
    // as a fall-through, which is what makes "the C2 tier produced no bodies"
    // separable from "the C2 tier was never asked".
    metrics.enter_single_pass();
    note_single_pass_entered();

    // Resolve multianewarray entries.
    //
    // What travels to the backend is the site's `(holder_class_id, cp_idx)`,
    // packed — see `pack_multianewarray_site`. The helper resolves the array
    // class from it at run time through the same
    // `interpreter::multianewarray_alloc` the interpreter uses, so both tiers
    // stamp the same per-level component classes. The old shape was a leaf
    // element-type code, which named no class at all: the helper allocated
    // every level with `ClassId(0)` and a compiled `new String[a][b]` read back
    // as `[Ljava.lang.Object;`, so any `checkcast` to the declared array type
    // threw. The resolver call stays, because a CP index that does not name a
    // class is still a whole-compile refusal.
    let mut mna_info = Vec::new();
    if !scan.multianewarray_ops.is_empty() {
        let Some(resolver) = cp_class_name_resolver else {
            jitc_bail!("cp_class_name_resolver(multianewarray)")
        };
        for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
            let Some(_class_name) = resolver(cp_idx) else {
                jitc_bail!("multianewarray_class")
            };
            mna_info.push((
                pc,
                pack_multianewarray_site(cached.declaring_class_id.as_u32(), cp_idx),
            ));
        }
    }

    let mut needs_heap = scan.needs_heap;
    let mut field_info = Vec::new();
    // Compact reference-field layout: per field op, the packed byte offset +
    // ref-ness (from the resolver, which has the declaring class). Stays empty
    // when the flag is off → inline emitters use the legacy path.
    let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
    // The volatile `getfield`/`putfield` pcs, for the backend's post-store
    // StoreLoad fence. Travels on the `BackendRequest` (below), not as a
    // widened `field_info` row, so the twenty-odd consumers of that tuple are
    // untouched. See `CompileRequest::cp_field_volatile_resolver`.
    let mut volatile_field_pcs: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let compact_fields = cratonvm_types::compact_ref_fields_enabled();
    // Interpreter round i4 wave 92, lane L2 (`CRATONVM_JIT_DEFERRED_FIELD_SITES`,
    // default OFF; `CompileRequest::cp_field_deferred_resolver`): a field site
    // the door could not resolve compiles as a deferred site -- resolved,
    // linked, initialised and accessed at its first execution by
    // `DirectHelperTable::field_site_cp` -- instead of refusing the method.
    // `(pc, (holder class id, cp index, descriptor tag))`, the holder being
    // this method's class (the scan covers its own bytecode only).
    let mut deferred_field_sites: Vec<(usize, (u32, u16, u8))> = Vec::new();
    let deferred_field_tag = |cp_idx: u16| -> Option<u8> {
        if direct_helpers.field_site_cp == 0 {
            return None;
        }
        req.cp_field_deferred_resolver
            .and_then(|tag_of| tag_of(cp_idx))
            .filter(|&tag| {
                matches!(
                    tag,
                    b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'L' | b'['
                )
            })
    };
    if !scan.field_ops.is_empty() {
        let Some(resolver) = cp_field_resolver else {
            jitc_bail!("cp_field_resolver")
        };
        for &(pc, cp_idx) in &scan.field_ops {
            let Some((field_index, type_tag, compact_slot)) = resolver(cp_idx) else {
                if let Some(tag) = deferred_field_tag(cp_idx) {
                    let site = (cached.declaring_class_id.as_u32(), cp_idx, tag);
                    deferred_field_sites.push((pc, site));
                    needs_heap = true;
                    continue;
                }
                jitc_bail!("field_resolve")
            };
            // Type tag `0` is the resolver's marker for a field read that must
            // stay on the checked `jit_getfield` helper (SBR4 satb:
            // `Reference.referent`, whose load carries an SATB keep-alive no
            // inline arm has). No field row is recorded for such a `getfield`,
            // which is the existing "no statically-resolved metadata" arm:
            // `jit_getfield(vm, obj, 0)` (slot 0 is the referent's slot).
            if type_tag == 0 && code[pc] == 0xb4 {
                needs_heap = true;
                continue;
            }
            field_info.push((pc, field_index, type_tag));
            if cp_field_volatile_resolver.is_some_and(|is_volatile| is_volatile(cp_idx)) {
                volatile_field_pcs.insert(pc);
            }
            // Only a genuinely-resolved compact slot may enter the inline
            // emitter's compact-offset map. `None` (no registered layout, or
            // index outside it) previously arrived as a fabricated
            // `(0, false)` tuple and steered the compact-offset inline
            // getfield/putfield arms at a garbage offset — the WildFly Host
            // Controller SIGSEGV. Such pcs now take the guarded uniform arm,
            // which keys on the per-object GC_FLAG_COMPACT bit and routes
            // compact receivers to the layout-aware helper.
            if compact_fields {
                if let Some((c_off, c_ref)) = compact_slot {
                    compact_field_info.push((pc, c_off, c_ref));
                } else if false {
                    // ENGAGEMENT CENSUS — see the OSR twin in
                    // `vm/src/runtime/interpreter/jit_bridge.rs`. A `None` here
                    // costs a `jit_getfield` call on EVERY access to a compact
                    // receiver, not merely a missed inline.
                    eprintln!(
                        "[compact-inline] MISS entry pc={pc} field_index={field_index} -> guarded-uniform arm (helper on every compact receiver)"
                    );
                }
            }
            if code[pc] == 0xb5 && (type_tag == b'L' || type_tag == b'[') {
                needs_heap = true;
            }
        }
    }

    let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
    let mut owned_strings: Vec<Box<str>> = Vec::new();
    if !scan.typecheck_ops.is_empty() {
        let Some(resolver) = cp_class_name_resolver.as_ref() else {
            jitc_bail!("cp_class_name_resolver(typecheck)")
        };
        for &(pc, cp_idx) in &scan.typecheck_ops {
            let Some(class_name) = resolver(cp_idx) else {
                jitc_bail!("typecheck_class")
            };
            // Interned process-wide, NOT owned by this compilation — the
            // type-check helpers memoize on `(ptr, len)` from thread-locals
            // that outlive us. See `intern_typecheck_target`.
            //
            // When the `new` resolver can name the exact `ClassId` this site's
            // `CONSTANT_Class` entry resolves to through the compiling class's
            // loader, the site is interned under that identity so the runtime
            // helper compares ids rather than re-resolving a bare name against
            // a `(ClassLoaderId, name)`-keyed dictionary.
            //
            // Round 9 wave 8 (`arraylist8`): the VM's verdict on whether that
            // resolved target is a final class, which lets the inline
            // `instanceof` answer a definite miss for it (`op_object.rs`,
            // `final_miss_target`). No resolver, no id, or `false`: the
            // pre-wave-8 pointer and behaviour. A `Deferred` answer interns the
            // site under its holder, so the helper resolves it through the
            // holder's loader rather than by bare name.
            let (ptr, len) = intern_typecheck_for_new_site(
                &class_name,
                cp_new_resolver.and_then(|r| r(cp_idx)),
                req.class_is_final_resolver,
            );
            typecheck_info.push((pc, ptr, len));
        }
    }

    let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
    if !scan.static_field_ops.is_empty() {
        let Some(resolver) = cp_static_field_resolver else {
            jitc_bail!("cp_static_field_resolver")
        };
        for &(pc, cp_idx) in &scan.static_field_ops {
            let Some((class_id_raw, field_index, type_tag, is_volatile)) = resolver(cp_idx) else {
                // Wave 92, lane L2: see `deferred_field_sites` above.
                if let Some(tag) = deferred_field_tag(cp_idx) {
                    let site = (cached.declaring_class_id.as_u32(), cp_idx, tag);
                    deferred_field_sites.push((pc, site));
                    needs_heap = true;
                    continue;
                }
                jitc_bail!("static_field_resolve")
            };
            static_field_info.push((pc, class_id_raw, field_index, type_tag, is_volatile));
        }
    }

    // CRIT-2 — new_info tuple shape:
    //   (pc, class_id_raw, num_fields,
    //    has_nonzero_tag_primitive_init, // class has long/float/double fields
    //                                    // whose typed-zero Value tag is nonzero
    //    has_finalizer)         // class overrides `finalize()` and must
    //                           // be registered with the finalizer queue
    //
    // When both flags are `false` the inline TLAB fast path skips the
    // `jit_post_tlab_init` helper entirely (class_id + num_slots are
    // written inline; identity hash stays lazily-minted zero). The
    // resolver computes the real flags from class metadata; a resolver
    // that cannot determine them must return `(true, true)` so the
    // helper call stays in place.
    //
    // A `JitNewSite::Deferred` site (target class not loaded yet) goes into
    // the parallel `new_deferred_info` / `anewarray_deferred_info` lists
    // instead: those compile to the CP-indexed helper, which resolves the
    // class on first execution. Before that existed, a `Deferred` site was
    // indistinguishable from a malformed one and bailed the WHOLE compile —
    // permanently, after `MAX_TIER_FAIL_RETRIES` — which is what left every
    // hot method carrying a cold `throw new SomeException(...)` in the
    // interpreter forever. `None` (malformed CP entry / missing holder) is
    // still a bail: no runtime resolution can rescue it.
    //
    // The `new_object_cp` / `anewarray_object_cp` helpers are optional in the
    // ABI (a hand-built test table leaves them 0), so an unwired helper falls
    // back to the historical whole-compile bail rather than emitting a CALL
    // to address 0.
    let mut new_info: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
    let mut anewarray_info: Vec<(usize, u32)> = Vec::new();
    let mut new_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
    let mut anewarray_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
    if !scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty() {
        let Some(resolver) = cp_new_resolver else {
            jitc_bail!("cp_new_resolver")
        };
        for &(pc, cp_idx) in &scan.new_ops {
            match resolver(cp_idx) {
                Some(JitNewSite::Resolved {
                    class_id,
                    num_fields,
                    needs_post_alloc_init,
                    has_finalizer,
                }) => {
                    new_info.push((
                        pc,
                        class_id,
                        num_fields,
                        needs_post_alloc_init,
                        has_finalizer,
                    ));
                }
                Some(JitNewSite::Deferred {
                    holder_class_id,
                    cp_idx,
                }) => {
                    if helpers.new_object_cp == 0 {
                        jitc_bail!("new_resolve_deferred_unwired")
                    }
                    new_deferred_info.push((pc, holder_class_id, cp_idx));
                }
                None => jitc_bail!("new_resolve"),
            }
        }
        for &(pc, cp_idx) in &scan.anewarray_ops {
            match resolver(cp_idx) {
                Some(JitNewSite::Resolved { class_id, .. }) => {
                    anewarray_info.push((pc, class_id));
                }
                Some(JitNewSite::Deferred {
                    holder_class_id,
                    cp_idx,
                }) => {
                    if helpers.anewarray_object_cp == 0 {
                        jitc_bail!("anewarray_resolve_deferred_unwired")
                    }
                    anewarray_deferred_info.push((pc, holder_class_id, cp_idx));
                }
                None => jitc_bail!("anewarray_resolve"),
            }
        }
    }

    // The `ldc` / `ldc_w` / `ldc2_w` constant tables (`w7_ldc_tables`).
    let super::w7_ldc_tables::LdcTables {
        ldc_info,
        ldc_string_info,
        mut ldc_class_info,
        mut ldc_slots,
        ldc_fp_pcs,
        ldc2w_info,
    } = match super::w7_ldc_tables::resolve_ldc_tables(
        &scan.ldc_ops,
        &scan.ldc2w_ops,
        cp_ldc_resolver,
        cp_ldc2w_resolver,
        helpers,
    ) {
        Ok(tables) => tables,
        Err(super::w7_ldc_tables::LdcBail::Transient(site)) => jitc_bail!(site),
        Err(super::w7_ldc_tables::LdcBail::Permanent(site)) => jitc_permanent_bail!(site),
    };

    let mut invoke_info: Vec<(usize, *const JitInvokeInfo)> = Vec::new();
    let mut owned_invoke_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
    // The callee info for this method's raw self-recursive `CALL` sites, which
    // are the one shape the scan loop deliberately keeps OUT of `invoke_info`.
    // Staged here and handed to the backend below; `0` = this method has none.
    // See `stage_self_call_invoke_info`.
    let mut self_call_invoke_info: usize = 0;
    // JIT round 19 wave 5 (lane sync5, SYNCDOOR4-2): the dispatch infos of a
    // synchronized instance method's self-call sites planned as a direct
    // self-call (`sync_self_call_goes_direct`). Only the SELF-LOCKING run may
    // CALL its own entry; every other run swaps these in for the marker rows
    // (`SinglePassTables::unbind_sync_self_calls`).
    let mut sync_self_call_fallback: Vec<(usize, *const JitInvokeInfo)> = Vec::new();
    let sync_self_call_candidate = self_lock_self_call_enabled()
        && cached.is_synchronized
        && !cached.is_static
        && self_lock_admits_method(cached, code, code_len, req.self_lock_mirror_slot)
        && !self_lock_refusal_memoized(req.verdicts(), cached);
    let mut direct_calls: Vec<(usize, JitDirectCall)> = Vec::new();
    let mut direct_callee_entries: Vec<(usize, u64)> = Vec::new();
    let mut mic_slots: Vec<(usize, *const JitMICSlot)> = Vec::new();
    let mut owned_mic_slots: Vec<Box<JitMICSlot>> = Vec::new();
    // HIGH-7 — Eager PIC allocation strategy.
    //
    // At first JIT compile we optimistically allocate one
    // `Box<JitPICSlot>` for every invokevirtual / invokeinterface
    // bci in the method, regardless of MIC miss history. Rationale:
    //   * The cost is small (≈64 B per call site; typical methods
    //     have <5 virtual call sites).
    //   * The 4-way inline cascade is silent on empty slots: each
    //     entry's `EMPTY_WAY_CLASS_ID` fails the CMP and falls
    //     through to the helper, so cold sites pay zero extra cycles.
    //   * Once the runtime helper populates a slot, the inline
    //     cascade starts hitting — no recompile required, which
    //     sidesteps the adaptive-recompile machinery entirely.
    //
    // This is the "simpler approach" from the HIGH-7 design doc, and the
    // only one: there is no adaptive MIC→PIC promotion path.
    let mut pic_slots: Vec<(usize, *const JitPICSlot)> = Vec::new();
    let mut owned_pic_slots: Vec<Box<JitPICSlot>> = Vec::new();
    // Round 9 wave 7 (`mutrec7`): see `JitCycleEdgeCell`.
    let mut owned_cycle_cells: Vec<Arc<JitCycleEdgeCell>> = Vec::new();
    // Round 15 wave 6 (lane vh8): see `VarHandleJudgedSite`.
    let mut owned_vh_judged_sites: Vec<Box<VarHandleJudgedSite>> = Vec::new();
    let mut inline_sites: HashMap<usize, InlineSite> = HashMap::new();
    // PGO-02: the guarded variants of every admitted speculative virtual/
    // interface inline plan — `(receiver class id, the body THAT CLASS
    // dispatches to)`, in guard order, keyed by the same pc as inline_sites.
    // One entry for a Monomorphic verdict, two for a Bimorphic one.
    //
    // Element [0] is also the `inline_sites` entry for the pc, so the
    // backend's buffer/frame reservations count it exactly once; element [1]
    // lives only here and the backend adds it to those reservations
    // explicitly (see `x64::driver`'s `extra_guard_bodies`).
    //
    // Kept as a separate map rather than a new InlineSite field because
    // vm/src/runtime/interpreter/invoke.rs constructs InlineSite with an
    // exhaustive struct literal (adding a field there would break the vm
    // crate). NOT threaded through the loop-unroll pc replication below
    // (unlike inline_sites itself) - a loop-unrolled copy of a guarded
    // virtual call site simply won't find an entry here and falls back to
    // normal dispatch, which is always correct, just not optimized.
    let mut inline_guard_variants: HashMap<usize, Vec<(u32, InlineSite)>> = HashMap::new();
    // jit-inlining-and-ir-calls: HotSpot-shaped inlining budget. `hot_loops` is
    // derived once from the profile plus the bytecode; a caller that executes
    // any hot loop gets the larger whole-method budget, and each site inside
    // one gets the `FreqInlineSize` (325 / expansion 512) rather than
    // `MaxInlineSize` (35 / expansion 64) allowance. With no profile the list
    // is empty, every site is cold, and both the per-site tier and the
    // whole-method budget collapse to the pre-2026-07-26 values — an
    // unprofiled compile inlines identically.
    // Round 14 wave 4 (lane calls2, C14W3-1): no longer true by default --
    // with no loop evidence in the profile, every loop of the method is hot
    // (`method_entry_hot_loop_ranges`); `CRATONVM_JIT_INLINE_LOOP_SITES_HOT=0`
    // restores the sentence above.
    let inline_hot_loops = method_entry_hot_loop_ranges(code, code_len, profile);
    let mut inline_budget_remaining: usize = if inline_hot_loops.is_empty() {
        MAX_INLINE_BUDGET
    } else {
        MAX_INLINE_BUDGET_HOT
    };
    let mut inlined_methods: Vec<(String, String, String)> = Vec::new();
    // C2-review P1 — inlining decision totals for the metrics report. Attached
    // to the artifact at the end of this function.
    let mut inline_tally = InlineDecisionTally::default();
    // `resolved_string_layout` is resolved once near the top of this function
    // (the optimizing-tier gate consumes it too); see its declaration there.
    if !scan.invoke_ops.is_empty() {
        let Some(resolver) = cp_invoke_resolver else {
            jitc_bail!("cp_invoke_resolver")
        };
        for &(pc, cp_idx, opcode) in &scan.invoke_ops {
            let Some((class_name, method_name, descriptor)) = resolver(cp_idx) else {
                jitc_bail!("invoke_resolve")
            };
            let mut invoke_kind = match opcode {
                0xb6 => 0u8,
                0xb7 => 1,
                0xb9 => 2,
                _ => 3,
            };
            // JVMS 5.4.6: an `invokevirtual` that RESOLVES TO A PRIVATE METHOD
            // selects that method, with no override lookup at all.
            //
            // javac has emitted `invokevirtual` for a call to a private instance
            // method since Java 11 (JEP 181 nestmates) — before that it emitted
            // `invokespecial`. The opcode changed; the semantics did not. Every
            // dispatcher downstream of `invoke_kind == 0` resolves by walking up
            // from the RECEIVER's class, so on such a site it finds the
            // most-derived same-named private method and calls THAT.
            //
            // The shape is ordinary and common: a class whose constructor calls
            // its own `private void init()`, subclassed by a class that does the
            // same. `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
            // `HttpClientOptions` are three such levels in one chain, and the
            // consequence is that constructing a `WebClientOptions` runs
            // `HttpClientOptions.init()` three times and `TCPSSLOptions.init()`
            // never — leaving `transportOptions` null, which surfaces much later
            // as a null `other` in `TcpConfig`'s copy constructor
            // (`known-issues/jit/bg-compile-off-nulls-a-reference-argument-…`).
            //
            // A private target is not a dispatch site, so it takes the same
            // route `invokespecial` does: bind exactly, at the resolved owner.
            //
            // ...UNLESS an inline call-site intrinsic would take this site.
            // Statically binding it is a correct claim about DISPATCH and a
            // disastrous one about CODEGEN: the instance-intrinsic gate below
            // is `invoke_kind == 0 || invoke_kind == 2`, and a kind-1 site
            // reaches the inline/direct-bind ladder first and leaves the loop
            // through its `continue`. `java/lang/String` is `final`, so this
            // rule answers for EVERY `String.charAt`/`length`/`isEmpty`/
            // `hashCode` site in the tree — and every one of them was bound to
            // a real call instead of the inline decode, silently: not declined,
            // not counted, and invisible to all three `string-intrinsic`
            // diagnostics, which sit past the point the site left.
            //
            // Measured on `probes/CharAtDoorProbe.java`, one class, one run,
            // five byte-identical bodies — the arm called straight from `main`
            // 349.64 ns/char against 3.2-4.3 for the four the OSR door
            // compiles, and 6.85 on the same binary with
            // `CRATONVM_JIT_FINAL_DEVIRT=0`.
            //
            // So the site is handed back. `try_resolve_intrinsic` is the
            // layout-independent matcher and `try_resolve_string_intrinsic` the
            // layout-aware one; between them they are exactly the set the gate
            // below would accept, asked the same way it asks. A PRIVATE target
            // is not affected: no intrinsic matches a private method, so the
            // JVMS 5.4.6 correctness rule above keeps every site it had.
            let yields_to_intrinsic = invoke_kind == 0
                && (site_yields_to_call_site_intrinsic(
                    &class_name,
                    &method_name,
                    &descriptor,
                    resolved_string_layout,
                ) || site_yields_to_atomic_intrinsic(
                    cp_idx,
                    &class_name,
                    &method_name,
                    &descriptor,
                    cp_invoke_class_id_resolver,
                    cp_invoke_declaring_class_resolver,
                ));
            // JVMS §6.5 `invokeinterface`, selection step 1: an `0xb9` site
            // whose resolved method is PRIVATE (a private interface method
            // called from a default method of the same interface) selects that
            // method, never the receiver's same-named public one. The VM's
            // owner resolver answers `Some(declaring interface)` for exactly
            // that case and `None` for every other `0xb9` site, so the site is
            // pinned as a direct bind here just as a private `0xb6` is —
            // before the MIC/PIC is allocated (`invoke_kind_uses_inline_cache`)
            // and before the guarded inline planner sees a speculative site.
            // No intrinsic matches a private method, so `yields_to_intrinsic`
            // has nothing to protect on this arm.
            // The class id of the owner substituted for the constant-pool
            // class below, baked into every `JitInvokeInfo` this site gets
            // (`JitInvokeInfo::owner_class_id`); `0` while `class_name` is the
            // constant pool's own.
            let mut owner_class_id: u32 = 0;
            let class_name = if (invoke_kind == 0 && !yields_to_intrinsic)
                || (invoke_kind == 2 && opcode == 0xb9)
            {
                match cp_invokespecial_owner_resolver.and_then(|r| r(cp_idx, opcode)) {
                    Some((owner, owner_id)) => {
                        invoke_kind = 1;
                        owner_class_id = owner_id;
                        if opcode == 0xb6 {
                            PRIVATE_INVOKEVIRTUAL_PINNED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        owner
                    }
                    None => class_name,
                }
            } else {
                if yields_to_intrinsic {
                    DEVIRT_YIELDED_TO_INTRINSIC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if crate::x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] devirt YIELDS to intrinsic {class_name}.{method_name}{descriptor} @pc={pc}"
                        );
                    }
                }
                class_name
            };
            let invoke_kind = invoke_kind;
            if dbg_intrinsic_enabled() {
                eprintln!(
                    "[cratonvm-intrinsic] single-pass site {class_name}.{method_name}{descriptor} @pc={pc} kind={invoke_kind}"
                );
            }
            // JVMS §6.5 super-call redirect (see `try_compile`'s doc comment
            // on `cp_invokespecial_owner_resolver`): for `invokespecial`
            // sites only, substitute the JVMS-correct selection-start class
            // when it differs from the plain CP-referenced class. Must run
            // before EVERY downstream use of `class_name` below (the
            // recursive-call check, inlining, direct-callee compile, and the
            // `JitInvokeInfo` baked into the compiled code), so a wrong
            // target is never baked into any of them.
            let class_name = match (invoke_kind == 1 && opcode == 0xb7)
                .then(|| cp_invokespecial_owner_resolver.and_then(|r| r(cp_idx, opcode)))
                .flatten()
            {
                Some((owner, owner_id)) => {
                    owner_class_id = owner_id;
                    owner
                }
                None => class_name,
            };
            let owner_class_id = owner_class_id;
            let num_params = count_param_slots(&descriptor);
            let has_receiver = invoke_kind != 3;
            let num_jit_args = num_params + if has_receiver { 1 } else { 0 };
            let ret_type = return_type(&descriptor);

            let is_same_method_recursive_call = class_name == &*cached.class_name
                && method_name == &*cached.method_name
                && descriptor == &*cached.method_descriptor;
            // The class the recursion-cycle checks name this site by: the
            // resolved method's DECLARING class for an inherited-name call
            // (`Sub.f` -> `A.f`), the site's class otherwise. See
            // `cycle_site_declaring_class`.
            let cycle_declaring = cycle_site_declaring_class(
                verdicts,
                cp_invoke_declaring_class_resolver,
                cp_idx,
                &class_name,
                &method_name,
                &descriptor,
                || resolver(cp_idx).is_some_and(|(cp_class, _, _)| cp_class == class_name),
            );
            let cycle_class: &str = cycle_declaring.as_deref().unwrap_or(&class_name);
            // The loaded class `cycle_class` names, for the recursion-cycle
            // sets; asked only when a cycle member has these names. A declaring
            // class is named by the VM's id for it (wave 17), or by names when
            // the request has no id resolver.
            let site_class_id = || {
                if cycle_declaring.is_some() {
                    return cycle_site_declaring_class_id(
                        req.cp_invoke_declaring_class_id_resolver,
                        cp_idx,
                    );
                }
                cycle_site_class_id(resolver, cp_invoke_class_id_resolver, cp_idx, &class_name)
            };
            // `invoke_kind` is final here (after the private/final pin), so 0|2
            // is exactly "this site will be an inline cache".
            let closes_active_compile_cycle = verdicts.note_recursive_compile_cycle(
                cycle_class,
                &method_name,
                &descriptor,
                invoke_kind_uses_inline_cache(invoke_kind),
                site_class_id,
            );
            let recursive_cycle_target = closes_active_compile_cycle
                || verdicts.direct_call_requires_dispatch_of(
                    cycle_class,
                    &method_name,
                    &descriptor,
                    site_class_id,
                );
            let is_recursive_call = is_same_method_recursive_call || recursive_cycle_target;
            let use_raw_tail_self_call = invoke_kind == 3
                && is_same_method_recursive_call
                && invokestatic_self_call_uses_tail_jump(code, code_len, pc);
            // JIT round 20 wave 11 (lane pLate, W15L1-P1): set when the
            // direct bind below asked `callee_compiler` and got nothing; the
            // generic tail then plans a late-bind cell. See
            // [`late_static_cell_site`].
            let mut sp_late_static_declined = false;

            // Round 9 wave 7 (`mutrec7`): the statically bound CLOSING edge of
            // a compile-time recursion cycle (the target is this compile's own
            // compile-stack ancestor, so it has no entry yet). Instead of
            // leaving it on `jit_invoke_dispatch` for the life of this body,
            // plan a guarded call through a `JitCycleEdgeCell` that
            // `JitCache::put` fills once the ancestor publishes. The site still
            // gets its `JitInvokeInfo` below (the loop's generic tail): the
            // emitted code dispatches through it while the cell is empty, and
            // the deopt service reads it. `invokestatic` only; default OFF
            // (`CRATONVM_JIT_CYCLE_EDGE_CELL`).
            //
            // The id is the call site's OWN resolution of the class, so a
            // same-named class of another loader can never fill the cell. Not
            // for an inherited-name edge (`cycle_declaring`): the ancestor
            // publishes under its declaring class, so the cell would never fill.
            if closes_active_compile_cycle
                && cycle_declaring.is_none()
                && !is_same_method_recursive_call
                && invoke_kind == 3
                && opcode == 0xb8
                && jit_cycle_edge_cell_enabled()
                && jit_cycle_direct_bind_enabled()
                && direct_jit_callee_calls_enabled()
                && !verdicts.direct_call_requires_dispatch_of(
                    cycle_class,
                    &method_name,
                    &descriptor,
                    site_class_id,
                )
                && helpers.self_call_stack_guard != 0
                && helpers.invoke_dispatch != 0
                && req.cycle_edge_admission.is_some_and(|admit| {
                    admit(
                        cached.declaring_class_id.as_u32(),
                        &class_name,
                        &method_name,
                        &descriptor,
                    )
                })
            {
                if let Some(target_class_id) = cp_invoke_class_id_resolver.and_then(|r| r(cp_idx)) {
                    let cell = JitCycleEdgeCell::new(
                        &class_name,
                        &method_name,
                        &descriptor,
                        cratonvm_types::ClassId::new(target_class_id),
                        true,
                    );
                    // The guard and the dispatch fallback both take the VM
                    // context from the frame, and the cell only ever admits a
                    // context-ABI body.
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry: Arc::as_ptr(&cell) as usize,
                            needs_context: true,
                            num_params,
                            return_type: ret_type,
                            guard_class_id: JIT_CYCLE_EDGE_CELL_GUARD,
                        },
                    ));
                    owned_cycle_cells.push(cell);
                    if crate::x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] cycle-edge-cell {}.{}{} @pc={pc} -> {class_name}.{method_name}{descriptor} class_id={target_class_id}",
                            cached.class_name, cached.method_name, cached.method_descriptor
                        );
                    }
                }
            }

            // RBC.3 — a site planned for inlining MUST still get a
            // `JitInvokeInfo` dispatch fallback (below). The codegen's
            // `try_emit_inline` can bail mid-body and roll back, and its
            // fall-through is direct_calls → invoke_info → else "assume
            // self-recursive CALL to own entry". With the old `continue`
            // here, a bailed inline site had neither, so the emitted CALL
            // targeted the CALLER's own entry: BC's `Strings.fromByteArray`
            // (invokestatic to same-class sibling `asCharArray`, planned for
            // inline, bailed in codegen) recursed itself — one `new String`
            // per level — until a native stack overflow killed the asn1
            // RegressionTest at StringTest (1,365 self-frames in the cdb
            // dump). Skip only the direct-call/intrinsic attempts, then fall
            // through to the info construction.
            let mut planned_inline = false;
            // A raw JIT-to-JIT CALL has no interpreter transition to validate
            // argument roots, ABI state, or the callee's active frame. The
            // Elasticsearch IVFKnn stress test exposed a stale precise-root
            // mirror after a raw JIT-to-JIT CALL. x64 now republishes the
            // caller RBP after every such return, matching the normal Rust
            // dispatch boundary, so compiled callee calls can remain enabled.
            // A raw JIT-to-JIT call updates the per-thread active-RBP mirror
            // to the callee, while the interpreter-owned root-chain entry
            // still describes the caller.  Until that transition publishes
            // callee metadata atomically as well, a GC in the callee can
            // select the caller's oop map for the callee frame and lose live
            // roots.  Route through the checked re-entrant bridge instead;
            // it installs a distinct JitEntryGuard for the actual callee.
            let direct_jit_callee_calls_enabled = direct_jit_callee_calls_enabled();
            // PGO-02: virtual/interface sites (0 | 2) go through the SAME
            // plan_inline call as static/special (3 | 1) below, but ONLY when
            // `class_id_name_resolver` is `Some` — i.e. only when
            // CRATONVM_JIT_GUARDED_VIRTUAL_INLINE has not been opted OUT of.
            // Since N8 that flag is default-ON (`=0` is the one-variable
            // bisect lever; see `InlineBackendCaps` above and
            // vm/src/runtime/env_cache.rs's `jit_guarded_virtual_inline`), so
            // the ordinary production path DOES take this arm at every virtual
            // and interface site. The gate is kept because it is still what
            // restores the exact pre-PGO-02 code path when the lever is
            // pulled, and because the bug below is a live hazard for anyone
            // who pulls it:
            // admitting 0|2 unconditionally broke that: `plan_inline` calls
            // `classify_receiver_shape` (previously reached for these sites
            // only under the removed `metrics::enabled()`-gated pre-tally)
            // before it ever consults `caps.guarded_inline_body_at_virtual_sites`,
            // so flag-off callers paid for and ran that admission machinery on
            // every virtual/interface call site in every compiled method for
            // the first time — confirmed via bisect
            // (f697d618ea, this same plumbing commit with zero codegen behind
            // it) to be the cause of a javac-self-hosting internal
            // AssertionError (`Check$SuperThisChecker`, 100% reproducible on
            // Spring AOT chunk 9) that has nothing to do with the feature this
            // flag gates. Gating virtual/interface admission on the resolver
            // being present restores the exact pre-PGO-02 code path (this
            // whole `if` skipped for 0|2) when the flag is off, matching
            // static/special's own unconditional admission, which this bug
            // never touched. The metrics-only pre-tally this replaced is not
            // restored — losing that one measurement when the feature is off
            // is an acceptable trade for not running unaudited machinery on
            // every unrelated JIT compile.
            let virtual_interface_inline_admitted = class_id_name_resolver.is_some();
            // `Integer.intValue()` at a site `invokevirtual_site_final_owner`
            // pinned as statically bound (`invoke_kind == 1`). The arm further
            // down still owns `invoke_kind == 0`; this one exists because that
            // arm can no longer see these sites at all, and widening ITS
            // condition would hand every other recogniser inside the
            // `(0 | 2)` block a shape none of them was written for.
            //
            // `class_name` is already the pin's substituted DECLARING class,
            // which for `intValue` is `java/lang/Integer` itself. The opcode is
            // still `0xb6`, so the x64 ladder adds the receiver back exactly as
            // it does for the unpinned shape. See
            // [`INTEGER_INT_VALUE_DIRECT_SITES`].
            if !is_recursive_call
                && invoke_kind == 1
                && direct_jit_callee_calls_enabled
                && int_value_direct_enabled()
                && class_name == "java/lang/Integer"
                && method_name == "intValue"
                && descriptor == "()I"
            {
                let entry = direct_native_helper(
                    direct_helpers.integer_int_value,
                    jdk_only,
                    direct_bind_admission,
                    &class_name,
                    &method_name,
                    &descriptor,
                );
                if entry != 0 {
                    needs_heap = true;
                    note_integer_int_value_direct_site();
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'I',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `ScopedMemoryAccess.*Unaligned` at a site
            // `invokevirtual_site_final_owner` pins as statically bound
            // (`invoke_kind == 1`): `ScopedMemoryAccess` is `final` and every
            // one of the sixteen accessors is real bytecode (no native to
            // exclude the pin from), so a virtual call against it is
            // reclassified here, before the `invoke_kind == 0` arm further
            // down ever sees the site -- exactly the shape
            // `Integer.intValue()`'s own `invoke_kind == 1` arm above exists
            // for, and a separate arm rather than a widened condition on that
            // one for the same reason: widening it would hand every other
            // recogniser sharing that block a shape none of them was written
            // for. See
            // `docs/feature-designs/jit-ir-optimizations.md`.
            //
            // `class_name` is already the pin's substituted DECLARING class
            // -- `jdk/internal/misc/ScopedMemoryAccess` itself here, since
            // the call already named it -- so `is_scoped_memory_op` still
            // matches.
            if direct_jit_callee_calls_enabled && scoped_memory_direct_enabled() && invoke_kind == 1
            {
                if let Some(op) = is_scoped_memory_op(&class_name, &method_name, &descriptor) {
                    let entry = op.helper_entry(direct_helpers);
                    // Round 16 wave 17 (lane mega5, NIO2-1).
                    if entry != 0
                        && scoped_memory_bind_admitted(
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        )
                    {
                        SCOPED_MEMORY_SITES_SP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: op.num_params(),
                                return_type: op.return_type(),
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }
            // JIT round 20 wave 3, lane atomics: `Reference.refersTo0` /
            // `PhantomReference.refersTo0` are PRIVATE, so their site
            // (in `refersToImpl`) arrives here as `invoke_kind == 1`
            // -- a private `invokevirtual` is pinned to kind 1 too. The
            // thin-bind arm below the virtual/interface block was
            // written for kind 0 or 1 but sits inside
            // `invoke_kind == 0 || invoke_kind == 2`, so it never saw
            // the site: `callee_compiler` refused the native callee
            // (`bg-direct-call DECLINED ..refersTo0..: native-shadow`)
            // and every call took the generic dispatch. Bind it here,
            // before `callee_compiler`, with the same op, gate and
            // admission question as that arm.
            if direct_jit_callee_calls_enabled && invoke_kind == 1 {
                if let Some(op) = is_unsafe_accessor_op(&class_name, &method_name, &descriptor)
                    .filter(|op| op.is_refers_to() && op.direct_enabled())
                {
                    let entry = op.helper_entry(direct_helpers);
                    if entry != 0
                        && direct_bind_admission.map_or(true, |admits| {
                            admits(&class_name, &method_name, &descriptor)
                        })
                    {
                        op.note_site(0);
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] refers-to-direct bind (sp) {class_name}.{method_name}{descriptor} @pc={pc}"
                            );
                        }
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: op.num_params(),
                                return_type: op.return_type(),
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }
            if !is_recursive_call
                && (invoke_kind == 3
                    || invoke_kind == 1
                    || (virtual_interface_inline_admitted && matches!(invoke_kind, 0 | 2)))
            {
                // Try inlining first (before direct calls — inlining is more profitable)
                //
                // PGO-02 R5: the budget check used to short-circuit the whole
                // block, so every site past the point of exhaustion vanished
                // from `inline_tally` entirely and `candidates` silently
                // under-reported. Record the refusal instead — that costs one
                // counter bump and no resolution, and it makes the tally a real
                // denominator ("of the sites considered, how many were
                // monomorphic") rather than "of the sites we happened to still
                // be pricing".
                let speculative_site = matches!(invoke_kind, 0 | 2);
                // Round 13 wave 1 (lane callcost): a static site the call-site
                // intrinsic ladder below emits inline is neither spliced nor
                // direct-bound first. Both run BEFORE the ladder, so a target
                // with no registered native (nothing refuses its bind) got a
                // CALL to its compiled JDK body instead of the intrinsic --
                // for `Math.fma` the `@IntrinsicCandidate` BigDecimal
                // fallback, `R12Rt3FmaEdges`' ~1000x. Only the flag read is
                // conditional on a match, so non-intrinsic sites pay one
                // `match`. `CRATONVM_JIT_STATIC_INTRINSIC_FIRST=0` restores
                // the old order.
                let static_intrinsic_site = invoke_kind == 3
                    && try_resolve_intrinsic(&class_name, &method_name, &descriptor).is_some()
                    && static_intrinsic_first_enabled();
                // Resolving the callee for a SPECULATED receiver class. Bound
                // here so it borrows this site's names for the whole block.
                let receiver_body = |receiver_class_id: u32| {
                    receiver_inline_resolver
                        .and_then(|f| f(receiver_class_id, &class_name, &method_name, &descriptor))
                };
                // Class-hierarchy analysis for this site, bound the same way.
                // `plan_inline` calls it at most once per site and only on the
                // arms where no receiver profile answered, so the subtype walk
                // behind it is not paid for at a site the profile already
                // decides.
                let unique_concrete = || {
                    unique_concrete_resolver.and_then(|f| f(&class_name, &method_name, &descriptor))
                };
                // The bisect levers apply HERE too — see
                // `InlineRefusal::ForceInterpreted` for what it cost that they
                // did not. Checked against the CONSTANT-POOL name, which is the
                // callee for the statically bound kinds (3 | 1).
                //
                // A guarded virtual/interface site resolves its body per
                // receiver class, so the constant-pool name is NOT the name a
                // bisect would type after reading a stack trace — for
                // `TreeMap$EntryIterator.hasNext` the receiver class and the
                // declaring class are two different classes. This site does not
                // have the receiver's name and cannot ask. That gap is CLOSED
                // one level down, not here: the VM's `resolve_inline_site_from`
                // (vm/src/runtime/interpreter/jit_bridge.rs) asks
                // `jit_force_interpret` of BOTH the selected declaring class
                // and the receiver class, and refuses the body outright, so a
                // denied receiver never produces a splice for this planner to
                // admit. The check kept here is the statically bound half, and
                // the cheap one: it refuses before any resolver runs.
                if jit_force_interpret(&class_name, &method_name) {
                    inline_tally.record_refusal(&InlineRefusal::ForceInterpreted);
                } else if inline_budget_remaining == 0 {
                    inline_tally.record_refusal(&InlineRefusal::BudgetAlreadySpent);
                } else {
                    // A guarded site's callee is NOT the constant-pool callee
                    // (see `receiver_inline_resolver`), so it is not resolved
                    // here at all — `plan_inline` resolves one body per guard
                    // class through `receiver_callee_resolver`. Resolving the
                    // constant-pool name for a virtual site would also refuse
                    // every `invokeinterface` outright, since an interface
                    // method declaration carries no `Code`.
                    let cp_site = if speculative_site || static_intrinsic_site {
                        None
                    } else {
                        inline_resolver
                            .as_ref()
                            .and_then(|f| f(&class_name, &method_name, &descriptor))
                    };
                    if !speculative_site
                        && !static_intrinsic_site
                        && cp_site.is_none()
                        && inline_resolver.is_some()
                    {
                        // A resolver ran and declined the constant-pool callee
                        // (native, oversized, or an ineligible construct).
                        // Counted, so `candidates` means "sites considered"
                        // and the histogram has a denominator. Gated on the
                        // resolver EXISTING so a compile with inlining wholly
                        // disabled does not report every site as an
                        // unresolvable callee, which would be a different
                        // claim.
                        inline_tally.record_refusal(&InlineRefusal::CalleeUnresolved {
                            speculated_class_id: None,
                        });
                    }
                    // A switch dispatcher's straight-line tail-exit arm keeps a
                    // looping kernel a call (code size, not a per-iteration call;
                    // `ir_splice_order::switch_tail_loop_callee_refused`).
                    let switch_tail_loop = cp_site.as_ref().is_some_and(|s| {
                        ir_splice_order::switch_tail_loop_site(code, code_len, pc, s)
                    });
                    if switch_tail_loop {
                        inline_tally.record_refusal(&InlineRefusal::CalleeTooLarge);
                    }
                    let cp_site = cp_site.filter(|_| !switch_tail_loop);
                    if speculative_site || cp_site.is_some() {
                        {
                            // Real budget accounting in two independent
                            // dimensions (jit-inlining-and-ir-calls):
                            //  * PER SITE — the HotSpot three-tier ceiling. A
                            //    hot site gets `FreqInlineSize`; a cold one
                            //    keeps `MaxInlineSize`, which is exactly the
                            //    old flat cap. Without this tier the raised
                            //    admission constant (`MAX_INLINE_BYTECODE_SIZE`,
                            //    now 325, which is what the VM-side resolver
                            //    filters on) would splice 300-byte COLD callees
                            //    everywhere.
                            //  * PER METHOD — the running budget below, which
                            //    is what actually bounds code-size explosion:
                            //    the single-pass backend reserves ~64 buffer
                            //    bytes and ~1 frame slot per inlined bytecode,
                            //    both linear in this total.
                            //
                            // C2-review P1: both dimensions now live in
                            // `plan_inline`, together with the depth,
                            // recursion, exception-range and dependency rules
                            // this site had none of. For a statically bound
                            // callee (`invoke_kind` 3 | 1) the admission
                            // decision is the SAME size/expansion/budget
                            // arithmetic as before, so an unprofiled compile
                            // inlines exactly as it did; what is new is that a
                            // refusal now says why, and that the dependencies
                            // are taken from the plan rather than assumed. For
                            // a virtual/interface callee (0 | 2, PGO-02) the
                            // policy additionally requires a dominant receiver
                            // shape and a resolvable invalidation dependency —
                            // see `classify_receiver_shape` / `receiver_class_namer`
                            // below — before it will even consider the guard the
                            // backend caps must also allow.
                            let site_hot = call_site_is_hot(pc, &inline_hot_loops, profile);
                            // Ancestors only. The single-pass emitter DOES
                            // nest now (`try_emit_nested_inline` /
                            // `emit_guarded_nested_inline`, bounded by
                            // [`MAX_INLINE_NEST_DEPTH`]), but the nested plan
                            // is carried on `InlineSite::nested_sites` and
                            // resolved VM-side, not re-planned here — so the
                            // inline stack this request describes is still
                            // [caller, callee] and the only recursion it can
                            // see is callee == caller, which
                            // `is_recursive_call` has already excluded above.
                            // Computed rather than hard-coded to `0` so a
                            // planner that ever walks the nest inherits a
                            // correct count.
                            //
                            // Keyed off the CONSTANT-POOL names for a
                            // speculative site: the receiver-resolved bodies do
                            // not exist yet at this point, and a self-recursive
                            // virtual call names its own class at the site
                            // anyway.
                            let recursive_copies = usize::from(
                                cp_site
                                    .as_ref()
                                    .map_or(class_name == &*cached.class_name, |s| {
                                        s.class_name == &*cached.class_name
                                    })
                                    && method_name == &*cached.method_name
                                    && descriptor == &*cached.method_descriptor,
                            );
                            // Round nio20 wave 4 (lane L3, W3L3-1): the
                            // precise-frames interlock, answered per site as
                            // the single-pass OSR door answers it
                            // (`plan_osr_door_splices`) -- see
                            // [`sp_entry_precise_interlock_holds`].
                            let precise_interlock_at_site = sp_entry_precise_interlock_holds(
                                precise_exception_frames,
                                &cached.exception_table,
                                pc,
                                invoke_kind,
                                cp_site.as_ref(),
                            );
                            let plan = plan_inline(&InlineRequest {
                                pc,
                                invoke_kind,
                                site_method_name: &method_name,
                                site_descriptor: &descriptor,
                                site_class_name: &class_name,
                                site: cp_site.as_ref(),
                                site_is_hot: site_hot,
                                receivers: profile.and_then(|p| p.receivers.get(&pc)),
                                call_site_evidence: profile
                                    .map(|p| p.call_site_count(pc))
                                    .unwrap_or(profile::CallSiteEvidence::None),
                                depth: 1,
                                recursive_copies,
                                budget_remaining: inline_budget_remaining,
                                pc_in_protected_range: pc_in_protected_range(
                                    &cached.exception_table,
                                    pc,
                                ),
                                precise_exception_frames: precise_interlock_at_site,
                                // PGO-02: `class_id_name_resolver` doubles as the
                                // feature's on/off gate. The VM call sites pass it
                                // by DEFAULT since N8 and withhold it only under
                                // CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0, so an
                                // absent resolver means either that opt-out or an
                                // embedder that built its own request; either way
                                // it both disables guarded virtual/interface
                                // speculation AND is the reason plan_inline would
                                // refuse one anyway (no invalidation dependency
                                // could be recorded). Static/special DirectBind
                                // sites are unaffected either way — they never
                                // consult this.
                                caps: InlineBackendCaps {
                                    guarded_inline_body_at_virtual_sites: class_id_name_resolver
                                        .is_some(),
                                    ..InlineBackendCaps::single_pass_x64()
                                },
                                receiver_class_namer: class_id_name_resolver,
                                receiver_callee_resolver: Some(&receiver_body),
                                // CHA rides the SAME gate as the guard it
                                // feeds: its verdict is lowered as a receiver
                                // class-id compare, so without a resolver to
                                // name the class this plan would be refused as
                                // `NoInvalidationDependency` anyway. Passing it
                                // unconditionally would also pay for a subtype
                                // walk on every unprofiled virtual site of
                                // every compile under
                                // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0` —
                                // exactly the shape of the regression the gate
                                // above exists to prevent.
                                unique_concrete_resolver: if class_id_name_resolver.is_some() {
                                    Some(&unique_concrete)
                                } else {
                                    None
                                },
                            });
                            inline_tally.record(&plan);
                            // Name the planner's verdict per site. The RESOLVER
                            // names its own refusals now; this is the other half
                            // — a site whose body resolved fine can still be
                            // refused here on budget or policy, and a bare
                            // `nested-splice=0` cannot tell the two apart.
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] inline-plan pc={pc} {}.{}{}: {:?} (cost={:?} budget_left={})",
                                    class_name,
                                    method_name,
                                    descriptor,
                                    plan.verdict,
                                    cp_site
                                        .as_ref()
                                        .and_then(crate::inline_site_expansion_cost),
                                    inline_budget_remaining,
                                );
                            }
                            // PGO-02: the backend emits both speculative
                            // verdicts now — Monomorphic (one guard) and
                            // Bimorphic (a two-guard chain sharing one receiver
                            // load, one null check and one dispatch tail). See
                            // docs/feature-designs/profile-guided-inlining.md
                            // §5, and `x64::bytecode_walk`'s guard chain.
                            if plan.is_admitted() {
                                // The bodies to splice. For a guarded site each
                                // is the one ITS GUARD'S CLASS dispatches to,
                                // which `plan_inline` resolved and carried on
                                // the plan — never the constant-pool callee,
                                // which is a different method wherever the
                                // speculated class overrides the declared one.
                                // A statically bound site has exactly one body
                                // and no guard, so its `0` class id is never
                                // compared against anything.
                                let variants: Vec<(u32, InlineSite)> = if plan.is_speculative() {
                                    plan.speculative_sites.clone()
                                } else {
                                    cp_site.clone().map(|s| (0u32, s)).into_iter().collect()
                                };
                                // An admitted plan with no body is
                                // unrepresentable — `plan_inline` refuses both
                                // the unresolved-speculation and the
                                // unresolved-CP-callee cases. Handled rather
                                // than unwrapped so a future verdict that
                                // forgets to carry one leaves the SITE on
                                // normal dispatch instead of panicking.
                                if let Some((_, primary)) = variants.first().cloned() {
                                    if plan.is_speculative() {
                                        inline_guard_variants.insert(pc, variants.clone());
                                    }
                                    inline_budget_remaining =
                                        inline_budget_remaining.saturating_sub(plan.expansion_cost);
                                    // EVERY spliced body's requirement, not just
                                    // the first: a second guarded body that
                                    // touches the heap needs the context just as
                                    // much as the first one does.
                                    if variants.iter().any(|(_, s)| s.needs_heap) {
                                        needs_heap = true;
                                    }
                                    let spliced = primary;
                                    // The invalidation channel. Deduplicated:
                                    // the scans are `.any()` predicates run on
                                    // every class define, so a repeated triple
                                    // is pure cost.
                                    for dependency in plan.invalidation_triples() {
                                        if !inlined_methods.contains(&dependency) {
                                            inlined_methods.push(dependency);
                                        }
                                    }
                                    inline_sites.insert(pc, spliced);
                                    planned_inline = true;
                                    if crate::x64::dbg_jitc_enabled() {
                                        eprintln!(
                                            "[cratonvm-jitc] inline-planned {class_name}.{method_name}{descriptor} @pc={pc}"
                                        );
                                    }
                                }
                            } else if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] inline-refused {class_name}.{method_name}{descriptor} @pc={pc} reason={}",
                                    plan.refusal().map_or("none", InlineRefusal::category)
                                );
                            }
                        }
                    }
                }

                if !planned_inline {
                    if invoke_kind == 3 {
                        STATIC_SITES_SEEN_SINGLEPASS
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // `Integer.valueOf(I)` thin direct call (see
                    // `INTEGER_VALUE_OF_DIRECT_FN`): statically bound, native
                    // callee — the eager callee-compile attempt below can
                    // never succeed for it, and the generic dispatch fallback
                    // pays the full helper round trip per call. `needs_context`
                    // routes vm_ptr as arg 0; the helper preserves the
                    // identity-cache and pending-return rooting contracts.
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/StringLatin1"
                        && method_name == "toLowerCase"
                        && descriptor
                            == "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;"
                        // W5L4-6 (JIT round 20 wave 11, lane pNat): while
                        // `CRATONVM_STRING_LATIN1_LOWER_RETIRED` (default ON)
                        // retires the registered native, the JDK body runs
                        // and compiles; binding the thin helper here would
                        // put the native back for compiled callers only
                        // (`phases_early::string_latin1_lower_retired`).
                        && !true
                    {
                        // JDK-ONLY-WAVE2 §4 — the policy half is CLOSED, the
                        // "delete the literals" half is NOT ACHIEVABLE, and the
                        // second verdict is the one worth reading.
                        //
                        // The original ask was to replace the name matching with
                        // a resolver that asks whether the triple is an approved
                        // `NativeKind::Intrinsic`, and delete the literals.
                        // Half of that landed: `direct_native_helper` takes an
                        // `direct_bind_admission` and the VM answers it out of the
                        // registry's kind, so the BIND decision is no longer
                        // name-based in either mode.
                        //
                        // The literals cannot follow, because they are not the
                        // policy — they are the RECOGNITION. Each triple picks
                        // which `*_DIRECT_FN` cell this site maps to, and that
                        // is a triple-to-helper-address map the native registry
                        // has never held: a kind says whether a native may be
                        // bound, not which of seven thin VM-side helpers
                        // reimplements it. Deleting them would delete the
                        // mapping, not a name check. Do NOT "finish" this by
                        // removing the list; every entry is a measured hot path
                        // and the resolver it was meant to defer to cannot
                        // answer the question the list answers.
                        //
                        // The `String.toLowerCase(Locale)` ladder that used to
                        // sit beside this one IS gone (2026-08-04). It was the
                        // third copy of the forced-native `java/lang/String`
                        // policy — `check_override` forced that name, the warm
                        // gate refused it, and this bound it: three paths,
                        // three answers for one method.
                        //
                        // This one stays, and the difference is not that it is
                        // a different class — it is that its triple really IS
                        // registered `NativeKind::Intrinsic`
                        // (`register_string_latin1_natives`), so binding it is
                        // §1.4's reviewed exception rather than a native
                        // shadowing bytecode. It also accelerates the real
                        // `String.toLowerCase(Locale)` bytecode instead of
                        // replacing it, and its input is Latin-1 by
                        // construction, so it cannot reach the surrogate cases
                        // that made the `String`-level native diverge.
                        //
                        // That premise is a KIND, and this ladder cannot check
                        // kinds — so it is pinned by
                        // `the_jit_latin1_lower_ladder_binds_a_reviewed_intrinsic`
                        // in `vm/tests/wp8_10_9_string_contains_native.rs`.
                        // Re-tagging the native `Bridge` would silently turn
                        // this bind into a §1.4 violation that only compiled
                        // frames can observe.
                        let entry = direct_native_helper(
                            direct_helpers.string_latin1_lower,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 3,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // `Thread.currentThread()` thin direct call (see
                    // `THREAD_CURRENT_THREAD_DIRECT_FN`). Same shape as the
                    // `Integer.valueOf` bind below — statically bound,
                    // native callee, no compiled body to find — but with no
                    // arguments at all: `num_params: 0` with
                    // `needs_context: true` makes `emit_stack_arg_setup` pass
                    // `vm_ptr` in ARG_REGS[0] and nothing else.
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/Thread"
                        && method_name == "currentThread"
                        && descriptor == "()Ljava/lang/Thread;"
                    {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        let entry = direct_native_helper(
                            direct_helpers.thread_current_thread,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            THREAD_CURRENT_THREAD_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 0,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // nio21 wave 3, lane L4: `Thread.currentCarrierThread()`
                    // -- the arm above with its own helper (the registered
                    // native is the same `native_thread_current_thread`).
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/Thread"
                        && method_name == "currentCarrierThread"
                        && descriptor == "()Ljava/lang/Thread;"
                        && crate::direct_helpers::carrier_thread_direct_enabled()
                    {
                        let entry = direct_native_helper(
                            direct_helpers.thread_current_carrier_thread,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            THREAD_CURRENT_THREAD_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 0,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = system_clock_direct_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 0,
                                    return_type: b'J',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // `Preconditions.checkIndex` / `Reference.reachabilityFence`
                    // thin direct calls. Both are census-driven rather than
                    // guessed: `--dump-native-registry` reports a per-native
                    // invocation count, and on `probes/NioAccessorRate.java`
                    // these two are the top two entries — 4 000 000 and
                    // 3 200 000 calls for 800 000 `ByteBuffer` accessor
                    // operations, i.e. ~4.5 of the ~7 native calls a single
                    // `ByteBuffer.putLong` executes. Both are statically bound
                    // with a registered native callee, so `callee_compiler`
                    // can never find a body for them and the generic dispatch
                    // fallback pays the full ~160 ns funnel per call.
                    //
                    // JDK-ONLY-WAVE2: see the marker on the
                    // `StringLatin1.toLowerCase` bind above — same list.
                    if direct_jit_callee_calls_enabled
                        && census_direct_helpers_enabled()
                        && invoke_kind == 3
                        && class_name == "jdk/internal/util/Preconditions"
                        && method_name == "checkIndex"
                        && descriptor == "(IILjava/util/function/BiFunction;)I"
                    {
                        let entry = direct_native_helper(
                            direct_helpers.preconditions_check_index,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            PRECONDITIONS_CHECK_INDEX_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 3,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/System"
                        && ((method_name == "nanoTime" && descriptor == "()J")
                            || (method_name == "currentTimeMillis" && descriptor == "()J"))
                    {
                        let cell = if method_name == "nanoTime" {
                            direct_helpers.system_nano_time
                        } else {
                            direct_helpers.system_current_time_millis
                        };
                        let entry = direct_native_helper(
                            cell,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 0,
                                    return_type: b'J',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // JIT round 16 (lane strings, STR16-6): the JDK's
                    // int/long -> LATIN1 digit writer as one thin call. See
                    // `decimal_digits_latin1_helper` (the optimizing door asks
                    // the same function). `(value, index, buf)`, three
                    // operand-stack words for both overloads.
                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = decimal_digits_latin1_helper(
                            direct_helpers,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            DECIMAL_DIGITS_CENSUS[DECIMAL_DIGITS_SITES_SP]
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 3,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // JIT round 17 wave 1, lane nio (NIO2-2): the fence as NO
                    // code, ahead of the thin-helper CALL below. Not a native
                    // shadowing bytecode in either mode: JDK 25's body is an
                    // empty `return`, so emitting nothing IS the bytecode's
                    // semantics, and no registry row is consulted or
                    // bypassed. See `JitIntrinsic::ReachabilityFence` for why
                    // the referent stays mapped up to this pc.
                    if invoke_kind == 3
                        && is_reachability_fence_site(&class_name, &method_name, &descriptor)
                        && reachability_fence_elide_enabled()
                    {
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!("[cratonvm-jitc] reachability-fence elided (sp) @pc={pc}");
                        }
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry: JitIntrinsic::ReachabilityFence.as_entry(),
                                needs_context: false,
                                num_params: 1,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }

                    // Strings round 2 (`str17`): `ArraysSupport.vectorizedHashCode`
                    // as one thin call (see `vectorized_hash_code_helper`, which
                    // the optimizing door asks too). `(array, fromIndex, length,
                    // initialValue, basicType)`, five operand-stack words.
                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = vectorized_hash_code_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 5,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // JIT round 23 wave 4, lane L5: `Array.newArray(Class, int)`
                    // as one thin call (see `reflect_new_array_helper`, which
                    // the optimizing door asks too). `(component, length)`,
                    // two operand-stack words, an array (or null) back.
                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = reflect_new_array_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] reflect-new-array-direct bind (sp) {class_name}.{method_name} @pc={pc}"
                                );
                            }
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 2,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // JIT round 22 wave 1, lane intrinsics (INTRINSICS21-1):
                    // `ArraysSupport.vectorizedMismatch` as one thin call (see
                    // `vectorized_mismatch_helper`, which the optimizing door
                    // asks too). `(a, aOffset, b, bOffset, length,
                    // log2ArrayIndexScale)`, six operand-stack slots (one per
                    // parameter, the two `long`s included: `count_param_slots`).
                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = vectorized_mismatch_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 6,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // JIT round 22 wave 2, lane intrinsics (INTRINSICS22-2):
                    // `CRC32.updateBytes0` / `updateBytes`, `Adler32.updateBytes`
                    // as one thin call (see `zip_checksum_helper`, which the
                    // optimizing door asks too). `(checksum, b, off, len)`,
                    // four operand-stack slots.
                    if direct_jit_callee_calls_enabled && invoke_kind == 3 {
                        let entry = zip_checksum_helper(
                            direct_helpers,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 4,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    if direct_jit_callee_calls_enabled
                        && census_direct_helpers_enabled()
                        && invoke_kind == 3
                        && class_name == "java/lang/ref/Reference"
                        && method_name == "reachabilityFence"
                        && descriptor == "(Ljava/lang/Object;)V"
                    {
                        let entry = direct_native_helper(
                            direct_helpers.reachability_fence,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            REACHABILITY_FENCE_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: b'V',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/Integer"
                        && method_name == "valueOf"
                        && descriptor == "(I)Ljava/lang/Integer;"
                    {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        let entry = direct_native_helper(
                            direct_helpers.integer_value_of,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // `Long.valueOf(J)` — the twin of the `Integer.valueOf(I)`
                    // bind directly above, on the same terms and for the same
                    // reason (see `LONG_VALUE_OF_DIRECT_FN` for the measured
                    // gap that says the twin was missing rather than declined).
                    //
                    // `num_params: 1` and not 2: the JIT's `count_param_slots`
                    // counts one operand slot per PARAMETER, not JVMS
                    // category-2 pairs, so the `J` argument the emitter pops
                    // for this site is a single 64-bit slot — the same count
                    // `INTEGER_VALUE_OF`'s `I` gets.
                    if direct_jit_callee_calls_enabled
                        && long_box_direct_helpers_enabled()
                        && invoke_kind == 3
                        && class_name == "java/lang/Long"
                        && method_name == "valueOf"
                        && descriptor == "(J)Ljava/lang/Long;"
                    {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        let entry = direct_native_helper(
                            direct_helpers.long_value_of,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            LONG_VALUE_OF_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    // W2A / Item 4: `System.identityHashCode(Object)` single-pass thin CALL bind.
                    // Emitted with SpThinPrefix::IdentityHashCode to keep the inline header read.
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && identity_hash_direct_enabled()
                        && is_identity_hash_code_site(&class_name, &method_name, &descriptor)
                    {
                        let entry = direct_native_helper(
                            direct_helpers.identity_hash_code,
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            &descriptor,
                        );
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: b'I',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // STATICALLY BOUND ONLY (`invokespecial` / `invokestatic`).
                    //
                    // `callee_compiler` is asked about the constant-pool
                    // RESOLVED (class, method, descriptor). For an
                    // `invokevirtual` / `invokeinterface` that names the STATIC
                    // receiver type, not the runtime one — so binding its
                    // compiled entry here, with `guard_class_id: 0` and no
                    // receiver check at all, calls THAT body for every
                    // receiver, including one whose class overrides the method.
                    //
                    // The CRC32 note further down this ladder already states
                    // the assumption ("this invokestatic/invokespecial path
                    // never resolves a CRC32 intrinsic (those are
                    // `invokevirtual` only), so `guard_class_id` is 0") — it
                    // was simply never enforced, and the enclosing
                    // `matches!(invoke_kind, 0..=3)` lets 0 and 2 in.
                    //
                    // Virtual and interface sites are NOT unhandled: they have
                    // their own block below (`invoke_kind == 0 || == 2`), where
                    // every direct bind is either a `final` class, a helper
                    // that re-checks the receiver's exact class itself, or a
                    // `guard_class_id` the codegen compares at runtime — and
                    // failing all of those they fall through to the MIC/PIC
                    // inline cache, which is class-id guarded by construction.
                    //
                    // Measured: H2's `org.h2.value.VersionedValue.
                    // getCurrentValue()` returns `(T) this` (H2's "a raw value
                    // is its own VersionedValue" trick) and
                    // `VersionedValueCommitted` overrides it with
                    // `return value`. Once the BASE was compiled, every
                    // `v.getCurrentValue()` in `VersionedValueType.write` —
                    // static type `VersionedValue` — reached the base body and
                    // returned the WRAPPER, which the caller casts to the
                    // payload type: `ClassCastException:
                    // VersionedValueUncommitted cannot be cast to
                    // org.h2.value.Value`, deterministically, single-threaded,
                    // inside 1000 rows of a plain JDBC `MERGE` loop. The
                    // callee filters (no natives, no non-empty exception table)
                    // are why this only bites a small overridable method.
                    if direct_jit_callee_calls_enabled
                        && matches!(invoke_kind, 1 | 3)
                        && !static_intrinsic_site
                    {
                        if let Some(compiler) = callee_compiler.as_ref() {
                            let probed = compiler(&class_name, &method_name, &descriptor);
                            if probed.is_none() {
                                // See `DIRECT_CALLEE_BIND_HITS`: this site is now
                                // bound to `jit_invoke_dispatch` permanently, and
                                // the callee is very often compiled moments later.
                                DIRECT_CALLEE_BIND_MISSES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                sp_late_static_declined = true;
                            }
                            if let Some((entry, callee_needs_ctx)) = probed {
                                DIRECT_CALLEE_BIND_HITS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if verdicts.direct_call_requires_dispatch_of(
                                    cycle_class,
                                    &method_name,
                                    &descriptor,
                                    site_class_id,
                                ) {
                                    needs_heap = true;
                                    verdicts.mark_current_compile_method_recursive_cycle();
                                } else {
                                    if callee_needs_ctx {
                                        needs_heap = true;
                                    }
                                    // A raw JIT-to-JIT CALL has no Rust frame
                                    // in between, so the ONLY thing that can
                                    // notice the callee trapped is the
                                    // callee-deopt service check the codegen
                                    // emits after the CALL — and that check
                                    // needs a `JitInvokeInfo` for this pc to
                                    // name the callee it just invoked. The
                                    // `continue` below skips the generic
                                    // fallback registration at the end of this
                                    // loop, so register one here, exactly as
                                    // the `ArraycopyPrimitive` arm below does
                                    // for the same reason.
                                    //
                                    // Without it the callee's `i64::MIN`
                                    // sentinel reached THIS method's shared
                                    // exception-check stub, which reloads the
                                    // sentinel and returns — the callee's
                                    // reconstructed frame stayed in the
                                    // thread's one stash slot and travelled up
                                    // to a consumer that could not attribute
                                    // it. Measured on H2 `TestScript`:
                                    // `ValueVarchar.get(String,
                                    // CastDataProvider)` at pc 38 calls
                                    // `StringUtils.cache`, which guard-bailed
                                    // at its own bci 54, and the orphan
                                    // surfaced two frames up
                                    // (jit-direct-call-mints-an-orphaned-deopt-frame).
                                    // 582 sites in one run had no service
                                    // check; every one of them is a place an
                                    // orphan can be minted.
                                    //
                                    // Note the sibling arm below (the
                                    // INLINE-BAIL FALLBACK) already documents
                                    // that it deliberately does NOT `continue`
                                    // so this registration still happens; only
                                    // this copy of the bind skipped it.
                                    let class_ref = intern_jit_invoke_str(&class_name);
                                    let method_ref = intern_jit_invoke_str(&method_name);
                                    let desc_ref = intern_jit_invoke_str(&descriptor);
                                    let info = Box::new(JitInvokeInfo {
                                        class_name: class_ref,
                                        method_name: method_ref,
                                        descriptor: desc_ref,
                                        num_jit_args,
                                        return_type: ret_type,
                                        invoke_kind,
                                        declaring_class_id: cached.declaring_class_id.as_u32(),
                                        owner_class_id,
                                    });
                                    let info_ptr: *const JitInvokeInfo = &*info;
                                    owned_invoke_infos.push(info);
                                    invoke_info.push((pc, info_ptr));
                                    direct_callee_entries
                                        .push((entry, jit_entry_artifact_id(entry)));
                                    // Round 9 wave 9 (`evict9`): an in-loop
                                    // `invokestatic` bind goes through a
                                    // retire cell under
                                    // `CRATONVM_JIT_RETIRE_CELL`; otherwise
                                    // this is the plain row, unchanged.
                                    direct_calls.push((
                                        pc,
                                        retirable_direct_call(
                                            code,
                                            code_len,
                                            pc,
                                            invoke_kind,
                                            entry,
                                            callee_needs_ctx,
                                            num_params,
                                            ret_type,
                                            &class_name,
                                            &method_name,
                                            &descriptor,
                                            &mut owned_cycle_cells,
                                        ),
                                    ));
                                    continue;
                                }
                            }
                        }
                    }
                    // Round 11 wave 13 (lane `spsync`, page
                    // `r11w12-calls-single-pass-sync-direct-patch`): the
                    // single-pass twin of the IR caller-held synchronized
                    // direct CALL. A `static synchronized` callee always gets
                    // here (`callee_compiler` refuses it as
                    // `DirectBindRefusal::Synchronized`). The VM answers only
                    // for a published body that can neither trap, nor call, nor
                    // deopt, so nothing can stop it while this method holds a
                    // monitor no frame state names. The mirror rides an
                    // `ldc_class_info` row at the INVOKE pc: only the `ldc`
                    // arms read that table by pc, so an invoke pc there is inert
                    // for every other reader.
                    //
                    // Wave 14: the gates are one predicate,
                    // `compile_request::single_pass_sync_direct_site_admitted`,
                    // which the VM's single-pass OSR door asks too.
                    if invoke_kind == 3
                        && opcode == 0xb8
                        && direct_jit_callee_calls_enabled
                        && crate::compile_request::single_pass_sync_direct_site_admitted(
                            verdicts,
                            helpers,
                            direct_helpers,
                            &class_name,
                            &method_name,
                            &descriptor,
                            ret_type,
                        )
                    {
                        if let Some(t) = req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx)) {
                            if t.entry != 0 && !t.instance {
                                needs_heap = true;
                                // The site's `JitInvokeInfo`, as the plain bind
                                // above registers it: the codegen's dispatch
                                // arm for a pc it cannot serve with the monitor
                                // held reads it, and so does the deopt typing
                                // of the site.
                                let class_ref = intern_jit_invoke_str(&class_name);
                                let method_ref = intern_jit_invoke_str(&method_name);
                                let desc_ref = intern_jit_invoke_str(&descriptor);
                                let info = Box::new(JitInvokeInfo {
                                    class_name: class_ref,
                                    method_name: method_ref,
                                    descriptor: desc_ref,
                                    num_jit_args,
                                    return_type: ret_type,
                                    invoke_kind,
                                    declaring_class_id: cached.declaring_class_id.as_u32(),
                                    owner_class_id,
                                });
                                let info_ptr: *const JitInvokeInfo = &*info;
                                owned_invoke_infos.push(info);
                                invoke_info.push((pc, info_ptr));
                                // Keeps the callee mapped, and puts this method
                                // in its invalidation closure.
                                direct_callee_entries
                                    .push((t.entry, jit_entry_artifact_id(t.entry)));
                                // The monitor: the caller's own Methodref
                                // `class_index`, resolved through this class's
                                // loader exactly as the IR route's `ConstClass`.
                                let holder = cached.declaring_class_id.as_u32();
                                ldc_class_info.push((pc, holder, t.class_cp_idx));
                                // The VM's GC-maintained mirror word, so every
                                // load after the first skips the helper (the
                                // IR route bakes the same slot).
                                if t.mirror_slot != 0 {
                                    ldc_slots
                                        .entry((holder, t.class_cp_idx))
                                        .or_insert(t.mirror_slot);
                                }
                                direct_calls.push((
                                    pc,
                                    JitDirectCall {
                                        entry: t.entry,
                                        needs_context: t.needs_ctx,
                                        num_params,
                                        return_type: ret_type,
                                        guard_class_id: JIT_SYNC_DIRECT_GUARD,
                                    },
                                ));
                                if crate::x64::dbg_jitc_enabled() {
                                    eprintln!(
                                        "[cratonvm-jitc] sp-sync-direct-call {class_name}.{method_name}{descriptor} @pc={pc} entry={:#x} mirror_slot={:#x}",
                                        t.entry, t.mirror_slot
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    // Round 13 wave 1 (lane `sync`, page
                    // `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow`,
                    // option 1): the INSTANCE twin of the arm above, and the
                    // single-pass mirror of the optimizing tier's two instance
                    // arms (`ir-sync-direct-call ... receiver-monitor`). Without
                    // it a compiled caller's `synchronized` instance callee goes
                    // through `jit_invoke_dispatch` / the inline-cache miss helper
                    // and the monitored bytecode-callee door on EVERY call (the
                    // only body published for it is a wrapped entry no raw door
                    // may CALL): ~400 ns a call against HotSpot's ~35.
                    //
                    // The caller holds the RECEIVER's monitor around a CALL of the
                    // monitor-free body, under the static arm's contract (the VM
                    // answers only for a published body that can neither trap, nor
                    // call, nor deopt). Which body a receiver selects decides the
                    // shape, as on the optimizing tier:
                    //
                    // * statically bound (`invoke_kind == 1`: an `invokespecial`,
                    //   or an `invokevirtual` the VM pinned to a private/final
                    //   target): the VM substituted the selected owner and it IS
                    //   the Methodref's own class, whose declaration is the one
                    //   the lookup answers for; an `invokevirtual` also needs a
                    //   target no receiver can override. No class guard; the
                    //   backend null-checks the receiver at the invoke (JVMS 6.5).
                    // * genuinely virtual (`invoke_kind == 0`): class-hierarchy
                    //   analysis says the Methodref's class C is the only
                    //   instantiable provider at or below itself, and the backend
                    //   guards on EXACTLY C (`cha_sync_exact_class`). A receiver of
                    //   class C selects C's own declaration whatever is loaded
                    //   later; any other receiver, null included, takes the miss
                    //   edge to the ordinary dispatch through this site's
                    //   `JitInvokeInfo` (kind 0, so it still dispatches
                    //   virtually). The guard is the soundness argument, not the
                    //   hierarchy, so no dependency is recorded.
                    //
                    // `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE=0` is the kill switch.
                    if matches!((opcode, invoke_kind), (0xb6, 0 | 1) | (0xb7, 1))
                        && method_name != "<init>"
                        && class_name != "java/lang/Object"
                        && direct_jit_callee_calls_enabled
                        && sp_sync_direct_instance_enabled()
                        && crate::compile_request::single_pass_sync_direct_site_admitted(
                            verdicts,
                            helpers,
                            direct_helpers,
                            &class_name,
                            &method_name,
                            &descriptor,
                            ret_type,
                        )
                    {
                        let exact_class_id = if invoke_kind == 1 {
                            let owner_is_cp_class = owner_class_id != 0
                                && resolver(cp_idx)
                                    .is_some_and(|(cp_class, _, _)| cp_class == class_name);
                            owner_is_cp_class.then_some(0u32)
                        } else {
                            crate::compile_request::cha_sync_exact_class(
                                unique_concrete_resolver
                                    .and_then(|cha| cha(&class_name, &method_name, &descriptor)),
                                cp_invoke_class_id_resolver.and_then(|r| r(cp_idx)),
                            )
                        };
                        let bound = exact_class_id.and_then(|exact| {
                            let t = req.sync_direct_lookup.and_then(|lookup| lookup(cp_idx))?;
                            let selects = exact != 0 || opcode == 0xb7 || t.unoverridable;
                            let guard = sync_direct_instance_guard(exact)?;
                            (t.entry != 0 && t.instance && selects).then_some((t, guard))
                        });
                        if let Some((t, guard)) = bound {
                            needs_heap = true;
                            // The site's `JitInvokeInfo`, as the static arm
                            // registers it: the guard's miss edge and a pc the
                            // backend cannot serve with the monitor held dispatch
                            // through it (its own kind, 0 or 1), and the
                            // backend's argument-service range needs it.
                            let class_ref = intern_jit_invoke_str(&class_name);
                            let method_ref = intern_jit_invoke_str(&method_name);
                            let desc_ref = intern_jit_invoke_str(&descriptor);
                            let info = Box::new(JitInvokeInfo {
                                class_name: class_ref,
                                method_name: method_ref,
                                descriptor: desc_ref,
                                num_jit_args,
                                return_type: ret_type,
                                invoke_kind,
                                declaring_class_id: cached.declaring_class_id.as_u32(),
                                owner_class_id,
                            });
                            let info_ptr: *const JitInvokeInfo = &*info;
                            owned_invoke_infos.push(info);
                            invoke_info.push((pc, info_ptr));
                            // Keeps the callee mapped, and puts this method in
                            // its invalidation closure.
                            direct_callee_entries.push((t.entry, jit_entry_artifact_id(t.entry)));
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry: t.entry,
                                    needs_context: t.needs_ctx,
                                    num_params,
                                    return_type: ret_type,
                                    guard_class_id: guard,
                                },
                            ));
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] sp-sync-direct-call {class_name}.{method_name}{descriptor} @pc={pc} entry={:#x} receiver-monitor opcode={opcode:#04x} exact_class_id={}",
                                    t.entry,
                                    guard & !JIT_SYNC_DIRECT_INSTANCE_TAG
                                );
                            }
                            continue;
                        }
                    }
                    needs_heap = true;

                    // Call-site intrinsics — inline machine code, no call overhead.
                    // The matcher (`try_resolve_intrinsic`) keys on
                    // (class, name, descriptor) and applies the same CPU-feature
                    // gates the x64 codegen ladder relies on. This invokestatic/
                    // invokespecial path never resolves a CRC32 intrinsic (those
                    // are `invokevirtual` only), so `guard_class_id` is 0.
                    if let Some((entry, num_params, ret)) =
                        try_resolve_intrinsic(&class_name, &method_name, &descriptor)
                    {
                        // `ArraycopyPrimitive`'s speculative fast path bails
                        // (null/non-array/reference-element/mismatched-kind/
                        // out-of-bounds) via a deopt trap whose ONLY
                        // historical resume strategy is a whole-method
                        // re-run — safe only when nothing observable
                        // happened before this call, an invariant this scan
                        // cannot verify and the JDT `Parser` stack-corruption
                        // bug violates (jasper-jdt-parser-arrayindexoutofbounds.md: a
                        // `stack[ptr--]` decrement already committed earlier
                        // in the same method gets re-executed on re-run).
                        // Register an ordinary `JitInvokeInfo` dispatch
                        // fallback for this same pc — identical to what a
                        // non-intrinsic `invokestatic` site gets below — so
                        // the codegen can route EVERY guard failure through
                        // a normal native-dispatch CALL (which throws or
                        // succeeds exactly like the interpreter's native
                        // registry, with ordinary exception propagation)
                        // instead of a deopt trap. No re-run, so no
                        // double-executed side effect.
                        //
                        // The `Arrays.sort(prim[])` / `Arrays.fill(prim[], v)`
                        // intrinsics take the same row for their NULL-array
                        // edge, which dispatches the real JDK method so its
                        // body raises HotSpot's NPE (JIT round 20, lane
                        // huntIntr; `emit_arrays_intrinsic_null_dispatch`).
                        //
                        // Compared by sentinel, NOT through
                        // `JitIntrinsic::from_entry`: that classifies the
                        // CRC32 family only and answers `None` for these, so
                        // the first version of this row never registered.
                        let arrays_null_dispatch = [
                            JitIntrinsic::ArraysSortInt,
                            JitIntrinsic::ArraysSortLong,
                            JitIntrinsic::ArraysSortChar,
                            JitIntrinsic::ArraysSortShort,
                            JitIntrinsic::ArraysSortByte,
                            JitIntrinsic::ArraysFill1,
                            JitIntrinsic::ArraysFill2,
                            JitIntrinsic::ArraysFill4,
                            JitIntrinsic::ArraysFill8,
                            JitIntrinsic::ArraysFillRange1,
                            JitIntrinsic::ArraysFillRange2,
                            JitIntrinsic::ArraysFillRange4,
                            JitIntrinsic::ArraysFillRange8,
                        ]
                        .iter()
                        .any(|i| i.as_entry() == entry);
                        if entry == JitIntrinsic::ArraycopyPrimitive.as_entry()
                            || arrays_null_dispatch
                        {
                            let class_ref = intern_jit_invoke_str(&class_name);
                            let method_ref = intern_jit_invoke_str(&method_name);
                            let desc_ref = intern_jit_invoke_str(&descriptor);
                            let info = Box::new(JitInvokeInfo {
                                class_name: class_ref,
                                method_name: method_ref,
                                descriptor: desc_ref,
                                num_jit_args: num_params,
                                return_type: ret,
                                invoke_kind,
                                declaring_class_id: cached.declaring_class_id.as_u32(),
                                owner_class_id,
                            });
                            let info_ptr: *const JitInvokeInfo = &*info;
                            owned_invoke_infos.push(info);
                            invoke_info.push((pc, info_ptr));
                        }
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                } else if direct_jit_callee_calls_enabled
                    // Same static-binding restriction as the bind above:
                    // a bailed-inline virtual site must fall through to
                    // the class-id-guarded MIC/PIC, not to an unguarded
                    // raw CALL into the statically resolved body.
                    && matches!(invoke_kind, 1 | 3)
                {
                    // INLINE-BAIL FALLBACK (tomcat doc 04, 2026-07-27).
                    //
                    // A site planned for inlining used to get NO direct call,
                    // because the whole direct-call ladder sits inside
                    // `!planned_inline`. The codegen tries `try_emit_inline`
                    // first and ROLLS BACK on any unsupported bytecode
                    // (`x64.rs` 0xb8 arm), and its fall-through order is
                    // direct_calls → invoke_info — so a bailed inline landed on
                    // the generic `jit_invoke_dispatch` helper, the SLOWEST of
                    // the three options, for the life of the compiled body.
                    //
                    // That is not a corner case: `try_jit_compile_callee_slow`
                    // (the tiered BACKGROUND worker, which compiles nearly
                    // every hot method) always passes an inline resolver, while
                    // the mutator path passes none (it gated one on the
                    // default-OFF `CRATONVM_JIT_MAIN_INLINE`, deleted in round
                    // 14 wave 2). So background-compiled bodies
                    // planned inlining for every small static/special callee
                    // and, whenever codegen declined it, paid ~196 ns per call
                    // instead of the ~5 ns a raw CALL costs (measured with
                    // `apps/tomcat-suite-runner/probes/CallCostProbe.java`,
                    // same binary, `CRATONVM_BG_COMPILE=0` as the control).
                    //
                    // Planning a direct call ALONGSIDE the inline site costs
                    // nothing when the inline succeeds (codegen checks
                    // `inline_sites` first and `continue`s), and turns the bail
                    // into a raw CALL instead of a helper round trip. No
                    // `continue` here, so the RBC.3 `JitInvokeInfo` fallback is
                    // still registered below as the last resort.
                    if let Some(compiler) = callee_compiler.as_ref() {
                        if let Some((entry, callee_needs_ctx)) =
                            compiler(&class_name, &method_name, &descriptor)
                        {
                            if verdicts.direct_call_requires_dispatch_of(
                                cycle_class,
                                &method_name,
                                &descriptor,
                                site_class_id,
                            ) {
                                needs_heap = true;
                                verdicts.mark_current_compile_method_recursive_cycle();
                            } else {
                                if callee_needs_ctx {
                                    needs_heap = true;
                                }
                                note_direct_bind(
                                    &format!(
                                        "{}.{}{}",
                                        cached.class_name,
                                        cached.method_name,
                                        cached.method_descriptor
                                    ),
                                    pc,
                                    &class_name,
                                    &method_name,
                                    &descriptor,
                                    entry,
                                );
                                direct_callee_entries.push((entry, jit_entry_artifact_id(entry)));
                                // Round 9 wave 9 (`evict9`): see the bind above.
                                direct_calls.push((
                                    pc,
                                    retirable_direct_call(
                                        code,
                                        code_len,
                                        pc,
                                        invoke_kind,
                                        entry,
                                        callee_needs_ctx,
                                        num_params,
                                        ret_type,
                                        &class_name,
                                        &method_name,
                                        &descriptor,
                                        &mut owned_cycle_cells,
                                    ),
                                ));
                            }
                        }
                    }
                } // end !planned_inline (RBC.3)
            }

            // Call-site intrinsics for instance-method invokes
            // (invokevirtual / invokeinterface). The static/special path
            // above already runs the matcher; this covers `invoke_kind`
            // 0 and 2. `num_params` from the matcher excludes the receiver;
            // the x64 codegen ladder for 0xb6/b7/b9 adds it back
            // (`callee_params + 1`).
            //
            // Unlike a `final` class, the CRC32/CRC32C intrinsics target a
            // class (`java.util.zip.CRC32` is *not* final) that could in
            // principle be subclassed, so a virtual `update` site is not
            // statically monomorphic. The codegen therefore emits a runtime
            // receiver class-id guard; the constant it compares against is
            // resolved here via `cp_invoke_class_id_resolver` (the declared
            // class of the call site) and stored in `guard_class_id`. When
            // the resolver is absent or cannot resolve the class id,
            // `guard_class_id` stays 0 and the CRC32 codegen bails the site
            // to normal dispatch (0 is never a real class id).
            if !is_recursive_call && (invoke_kind == 0 || invoke_kind == 2) {
                // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
                // `Long.longValue()` / `Integer.intValue()` emitted INLINE,
                // and it MUST be asked before the two thin direct binds
                // immediately below, which recognise the same two triples and
                // `continue`.
                //
                // MEASURED, because this ordering is not cosmetic: with the
                // BOX_UNBOX block left where it naturally belonged — beside the
                // other intrinsic families, ~600 lines further down — the
                // engagement print was EMPTY on a probe that boxes a counter
                // 3.2 million times. The thin binds swallowed every site first,
                // the intrinsic never fired, and both arms of the correctness
                // probe still passed, which is precisely the vacuous green the
                // site counters beside this exist to expose.
                //
                // The binds are not removed: they remain the fallback for a
                // site whose constant-pool class id does not resolve, where
                // this matcher declines (`AtomicLongFieldLayout::new` refuses
                // id 0) and a CALL is still better than generic dispatch.
                if let Some((entry, num_params, ret, guard_class_id)) = cp_invoke_class_id_resolver
                    .and_then(|r| r(cp_idx))
                    .and_then(|cid| {
                        try_resolve_box_unbox_intrinsic(&class_name, &method_name, &descriptor, cid)
                    })
                    // JIT round 23 wave 5 (lane L3,
                    // `CRATONVM_JIT_SP_INTRINSIC_TRAP_DESPEC`): every miss of
                    // this inline unbox (a null receiver) is a reason-6 trap.
                    // A site the de-spec registry already withdrew falls
                    // through to the thin direct bind below, whose helper
                    // raises the NPE itself, instead of trapping again.
                    .filter(|_| {
                        !sp_intrinsic_trap_site_withdrawn(
                            despec,
                            cached,
                            pc,
                            &class_name,
                            &method_name,
                            &descriptor,
                        )
                    })
                {
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
                // ===== INTRINSIC REGION END: BOX_UNBOX =====

                // `Integer.intValue()` thin direct call (see
                // `INTEGER_INT_VALUE_DIRECT_FN`): `Integer` is `final`, so a
                // site declared against it is statically monomorphic — the
                // plain guard-free virtual direct-call path is sound, and
                // the helper handles the null-receiver NPE itself.
                if direct_jit_callee_calls_enabled
                    && int_value_direct_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/Integer"
                    && method_name == "intValue"
                    && descriptor == "()I"
                {
                    // JDK-ONLY-WAVE2: see the marker on the
                    // `StringLatin1.toLowerCase` bind above — same list.
                    let entry = direct_native_helper(
                        direct_helpers.integer_int_value,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        note_integer_int_value_direct_site();
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'I',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `byte[].clone()` thin direct call (strings round 2, `str17`; see
                // `byte_array_clone_helper`, which the optimizing door asks too).
                if direct_jit_callee_calls_enabled && invoke_kind == 0 {
                    let entry = byte_array_clone_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `super.clone()` to `Object.clone`, thin direct call (see
                // `object_clone_helper`, which the optimizing doors ask too).
                if direct_jit_callee_calls_enabled && invoke_kind == 1 {
                    let entry = object_clone_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // j27 wave 3 (lane CLONE): a virtual `clone()`, thin direct call
                // (see `clone_thin_bind`, which the optimizing doors ask too). No
                // guard: the helper screens the receiver itself and makes the
                // site's virtual call for every receiver it does not serve.
                if direct_jit_callee_calls_enabled && invoke_kind == 0 {
                    let entry = clone_thin_bind::virtual_clone_helper_at(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        profile,
                        pc,
                        (
                            class_name.as_str(),
                            method_name.as_str(),
                            descriptor.as_str(),
                        ),
                        "single-pass",
                    );
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `ThreadLocal.get()` thin direct call (SBR round 2, wave 8, lane
                // `tlocal`; see `DirectHelperTable::thread_local_get`). Not final,
                // so no guard is registered: the helper screens the receiver's
                // class itself (an overriding `get`, `InheritableThreadLocal`,
                // `TerminatingThreadLocal`) and declines everything it does not
                // positively recognise to the generic dispatch. `--real-jdk` only:
                // the real-field `ThreadLocal` natives it fronts are not
                // registered under `--compatible`.
                if direct_jit_callee_calls_enabled
                    && thread_local_get_direct_enabled()
                    && invoke_kind == 0
                    && (class_name == "java/lang/ThreadLocal"
                        || class_name == "java/lang/InheritableThreadLocal")
                    && method_name == "get"
                    && descriptor == "()Ljava/lang/Object;"
                    // j27 wave 1 (lane TYPECHK, `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE`).
                    && !thread_local_thin_bind::thread_local_thin_bind_refused(
                        direct_helpers,
                        profile,
                        pc,
                        false,
                        "single-pass",
                    )
                {
                    let entry = direct_native_helper(
                        direct_helpers.thread_local_get,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        note_thread_local_get_direct_site();
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `ThreadLocal.set(Object)` thin direct call, the twin of the `get()` bind
                // above (SBR round 2, wave 8, lane `tlocal`): a found-key overwrite of the
                // real table's entry, everything else declined to the generic dispatch.
                if direct_jit_callee_calls_enabled
                    && jdk_only
                    && thread_local_get_direct_enabled()
                    && invoke_kind == 0
                    && (class_name == "java/lang/ThreadLocal"
                        || class_name == "java/lang/InheritableThreadLocal")
                    && method_name == "set"
                    && descriptor == "(Ljava/lang/Object;)V"
                    && !thread_local_thin_bind::thread_local_thin_bind_refused(
                        direct_helpers,
                        profile,
                        pc,
                        true,
                        "single-pass",
                    )
                {
                    let entry = direct_native_helper(
                        direct_helpers.thread_local_set,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        note_thread_local_get_direct_site();
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled
                    && varhandle_write_direct_helpers_enabled()
                    && direct_helpers.static_base_resolver_ctx != 0
                    && invoke_kind == 0
                    && class_name == "java/lang/invoke/VarHandle"
                {
                    if let Some(slot) =
                        varhandle_byte_buffer_view_write_slot(&method_name, &descriptor)
                    {
                        let entry = direct_native_helper(
                            direct_helpers.varhandle_byte_buffer_view_write[slot],
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            "([Ljava/lang/Object;)V",
                        );
                        if entry != 0 {
                            needs_heap = true;
                            let row = single_pass_varhandle_row(
                                entry,
                                direct_helpers.static_base_resolver_ctx,
                                3,
                                b'V',
                                (
                                    class_name.as_str(),
                                    method_name.as_str(),
                                    descriptor.as_str(),
                                ),
                                num_jit_args,
                                ret_type,
                                invoke_kind,
                                cached.declaring_class_id.as_u32(),
                                owner_class_id,
                                &mut owned_vh_judged_sites,
                                &mut inlined_methods,
                            );
                            direct_calls.push((pc, row));
                            continue;
                        }
                    }
                }
                // A plain byte[] view read: [handle, byte[], int] is a
                // distinct four-word C ABI, with exact native fallback.
                if direct_jit_callee_calls_enabled
                    && varhandle_read_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/invoke/VarHandle"
                {
                    if let Some(slot) =
                        varhandle_byte_array_view_read_slot(&method_name, &descriptor)
                    {
                        let entry = direct_native_helper(
                            direct_helpers.varhandle_byte_array_view_read[slot],
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            "([Ljava/lang/Object;)Ljava/lang/Object;",
                        );
                        if entry != 0 {
                            VARHANDLE_READ_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            let row = single_pass_varhandle_row(
                                entry,
                                direct_helpers.static_base_resolver_ctx,
                                2,
                                VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot],
                                (
                                    class_name.as_str(),
                                    method_name.as_str(),
                                    descriptor.as_str(),
                                ),
                                num_jit_args,
                                ret_type,
                                invoke_kind,
                                cached.declaring_class_id.as_u32(),
                                owner_class_id,
                                &mut owned_vh_judged_sites,
                                &mut inlined_methods,
                            );
                            direct_calls.push((pc, row));
                            continue;
                        }
                    }
                }
                // `VarHandle.get`/`getVolatile`/`getOpaque`/`getAcquire` on an
                // instance field with a primitive return (see
                // `VARHANDLE_READ_DIRECT_FNS`). `java/lang/invoke/VarHandle` is
                // NOT final and the site is signature-polymorphic, so this is
                // not a monomorphism argument like `Integer.intValue`'s above —
                // it does not need to be. Every VarHandle read mode resolves to
                // ONE registered native (`varhandle_get`) regardless of the
                // receiver's concrete handle class, so there is no subclass
                // whose override a guard would have to protect; and the helper
                // re-validates the handle at runtime, declining to the generic
                // dispatcher for anything the side table does not describe as a
                // resolved instance field of the site's own return kind.
                //
                // `num_params: 1` — the single reference coordinate.
                // `needs_context: true` puts `vm_ptr` in ARG_REGS[0], so the
                // helper sees `(vm_ptr, varhandle, receiver)`.
                if direct_jit_callee_calls_enabled
                    && varhandle_read_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/invoke/VarHandle"
                {
                    if let Some(slot) = varhandle_read_helper_slot(&method_name, &descriptor) {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        // The descriptor handed to the policy check is the
                        // native's REGISTERED (erased) one, not the call site's:
                        // `register_varhandle_natives` files every read mode
                        // under `([Ljava/lang/Object;)Ljava/lang/Object;`, and
                        // asking the registry about the site's own descriptor
                        // would find nothing and refuse for the wrong reason.
                        // All four modes are `NativeKind::Bridge`, so under
                        // `JdkOnly` this bind is refused — deliberately, and on
                        // the same terms as `HashMap.get`.
                        let entry = direct_native_helper(
                            direct_helpers.varhandle_read[slot],
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            "([Ljava/lang/Object;)Ljava/lang/Object;",
                        );
                        if entry != 0 {
                            VARHANDLE_READ_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            // Round 15 wave 6 (lane vh8): judged, or the
                            // wave-3 unjudged row withdrawn at the VM's first
                            // exact mint. See `single_pass_varhandle_row`.
                            let row = single_pass_varhandle_row(
                                entry,
                                direct_helpers.static_base_resolver_ctx,
                                1,
                                varhandle_read_slot_return(slot),
                                (
                                    class_name.as_str(),
                                    method_name.as_str(),
                                    descriptor.as_str(),
                                ),
                                num_jit_args,
                                ret_type,
                                invoke_kind,
                                cached.declaring_class_id.as_u32(),
                                owner_class_id,
                                &mut owned_vh_judged_sites,
                                &mut inlined_methods,
                            );
                            direct_calls.push((pc, row));
                            continue;
                        }
                    }
                }
                // `VarHandle.set`/`setVolatile`/`setRelease`/`setOpaque` on an
                // instance field (see `VARHANDLE_WRITE_DIRECT_FNS`). The same
                // argument as the read bind directly above, and it carries over
                // unchanged: every write mode resolves to a registered
                // `VarHandle` native (`varhandle_set`, or `setVolatile`'s fenced
                // `varhandle_set_volatile`) regardless of the receiver's concrete
                // handle class, so there is no subclass override a guard would
                // have to protect, and the helper re-validates the handle — and
                // the native's final-field, receiver-class and value-type
                // refusals — at runtime.
                //
                // What is DIFFERENT from the read bind is the value kinds. The
                // read table refuses `L`/`[` because a reference RETURN needs a
                // handoff root and the direct arm has no thread borrow to
                // publish one with. A write returns nothing: the reference
                // travels inward in a register the compiled caller's frame
                // already describes, so references are served here.
                //
                // `num_params: 2` — the coordinate and the value.
                // `needs_context: true` puts `vm_ptr` in ARG_REGS[0], so the
                // helper sees `(vm_ptr, varhandle, receiver, value)`. That is
                // four words, which is exactly Windows' ARG_REGS; it is also
                // why `compareAndSet` (five) is not bound here.
                if direct_jit_callee_calls_enabled
                    && varhandle_write_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/invoke/VarHandle"
                {
                    if let Some(slot) = varhandle_write_helper_slot(&method_name, &descriptor) {
                        // The descriptor handed to the policy check is the
                        // native's REGISTERED (erased) one, not the call site's
                        // — `register_varhandle_natives` files `set` under
                        // `([Ljava/lang/Object;)V`, and asking the registry
                        // about the site's own descriptor would find nothing
                        // and refuse for the wrong reason. It is
                        // `NativeKind::Bridge`, so under `JdkOnly` this bind is
                        // refused, deliberately and on the same terms as the
                        // read bind.
                        let entry = direct_native_helper(
                            direct_helpers.varhandle_write[slot],
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            "([Ljava/lang/Object;)V",
                        );
                        if entry != 0 {
                            VARHANDLE_WRITE_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            // Round 15 wave 6 (lane vh8): see the read arm.
                            let row = single_pass_varhandle_row(
                                entry,
                                direct_helpers.static_base_resolver_ctx,
                                2,
                                b'V',
                                (
                                    class_name.as_str(),
                                    method_name.as_str(),
                                    descriptor.as_str(),
                                ),
                                num_jit_args,
                                ret_type,
                                invoke_kind,
                                cached.declaring_class_id.as_u32(),
                                owner_class_id,
                                &mut owned_vh_judged_sites,
                                &mut inlined_methods,
                            );
                            direct_calls.push((pc, row));
                            continue;
                        }
                    }
                }
                // `VarHandle.compareAndSet` on an instance field (see
                // `VARHANDLE_CAS_DIRECT_FNS`). The write bind's argument
                // carries over: `compareAndSet` resolves to ONE registered
                // native regardless of the receiver's concrete handle class, so
                // there is no subclass override a guard would protect, and the
                // helper re-validates the handle at runtime.
                //
                // `num_params: 3` — the coordinate, `expected` and `new`. With
                // `needs_context: true` that is five words. On SysV they are
                // five of six `ARG_REGS`; on Win64 the fifth goes on the stack,
                // which `emit_stack_arg_setup` has marshalled for this arm since
                // Round-8 wave-3. The comment above `VARHANDLE_WRITE_DIRECT_FNS`
                // said that setup did not exist and had been wrong for as long
                // as anyone had read it.
                if direct_jit_callee_calls_enabled
                    && varhandle_cas_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/invoke/VarHandle"
                {
                    if let Some(slot) = varhandle_cas_helper_slot(&method_name, &descriptor) {
                        // The REGISTERED (erased) descriptor, not the site's —
                        // `register_varhandle_natives` files `compareAndSet`
                        // under `([Ljava/lang/Object;)Z`, and asking the
                        // registry about the site's own descriptor would find
                        // nothing and refuse for the wrong reason. It is
                        // `NativeKind::Bridge`, so under `JdkOnly` this bind is
                        // refused on the same terms as the read and write ones.
                        let entry = direct_native_helper(
                            direct_helpers.varhandle_cas[slot],
                            jdk_only,
                            direct_bind_admission,
                            &class_name,
                            &method_name,
                            "([Ljava/lang/Object;)Z",
                        );
                        if entry != 0 {
                            VARHANDLE_CAS_SITES_SINGLEPASS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            needs_heap = true;
                            // Round 15 wave 6 (lane vh8): see the read arm.
                            let row = single_pass_varhandle_row(
                                entry,
                                direct_helpers.static_base_resolver_ctx,
                                3,
                                b'Z',
                                (
                                    class_name.as_str(),
                                    method_name.as_str(),
                                    descriptor.as_str(),
                                ),
                                num_jit_args,
                                ret_type,
                                invoke_kind,
                                cached.declaring_class_id.as_u32(),
                                owner_class_id,
                                &mut owned_vh_judged_sites,
                                &mut inlined_methods,
                            );
                            direct_calls.push((pc, row));
                            continue;
                        }
                    }
                }
                // `Long.longValue()` — the twin of the `Integer.intValue()`
                // bind directly above. `java/lang/Long` is `final` on exactly
                // the same terms, so a site whose constant-pool class is
                // `java/lang/Long` is statically monomorphic and the guard-free
                // virtual direct-call path is sound; the helper handles the
                // null-receiver NPE itself and declines any receiver that fails
                // heap validation to the generic dispatcher.
                if direct_jit_callee_calls_enabled
                    && long_box_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/lang/Long"
                    && method_name == "longValue"
                    && descriptor == "()J"
                {
                    // JDK-ONLY-WAVE2: see the marker on the
                    // `StringLatin1.toLowerCase` bind above — same list.
                    let entry = direct_native_helper(
                        direct_helpers.long_long_value,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        LONG_LONG_VALUE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // ...and the SECOND counter, which this arm has never
                        // fed. The `Integer.intValue` twin two hundred lines
                        // above calls `note_integer_int_value_direct_site()`
                        // here; this arm bumped only `LONG_LONG_VALUE_SITES` and
                        // left `LONG_LONG_VALUE_DIRECT_SITES` at zero.
                        //
                        // Two counters for one bind is not duplication: they
                        // feed two different reports that ask different
                        // questions. `LONG_LONG_VALUE_SITES` is the `longValue`
                        // half of `long_box_direct_helper_sites()`, printed by
                        // `tiered.rs` beside `Long.valueOf` — the BOXING pair.
                        // `LONG_LONG_VALUE_DIRECT_SITES` is read by
                        // `long_long_value_direct_sites()` and printed by
                        // `vm::runtime::interp_census` under
                        // `CRATONVM_DBG_DIRECT_BINDS=1` as
                        // `[direct-binds] Long.longValue: sites_bound=N
                        // served=N declined_to_dispatch=N` — the BIND-vs-
                        // EXECUTION triple, beside the identically-shaped
                        // `Integer.intValue` line.
                        //
                        // That second line was self-contradicting, and this is
                        // the whole bug. `served`/`declined` come from
                        // `interp_census::LONG_VALUE_DIRECT`, which
                        // `jit_long_long_value_direct_body` in
                        // `vm/src/jit/helpers.rs` does feed. So a workload that
                        // bound and ran a `longValue` site printed
                        // `sites_bound=0 served=N` with N > 0: calls served by a
                        // bind the same line says was never made. A reader
                        // comparing it against the correct `Integer.intValue`
                        // line above concludes the `longValue` RECOGNITION is
                        // broken, when what is broken is the instrument
                        // (`docs/feature-designs/jit-ir-optimizations.md`).
                        //
                        // COMPLETE as of 2026-09-22: the OSR door at
                        // `vm/src/runtime/interpreter/jit_bridge.rs`, which
                        // also bumps `LONG_LONG_VALUE_SITES` for this same
                        // bind, now makes the same call beside it. Both doors
                        // feed both counters, so `sites_bound` and the
                        // `tiered.rs` figure for the same quantity agree —
                        // measured with a differential build, which read
                        // `sites_bound=1` against a tiered `2` without that
                        // call and `2` with it. Anyone adding a third door
                        // must bump BOTH: they feed two reports that ask
                        // different questions.
                        note_long_long_value_direct_site();
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                // `b'J'`, and the emitter's return-value ladder
                                // already handles it: it special-cases only
                                // `D`/`F` (XMM) and `L`/`[` (oop marking), and
                                // everything else — `I` and `J` alike — is one
                                // `push_from_rax`. The JIT's operand stack is
                                // one 64-bit slot per value, not JVMS
                                // category-2 pairs.
                                return_type: b'J',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `ByteBuffer.put(int,byte)` / `ByteBuffer.get(int)` — the
                // per-BYTE element accessors. See
                // `NIO_BYTEBUFFER_PUT_BYTE_DIRECT_FN` for the census that
                // named them and for why the receiver test lives in the
                // helper rather than in a `guard_class_id` here: the fast
                // path serves only a class the FUNNEL has already served
                // with the modelled layout, which is a fact this compile
                // cannot know.
                //
                // The receiver test lives in the helper, but the QUESTION OF
                // WHETHER TO BIND AT ALL is asked here, from this site's own
                // profile — see `nio_byte_element_bind_refused`. A site whose
                // receiver the helper cannot serve is better off unbound: the
                // inline cache takes it straight to the compiled
                // `HeapByteBuffer.get`, where a bind buys a crossing, a decline
                // and the funnel. This door needs the gate more than the
                // optimizing one does, not less: `osr_entered_optimizing=0` on
                // the heap probe says the hot loop runs in a SINGLE-PASS OSR
                // body, so a gate only at the other door leaves the measured
                // 190-vs-20 ns case exactly as it was.
                // `Buffer.session()` — the other half of the crossing census
                // that named the `byteElement` pair. See
                // `BUFFER_SESSION_DIRECT_FN`: one crossing per multi-byte heap
                // accessor, for a constant null. Matched on `(name,
                // descriptor)` because the method is `final` on
                // `java/nio/Buffer` and every site names it through the
                // caller's own class; the receiver screen that matters is the
                // helper's, at run time.
                if direct_jit_callee_calls_enabled
                    && buffer_session_direct_enabled()
                    && invoke_kind == 0
                    && is_buffer_session_site(&method_name, &descriptor)
                {
                    let entry = direct_helpers.buffer_session;
                    if entry != 0 {
                        BUFFER_SESSION_SITES_SP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // JIT round 17 wave 1, lane nio (NIO2-2): `Object.getClass()`
                // as one thin CALL instead of the MIC round trip plus the
                // native funnel. Opt-in. `Object.getClass` is an
                // `ACC_NATIVE` method, so there is no bytecode for the helper
                // to shadow; the bind still asks the same admission question
                // every thin helper asks under `--real-jdk`. The helper
                // screens the receiver itself (null -> NPE, anything it cannot
                // answer exactly -> the generic dispatcher).
                if direct_jit_callee_calls_enabled
                    && invoke_kind == 0
                    // JIT r20 (lane qMisc): resolved to `Object.getClass`, not
                    // a class's own private / static `getClass()` (this bind
                    // also carries the single-pass inline prefix).
                    && object_get_class_site_resolved(
                        &method_name,
                        &descriptor,
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
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] object-getclass-direct bind (sp) {class_name}.{method_name} @pc={pc}"
                            );
                        }
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled && matches!(invoke_kind, 0 | 1) {
                    let entry = object_notify_direct_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        &method_name,
                        &descriptor,
                        cp_invoke_declaring_class_resolver,
                        cp_idx,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // JIT round 22 wave 2, lane runtime (RUNTIME22-2): `Class.getName()`
                // as one thin CALL into the `getName()` memos. Opt-in. The
                // helper screens the receiver itself (null -> NPE, a miss ->
                // the generic dispatcher, which runs the registered native).
                if direct_jit_callee_calls_enabled
                    && matches!(invoke_kind, 0 | 1)
                    && is_class_get_superclass_site(&class_name, &method_name, &descriptor)
                {
                    let entry = direct_native_helper(
                        direct_helpers.class_get_superclass,
                        jdk_only,
                        direct_bind_admission,
                        "java/lang/Class",
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled
                    && matches!(invoke_kind, 0 | 1)
                    && is_class_get_name_site(&class_name, &method_name, &descriptor)
                    && class_get_name_direct_enabled()
                {
                    let entry = direct_native_helper(
                        direct_helpers.class_get_name,
                        jdk_only,
                        direct_bind_admission,
                        "java/lang/Class",
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] class-getname-direct bind (sp) {class_name}.{method_name} @pc={pc}"
                            );
                        }
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // JIT round 23 wave 4, lane L5: `String.matches` / `replaceAll`
                // / `replaceFirst` as one thin CALL into the registered
                // native's exact path. The helper screens the receiver itself
                // (null -> NPE, every decline -> the generic dispatcher).
                if direct_jit_callee_calls_enabled && matches!(invoke_kind, 0 | 1) {
                    if let Some((entry, num_params, return_type)) = string_regex_helper(
                        direct_helpers,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    ) {
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] string-regex-direct bind (sp) {class_name}.{method_name} @pc={pc}"
                            );
                        }
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params,
                                return_type,
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled
                    && scoped_memory_direct_enabled()
                    && invoke_kind == 0
                {
                    if let Some(op) = is_scoped_memory_op(&class_name, &method_name, &descriptor) {
                        let entry = op.helper_entry(direct_helpers);
                        // Round 16 wave 17 (lane mega5, NIO2-1).
                        if entry != 0
                            && scoped_memory_bind_admitted(
                                direct_bind_admission,
                                &class_name,
                                &method_name,
                                &descriptor,
                            )
                        {
                            SCOPED_MEMORY_SITES_SP
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: op.num_params(),
                                    return_type: op.return_type(),
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                }
                // Plain `Unsafe.get/put{Byte,Short,Int,Long}(Object,long)` --
                // see `UnsafeAccessorOp` for why no `direct_native_helper`
                // policy check applies.
                //
                // nio21 wave 3, lane L4: `Reference.refersTo0` is private, so
                // its site may arrive as `invokespecial` (kind 1) here; only
                // that op is admitted at kind 1, where the receiver is still
                // the first argument, as at the `ScopedMemoryAccess` kind-1 arm.
                if direct_jit_callee_calls_enabled && (invoke_kind == 0 || invoke_kind == 1) {
                    if let Some(op) = is_unsafe_accessor_op(&class_name, &method_name, &descriptor)
                        .filter(|op| op.direct_enabled() && (invoke_kind == 0 || op.is_refers_to()))
                    {
                        let entry = op.helper_entry(direct_helpers);
                        // Capability-classified (`RawMemory`) -- see the IR
                        // door's twin of this comment.
                        if entry != 0
                            && direct_bind_admission.map_or(true, |admits| {
                                admits(&class_name, &method_name, &descriptor)
                            })
                        {
                            op.note_site(0);
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: op.num_params(),
                                    return_type: op.return_type(),
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                }
                if direct_jit_callee_calls_enabled
                    && nio_byte_direct_helpers_enabled()
                    && invoke_kind == 0
                    && class_name == "java/nio/ByteBuffer"
                    && ((method_name == "put" && descriptor == "(IB)Ljava/nio/ByteBuffer;")
                        || (method_name == "get" && descriptor == "(I)B"))
                    && !nio_byte_element_bind_refused(
                        direct_helpers,
                        profile,
                        pc,
                        method_name == "put",
                        "single-pass",
                    )
                {
                    let is_put = method_name == "put";
                    let entry = direct_native_helper_for_impl(
                        if is_put {
                            direct_helpers.nio_bytebuffer_put_byte
                        } else {
                            direct_helpers.nio_bytebuffer_get_byte
                        },
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        "java/nio/DirectByteBuffer",
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        NIO_BYTE_ELEMENT_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: if is_put { 2 } else { 1 },
                                // `put` hands back the receiver, which the
                                // return-value ladder must mark as an oop;
                                // `get` returns a Java `byte`, which reaches
                                // the operand stack sign-extended into an int
                                // exactly as the registered native's
                                // `Value::Int` does.
                                return_type: if is_put { b'L' } else { b'I' },
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // `MessageDigest.update(byte)` — see
                // `MD_UPDATE_BYTE_DIRECT_FN`. `update` is FINAL on
                // `MessageDigest`, so the site is statically monomorphic in the
                // only sense that matters here: whatever the receiver's class,
                // this method is the one that runs. Which receivers the fast
                // path may SERVE is the helper's question, not this one's.
                if direct_jit_callee_calls_enabled
                    && md_update_direct_helper_enabled()
                    && invoke_kind == 0
                    && class_name == "java/security/MessageDigest"
                    && method_name == "update"
                    && descriptor == "(B)V"
                {
                    let entry = direct_native_helper(
                        direct_helpers.md_update_byte,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        &method_name,
                        &descriptor,
                    );
                    if entry != 0 {
                        MD_UPDATE_BYTE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled
                    && invoke_kind == 2
                    && class_name == "java/util/concurrent/ConcurrentMap"
                    && method_name == "get"
                    && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
                {
                    // JDK-ONLY-WAVE2: see the marker on the
                    // `StringLatin1.toLowerCase` bind above — same list.
                    // H7-1: the site names the INTERFACE; the helper runs
                    // `native_chm_get`, registered on `ConcurrentHashMap`.
                    // Both rows must be admitted — see
                    // `direct_native_helper_for_impl`.
                    let entry = direct_native_helper_for_impl(
                        direct_helpers.concurrent_hashmap_get,
                        jdk_only,
                        direct_bind_admission,
                        &class_name,
                        "java/util/concurrent/ConcurrentHashMap",
                        &method_name,
                        &descriptor,
                    );
                    // cjavac3: the `--compatible` `jdk-concurrent-hash-map` row.
                    let entry = if entry != 0
                        && compat_collection_bind_refused(
                            jdk_only,
                            direct_bind_admission,
                            "java/util/concurrent/ConcurrentHashMap",
                            &method_name,
                            &descriptor,
                        ) {
                        0
                    } else {
                        entry
                    };
                    if entry != 0 {
                        // H7-1 instrument — see `CONCURRENT_HASHMAP_GET_DIRECT_SITES`.
                        // The POLICY question above was asked about
                        // `java/util/concurrent/ConcurrentMap.get`, the
                        // INTERFACE named at the call site; the helper this
                        // binds runs `native_chm_get`, which is registered on
                        // `ConcurrentHashMap` as well. Both rows are `bridge`
                        // today (`scripts/baselines/jdk-only-kind-map-25-linux.tsv`),
                        // so the refusal is correct — but it is correct about
                        // the interface row, and a retag that moved only ONE of
                        // the two would separate them. See H7-1 §OUT-OF-FILE.
                        CONCURRENT_HASHMAP_GET_DIRECT_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }

                // The `java/lang/String.toLowerCase(Ljava/util/Locale;)` direct
                // bind stood here and is GONE. It was the THIRD copy of the
                // forced-native `java/lang/String` policy — `check_override`
                // forced this name, `force_native_over_real_jdk_bytecode`
                // refused it, and this bound it: three paths, three answers for
                // one method. The interpreter's two are deleted, so this one
                // goes with them rather than leaving compiled frames as the
                // only place a `String` native still shadows the bytecode.
                //
                // The perf it bought is not lost: the real
                // `String.toLowerCase(Locale)` bytecode delegates to
                // `StringLatin1.toLowerCase`, whose own direct bind above
                // survives. That one accelerates the real path instead of
                // replacing it, and its input is Latin-1 by construction, so
                // it cannot reach the surrogate cases that made the `String`
                // -level native diverge.
                // Exact-HashMap `put`/`get` thin direct calls (see
                // `HASHMAP_PUT_DIRECT_FN`): guard-free registration — the
                // helper verifies the receiver's exact class at runtime and
                // routes everything non-exact/non-overlay to the generic
                // dispatcher, so a subclass receiver keeps full virtual
                // semantics.
                if (invoke_kind == 0 && class_name == "java/util/HashMap")
                    || (invoke_kind == 2
                        && class_name == "java/util/Map"
                        && method_name == "get"
                        && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;")
                {
                    let recognized = if method_name == "put"
                        && descriptor == "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                    {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        // H7-1: `put` is only reached from the
                        // `invokevirtual java/util/HashMap` arm, so site class
                        // and implementing class coincide; routed through the
                        // same helper as `get` so the two stay one rule.
                        Some((
                            direct_native_helper_for_impl(
                                direct_helpers.hashmap_put,
                                jdk_only,
                                direct_bind_admission,
                                &class_name,
                                "java/util/HashMap",
                                &method_name,
                                &descriptor,
                            ),
                            2usize,
                        ))
                    } else if method_name == "get"
                        && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
                    {
                        // JDK-ONLY-WAVE2: see the marker on the
                        // `StringLatin1.toLowerCase` bind above — same list.
                        // H7-1: this arm also fires for
                        // `invokeinterface java/util/Map.get`, where the site
                        // class is the INTERFACE and the helper runs
                        // `native_hashmap_get_exact`, registered on
                        // `java/util/HashMap`. Both rows must be admitted.
                        Some((
                            direct_native_helper_for_impl(
                                direct_helpers.hashmap_get,
                                jdk_only,
                                direct_bind_admission,
                                &class_name,
                                "java/util/HashMap",
                                &method_name,
                                &descriptor,
                            ),
                            1usize,
                        ))
                    } else {
                        None
                    };
                    // cjavac3: the `--compatible` `jdk-hash-collections` row --
                    // asked about the IMPLEMENTING `java/util/HashMap` row the
                    // helper serves, whatever class the site names.
                    let recognized = recognized.map(|(entry, num_params)| {
                        let refused = entry != 0
                            && compat_collection_bind_refused(
                                jdk_only,
                                direct_bind_admission,
                                "java/util/HashMap",
                                &method_name,
                                &descriptor,
                            );
                        (if refused { 0 } else { entry }, num_params)
                    });
                    if let Some((entry, num_params)) = recognized {
                        if entry != 0 {
                            // H7-1 instrument — see `HASHMAP_GET_DIRECT_SITES`.
                            // Note which triple the POLICY question above was
                            // asked about: for the `invoke_kind == 2` arm it is
                            // `java/util/Map.get`, the INTERFACE, while the
                            // helper this binds runs `native_hashmap_get_exact`
                            // against a `java/util/HashMap` receiver. Both rows
                            // are `bridge` today, so today's refusal is right;
                            // it is right about the interface row. H7-1
                            // §OUT-OF-FILE asks for the two to be tied.
                            if num_params == 2 {
                                HASHMAP_PUT_DIRECT_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                HASHMAP_GET_DIRECT_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                }
                if class_name == "java/lang/reflect/Constructor"
                    && method_name == "newInstance"
                    && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
                    && direct_helpers.ctor_new_instance != 0
                {
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry: direct_helpers.ctor_new_instance,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
                // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
                // `MemorySegment.getAtIndex`/`setAtIndex` — the FFM ELEMENT
                // accessors, ~1158 ns/element through the ordinary dispatch
                // funnel against ~0.8 ns for a `short[]` element. See
                // `JitIntrinsic::FfmSegmentGetAtIndex`.
                //
                // Registered with its OWN `JitInvokeInfo` for this same pc,
                // because the emitted fast path DECLINES for every carrier it
                // does not positively recognise and the decline edge runs this
                // exact native dispatch — the same reason
                // `ArraycopyPrimitive` registers one on the static path. No
                // `guard_class_id`: the helper asks the native for a verdict
                // rather than speculating on a receiver class, so there is no
                // speculation here to guard or to de-speculate.
                if class_name == "java/lang/foreign/MemorySegment"
                    && ffm_element_accessor_name(&method_name)
                    && ffm_kind_for_descriptor(&descriptor).is_some()
                {
                    let entry = if ffm_element_accessor_is_get(&method_name) {
                        JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                    } else {
                        JitIntrinsic::FfmSegmentSetAtIndex.as_entry()
                    };
                    if false {
                        eprintln!(
                            "[ffm] REGISTERED {class_name}.{method_name}{descriptor} @pc={pc} kind={invoke_kind}"
                        );
                    }
                    let class_ref = intern_jit_invoke_str(&class_name);
                    let method_ref = intern_jit_invoke_str(&method_name);
                    let desc_ref = intern_jit_invoke_str(&descriptor);
                    let info = Box::new(JitInvokeInfo {
                        class_name: class_ref,
                        method_name: method_ref,
                        descriptor: desc_ref,
                        num_jit_args,
                        return_type: ret_type,
                        invoke_kind,
                        declaring_class_id: cached.declaring_class_id.as_u32(),
                        owner_class_id,
                    });
                    let info_ptr: *const JitInvokeInfo = &*info;
                    owned_invoke_infos.push(info);
                    invoke_info.push((pc, info_ptr));
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret_type,
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
                // ===== INTRINSIC REGION END: FFM_SEGMENT =====

                // First the layout-independent instance intrinsics.
                if let Some((entry, num_params, ret)) =
                    try_resolve_intrinsic(&class_name, &method_name, &descriptor)
                {
                    let guard_class_id = cp_invoke_class_id_resolver
                        .and_then(|r| r(cp_idx))
                        .unwrap_or(0);
                    // A CRC32/CRC32C intrinsic's inline code is only sound
                    // behind a receiver class-id guard. If the declared
                    // class id could not be resolved (`guard_class_id == 0`),
                    // do NOT register the intrinsic — leave the site to
                    // normal virtual dispatch (MIC/PIC). Registering it
                    // anyway would leave a `direct_calls` entry whose
                    // `entry` is an intrinsic sentinel the codegen could
                    // not safely emit.
                    if JitIntrinsic::from_entry(entry).is_some_and(|i| i.is_crc32_family())
                        && guard_class_id == 0
                    {
                        // Fall through — no `continue`, no direct_calls push.
                    } else {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                                guard_class_id,
                            },
                        ));
                        continue;
                    }
                }
                // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
                // `AtomicInteger` read-modify-write family. Registered only
                // with a resolved receiver class id: `AtomicInteger` is not
                // final and its methods are not final, so the inline field
                // access is sound only behind an exact class-id guard, and
                // without one the site must keep ordinary dispatch (which runs
                // any subclass override).
                //
                // A site naming a subclass (`Counter extends AtomicInteger`,
                // review #80) matches through the class that DECLARES the
                // resolved method, which `cp_invoke_declaring_class_resolver`
                // supplies, and only for a method `final` in the JDK. The guard
                // is the site's own class id. See
                // `atomic_intrinsic_for_invoke_site`.
                if let Some((entry, num_params, ret, guard_class_id)) =
                    atomic_intrinsic_for_invoke_site(
                        "java/util/concurrent/atomic/AtomicInteger",
                        cp_idx,
                        &class_name,
                        &method_name,
                        &descriptor,
                        cp_invoke_class_id_resolver,
                        cp_invoke_declaring_class_resolver,
                    )
                {
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
                // ===== INTRINSIC REGION END: ATOMIC_INT =====

                // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
                // The 64-bit twin, on exactly the same terms: registered only
                // with a resolved receiver class id, because `AtomicLong` is
                // not final and a subclass override must keep ordinary
                // dispatch. Subclass sites match through the declaring class,
                // as above.
                if let Some((entry, num_params, ret, guard_class_id)) =
                    atomic_intrinsic_for_invoke_site(
                        "java/util/concurrent/atomic/AtomicLong",
                        cp_idx,
                        &class_name,
                        &method_name,
                        &descriptor,
                        cp_invoke_class_id_resolver,
                        cp_invoke_declaring_class_resolver,
                    )
                {
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
                // ===== INTRINSIC REGION END: ATOMIC_LONG =====

                // `Object.hashCode()` / `CharSequence.hashCode()` on a receiver
                // the profile says is a `String` (round 9 wave 3). The same
                // inline `StringHashCode` body the `String`-typed site gets,
                // behind the String class-id guard the emitter's
                // STRING_ACCESS arm already emits for a non-zero
                // `guard_class_id`; a miss deopts, so admission needs positive
                // profile evidence (see `object_hashcode_string_guard`) and a
                // clean per-bci de-spec record for THIS speculation.
                if let Some(guard_class_id) = object_hashcode_string_guard(
                    &class_name,
                    &method_name,
                    &descriptor,
                    resolved_string_layout,
                    profile,
                    pc,
                ) {
                    let despecialised = despec.is_some_and(|registry| {
                        registry.contains_speculation(
                            &format!(
                                "{}.{}:{}",
                                cached.class_name, cached.method_name, cached.method_descriptor
                            ),
                            pc as u32,
                            crate::deopt::speculation_id(
                                pc as u32,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            ),
                        )
                    });
                    if despecialised {
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_DECLINED,
                        );
                    } else {
                        needs_heap = true;
                        OBJECT_HASHCODE_STRING_GUARD_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] object-hashcode STRING-GUARDED {class_name}.{method_name}{descriptor} @pc={pc} guard_class_id={guard_class_id}"
                            );
                        }
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry: JitIntrinsic::StringHashCode.as_entry(),
                                needs_context: false,
                                num_params: 0,
                                return_type: b'I',
                                guard_class_id,
                            },
                        ));
                        continue;
                    }
                }

                // Then the `java/lang/String` intrinsics — registered only
                // when the String field layout has resolved (and carries a
                // `coder` field). `string_layout` is resolved ONCE per
                // compilation; the same value is threaded into `x64::compile`
                // (`compiler.string_layout`), so the matcher's "registered"
                // decision and the codegen's "can emit inline" decision
                // never disagree — a String sentinel is never registered
                // for a site whose codegen would then bail to a raw `CALL`.
                if let Some((entry, num_params, ret, guard_class_id)) = try_resolve_string_intrinsic(
                    &class_name,
                    &method_name,
                    &descriptor,
                    resolved_string_layout,
                )
                // `String.indexOf(int)` is intrinsified ONLY for a needle this
                // compile proves is a constant in `0..=0xFFFF` (the range on
                // which the inline single-code-unit scan and the JDK's
                // code-point search are the same function). The screen used
                // to live only in the backend's INVOKESTATIC ladder, which an
                // instance method never reaches, so every variable-needle
                // site was registered here and got a runtime trap screen in
                // the emitter. Declining HERE leaves the site unregistered: it
                // falls through to `invoke_info` / its MIC like any other
                // invoke. See
                // `string-indexof-char-screen-belongs-at-the-resolver-20260918.md`.
                //
                // `pc` must also not be a branch target: the constant the
                // linear walk sees is the needle on the FALL-THROUGH path
                // only. javac compiles `s.indexOf(f ? ch : 'a')` as
                // `iload ch; goto L; bipush 97; L: invokevirtual`, whose
                // linear predecessor is the constant while the `goto` edge
                // carries the variable.
                .filter(|&(entry, _, _, _)| {
                    string_index_of_char_site_admitted(entry, code, code_len, pc)
                })
                // A `java/lang/CharSequence` site is intrinsified behind an
                // exact `String` class-id guard whose MISS IS A DEOPT. Ask the
                // interpreter's own receiver profile first: if this bci has
                // been reached thousands of times and hardly ever by a
                // `String`, the guard is not mis-tuned, it is wrong about the
                // program, and emitting it buys a deopt per call plus an
                // eventual whole-method blacklist. netty's
                // `HttpHeaderValidationUtil.validateValidHeaderValue` is that
                // site (`CharSequence.length()` at bci 1, reached only with
                // `AsciiString` and an anonymous `CharSequence`).
                .filter(|&(entry, _, _, guard_class_id)| {
                    // JIT round 23 wave 5 (lane L3,
                    // `CRATONVM_JIT_SP_INTRINSIC_TRAP_DESPEC`): the unguarded
                    // (`java/lang/String`-declared) members too. Their misses
                    // -- a null receiver, a `charAt` index out of range --
                    // are reason-6 traps, not guard failures, so the filter
                    // below (`guard_class_id == 0` -> admit) never asked the
                    // registry, and a site that throws on most calls trapped
                    // on every compile until the method's deopt budget retired
                    // it to the interpreter (`H8Msgs.k`, 40 traps at bci 160
                    // and 516, then `MakeNotCompilable`). Withdrawn, the site
                    // keeps its ordinary call, which throws as HotSpot's C1
                    // call does. The StringBuilder family declines to a call,
                    // never a trap, and is not asked.
                    if guard_class_id == 0
                        && !string_intrinsic_declines_to_a_call(entry)
                        && sp_intrinsic_trap_site_withdrawn(
                            despec,
                            cached,
                            pc,
                            &class_name,
                            &method_name,
                            &descriptor,
                        )
                    {
                        return false;
                    }
                    if guard_class_id == 0 || !receiver_despec_enabled() {
                        return true;
                    }
                    // The StringBuilder family carries a non-zero guard and is
                    // NOT what this filter is about. Its guard miss is a CALL
                    // to the ordinary dispatch, not a deopt — so a receiver
                    // that keeps missing costs a predicted branch, never a
                    // deopt-per-call or a whole-method blacklist, and there is
                    // nothing here for a receiver profile to protect against.
                    // (It also cannot miss in practice: `StringBuilder` is
                    // final, so the site's declared class IS the guard.)
                    if string_intrinsic_declines_to_a_call(entry) {
                        return true;
                    }
                    // Positive evidence admits the guard outright; the de-spec
                    // registry can still veto it below if this exact bci has
                    // already proven the profile wrong.
                    let supported = receiver_profile_supports_guard(profile, pc, guard_class_id)
                        || charseq_blind_guard_enabled();
                    // Two independent reasons to refuse the guard, in the order
                    // they become available. (1) the receiver profile already
                    // says the guarded class is a minority here -- free, but
                    // only populated under `CRATONVM_TIER_PGO`. (2) this bci is
                    // in the per-bci de-spec registry, i.e. it has ALREADY
                    // deopted `PER_BCI_DESPEC_LIMIT` times on this guard. (2) is
                    // the one that fires on an ordinary run, and it has to be
                    // asked HERE rather than in the backend: a site the resolver
                    // registers as an intrinsic never gets `invoke_info`
                    // (`direct_calls.push(..); continue;` happens above the
                    // `invoke_info.push`), so a backend that drops the direct
                    // call leaves the pc with no dispatch metadata at all and
                    // compiles an `UnreachedCode` trap there instead. Refusing
                    // at the resolver keeps the site on the ordinary path and
                    // it gets its MIC like any other invoke.
                    let by_profile = !supported
                        || receiver_profile_rejects_guard(profile, pc, guard_class_id);
                    // M4 — asked about THIS speculation, not about the
                    // bytecode index. The guard being decided here takes the
                    // `ReceiverTypeChanged` deopt edge, and a protected invoke
                    // publishes a `PendingException` frame at the SAME bci
                    // (`jit/src/osr_exit.rs` calls that the ordinary shape). On
                    // the old per-bci key, four traps on the exceptional frame
                    // withdrew this guard as well — on evidence about a
                    // speculation that is not this one. A wildcard entry
                    // (`SPECULATION_ID_ANY`, recorded when the VM could not
                    // recover which point trapped) still matches, so the
                    // undecidable case keeps the coarse behaviour.
                    let by_despec = despec.is_some_and(|registry| {
                        registry.contains_speculation(
                            &format!(
                                "{}.{}:{}",
                                cached.class_name, cached.method_name, cached.method_descriptor
                            ),
                            pc as u32,
                            crate::deopt::speculation_id(
                                pc as u32,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            ),
                        )
                    });
                    if by_profile {
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_PROFILE_DECLINED,
                        );
                    }
                    if by_despec {
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_DECLINED,
                        );
                    }
                    if (by_profile || by_despec)
                        && crate::x64::dbg_jitc_enabled()
                    {
                        eprintln!(
                            "[cratonvm-jitc] string-intrinsic DECLINED {class_name}.{method_name}{descriptor} @pc={pc} guard_class_id={guard_class_id} by_profile={by_profile} by_despec={by_despec}"
                        );
                    }
                    !(by_profile || by_despec)
                }) {
                    needs_heap = true;
                    // The StringBuilder family is registered with its OWN
                    // `JitInvokeInfo` for this same pc, for the reason the FFM
                    // region above gives: its emitted fast path DECLINES — a
                    // UTF16 payload, a full one, a character above LATIN1 — and
                    // the decline edge runs this exact native dispatch. Without
                    // it the pc has no dispatch metadata at all, because a site
                    // the resolver registers as an intrinsic never reaches the
                    // generic `invoke_info.push` below (this arm `continue`s
                    // above it). That is not a hypothetical: it is what this
                    // arm did on its first run, and the emitter's
                    // `sb-intrinsic-unemittable` refusal dropped every method
                    // containing a `StringBuilder.append(char)` to the
                    // interpreter — 404 696 interpreted dispatches against
                    // 8 695 before, and `append(char)` 309 ns -> 1 091 ns.
                    //
                    // The String/CharSequence members need none of this: every
                    // one of their edges is a deopt, which needs no dispatch
                    // metadata.
                    if string_intrinsic_declines_to_a_call(entry) {
                        let class_ref = intern_jit_invoke_str(&class_name);
                        let method_ref = intern_jit_invoke_str(&method_name);
                        let desc_ref = intern_jit_invoke_str(&descriptor);
                        let info = Box::new(JitInvokeInfo {
                            class_name: class_ref,
                            method_name: method_ref,
                            descriptor: desc_ref,
                            num_jit_args,
                            return_type: ret_type,
                            invoke_kind,
                            declaring_class_id: cached.declaring_class_id.as_u32(),
                            owner_class_id,
                        });
                        let info_ptr: *const JitInvokeInfo = &*info;
                        owned_invoke_infos.push(info);
                        invoke_info.push((pc, info_ptr));
                    }
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            // `java/lang/String` is `final` → monomorphic →
                            // `guard_class_id == 0` (no guard). A
                            // `java/lang/CharSequence` site carries the String
                            // class id so the codegen guards the receiver and
                            // deopts for any non-String CharSequence.
                            guard_class_id,
                        },
                    ));
                    continue;
                }
                // DIAGNOSTIC (2026-08-13): a String accessor site that did NOT
                // take the intrinsic is otherwise invisible — it just runs the
                // real bytecode at ~400 ns/call. Name the two reasons apart
                // (no resolved layout vs. an unmatched name/descriptor) so
                // "the intrinsic is inert" is answerable in one run.
                if (class_name == "java/lang/String" || class_name == "java/lang/CharSequence")
                    && crate::x64::dbg_jitc_enabled()
                {
                    eprintln!(
                        "[cratonvm-jitc] string-intrinsic MISSED {class_name}.{method_name}{descriptor} @pc={pc} layout={} coder={}",
                        resolved_string_layout.is_some(),
                        resolved_string_layout.map(|l| l.has_coder).unwrap_or(false)
                    );
                }
            } else if (class_name == "java/lang/String" || class_name == "java/lang/CharSequence")
                && crate::x64::dbg_jitc_enabled()
            {
                eprintln!(
                    "[cratonvm-jitc] string-intrinsic SKIPPED-GATE {class_name}.{method_name}{descriptor} @pc={pc} recursive={is_recursive_call} kind={invoke_kind}"
                );
            }

            // Keep only static tail self-calls on the raw backend path. Every
            // other recursive site gets dispatch metadata so the helper stack
            // guard runs before re-entering compiled code.
            if use_raw_tail_self_call {
                // Stage the callee info here TOO, even though the tail form
                // itself needs no callee-deopt service (it reuses this frame,
                // so there is no callee activation to resolve a handler in).
                //
                // The two tail predicates are not the same predicate.
                // `invokestatic_self_call_uses_tail_jump` is purely syntactic —
                // an `invokestatic` followed by an `xreturn` — while the
                // codegen additionally requires `self_tailcall_enabled()`
                // (default OFF since 2026-08-20), a non-branch-target return
                // and an unprotected pc. So a tail-SHAPED site reaches this
                // `continue` and is then emitted by the codegen as an ordinary
                // raw non-tail `CALL`, which does need the service. Staging on
                // the wider predicate makes the info available for whichever
                // form the backend actually picks; the tail form ignores it.
                //
                // A PROTECTED pc settles the disagreement outright: the codegen
                // refuses the tail form inside a handler's range (the tail form
                // tears this frame down, so a throw from the callee would bypass
                // the handler covering this pc), so such a site is certainly a
                // raw `CALL`. It therefore needs the `vm_ptr` frame slot the
                // service is called with, exactly as the non-tail arm below
                // requires — and a self-call covered by a handler is precisely
                // the shape this whole fix is about. Narrow on purpose: methods
                // with no handler over the site keep their current frame shape.
                if cached
                    .exception_table
                    .iter()
                    .any(|e| pc >= e.start_pc as usize && pc < e.end_pc as usize)
                {
                    needs_heap = true;
                }
                if self_call_invoke_info == 0 {
                    self_call_invoke_info = stage_self_call_invoke_info(
                        cached,
                        invoke_kind,
                        num_jit_args,
                        ret_type,
                        &mut owned_strings,
                        &mut owned_invoke_infos,
                    );
                }
                // t1 wave 3, lane X: the positive self-call marker (see
                // `static_self_call_marker`).
                if !direct_calls.iter().any(|(p, _)| *p == pc) {
                    direct_calls.push((pc, static_self_call_marker(num_params, ret_type)));
                }
                continue;
            }

            // bt18-regression fix (2026-07-18): NON-tail static self-recursion
            // may ALSO take the raw direct-CALL path — but only under the
            // caller-supplied identity proof (see `CompileRequest::self_call_identity_stable`:
            // builtin-loaded class whose name maps back to its own ClassId,
            // so the historical loader-identity hazard cannot arise) and
            // never for mutual-recursion cycle targets (the dispatch depth
            // guard is still their only stack protection). The x64 else-arm
            // emits the inline stack-floor check + `self_call_stack_guard`
            // helper before the direct CALL, so runaway recursion still
            // surfaces as a catchable StackOverflowError. `needs_heap` is
            // required: the guard is called with the vm_ptr frame slot.
            if invoke_kind == 3
                && is_same_method_recursive_call
                && !recursive_cycle_target
                && self_call_identity_stable
            {
                needs_heap = true;
                // The raw `CALL` still needs the callee-deopt service, and the
                // `continue` below is exactly what denies it an `invoke_info`
                // row to carry it. Stage the info on its own channel instead —
                // see `stage_self_call_invoke_info` for the miscompile.
                if self_call_invoke_info == 0 {
                    self_call_invoke_info = stage_self_call_invoke_info(
                        cached,
                        invoke_kind,
                        num_jit_args,
                        ret_type,
                        &mut owned_strings,
                        &mut owned_invoke_infos,
                    );
                }
                // t1 wave 3, lane X: the positive self-call marker (see
                // `static_self_call_marker`).
                if !direct_calls.iter().any(|(p, _)| *p == pc) {
                    direct_calls.push((pc, static_self_call_marker(num_params, ret_type)));
                }
                continue;
            }

            // Round 9 wave 10 (`call10`): the INSTANCE twin of the arm above.
            // A statically bound `invokevirtual` (javac 11+ emits it for a
            // private method; the owner pin above made it kind 1) or
            // `invokespecial` of this very method used to take the generic
            // tail below -- `jit_invoke_dispatch` on every recursion, ~150 ns
            // a call (`MutPriv self`: 2.8 s vs HotSpot's 25 ms). The row is a
            // marker ([`JIT_SELF_CALL_GUARD`]): no `JitInvokeInfo`, exactly
            // like the static self-call, and the codegen null-checks the
            // receiver before taking the self-recursive arm.
            // JIT round 19 wave 5 (lane sync5, SYNCDOOR4-2): a SYNCHRONIZED
            // instance method's self-call is planned the same way when its
            // compile may be a self-locking body (`sync_self_call_candidate`):
            // that body's entry locks local 0, so a CALL of it is exactly
            // JVMS 2.11.10's enter on the callee's receiver (recursive on the
            // same one). The marker is kept only by the self-locking run;
            // the wrapped retry and any run that does not self-lock dispatch
            // through the fallback info staged here
            // (`SinglePassTables::unbind_sync_self_calls`).
            let sync_self_call = sync_self_call_candidate
                && !recursive_cycle_target
                && is_same_method_recursive_call
                && sync_self_call_goes_direct(
                    cached,
                    opcode,
                    invoke_kind == 1,
                    &class_name,
                    &method_name,
                    &descriptor,
                    self_call_identity_stable,
                    helpers.self_call_stack_guard,
                );
            if sync_self_call {
                let fallback = stage_self_call_invoke_info(
                    cached,
                    invoke_kind,
                    num_jit_args,
                    ret_type,
                    &mut owned_strings,
                    &mut owned_invoke_infos,
                );
                // The site's own resolution, as the generic tail below
                // would have registered it (the owner pin's class id).
                if let Some(info) = owned_invoke_infos.last_mut() {
                    info.owner_class_id = owner_class_id;
                }
                // Cast: the arena address back to the pointer it was made from.
                sync_self_call_fallback.push((pc, fallback as *const JitInvokeInfo));
            }
            if sync_self_call
                || (!recursive_cycle_target
                    && is_same_method_recursive_call
                    && instance_self_call_goes_direct(
                        cached,
                        opcode,
                        invoke_kind == 1,
                        &class_name,
                        &method_name,
                        &descriptor,
                        self_call_identity_stable,
                        helpers.self_call_stack_guard,
                    ))
            {
                needs_heap = true;
                direct_calls.push((
                    pc,
                    JitDirectCall {
                        entry: 0,
                        needs_context: true,
                        num_params,
                        return_type: ret_type,
                        guard_class_id: JIT_SELF_CALL_GUARD,
                    },
                ));
                // The instance twin lowers through `walk_invokestatic`'s
                // self-recursive arm, so it needs the same callee-deopt service
                // and the same info. `invoke_kind` is the site's real kind (1),
                // not 3: the service reads arg 0 as the receiver for it.
                if self_call_invoke_info == 0 {
                    self_call_invoke_info = stage_self_call_invoke_info(
                        cached,
                        invoke_kind,
                        num_jit_args,
                        ret_type,
                        &mut owned_strings,
                        &mut owned_invoke_infos,
                    );
                }
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] instance-self-call {}.{}{} @pc={pc} opcode={opcode:#04x}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                }
                continue;
            }

            // BUG-1 companion — when the dedicated self-call stack guard is
            // Earlier revisions routed NON-tail static self-recursive sites
            // through a raw direct-CALL path with a self-call guard. It avoids
            // a dispatch round trip, but the target has no loader identity.
            // Mutual-recursion cycle targets
            // (`recursive_cycle_target`) keep the dispatch route — a direct
            // call into ANOTHER method's artifact is a different hazard the
            // guard does not cover.
            // Non-tail same-method candidates deliberately retain dispatch
            // metadata: a raw direct entry call has no loader identity and
            // can invoke a different same-named method recursively.

            // Trivial-constructor elision (callee/IR tier, via try_compile): an
            // elidable `invokespecial C.<init>()V` is emitted AS
            // `java/lang/Object.<init>` so the single-pass `0xb7` codegen elision
            // drops the per-object `jit_invoke_dispatch` — the same effect the
            // VM-side execute/OSR resolution gets, here for callees compiled
            // through this path. `cp_elidable_init_resolver` is the body-checking
            // predicate (`resolve_jit_elidable_init` → `is_elidable_construction`);
            // it resolves bootstrap/JDK targets via `find_class_by_name`, so
            // app-loaded targets are not yet covered on this path (follow-up).
            let elide_ctor = invoke_kind == 1
                && method_name == "<init>"
                && descriptor == "()V"
                && elide_trivial_ctor_enabled()
                && cp_elidable_init_resolver.map_or(false, |r| r(cp_idx));
            // The rewritten name is not the substituted owner's.
            let owner_class_id = if elide_ctor { 0 } else { owner_class_id };
            // nio21 wave 18, lane L4 (`CRATONVM_JIT_SP_BOUND_SITE_IC`): the
            // single-pass twin of the optimizing tier's bound-site inline
            // cache, for a statically bound instance site whose owner class is
            // `final` and which nothing above bound. See
            // [`sp_bound_site_ic_planned`].
            let sp_bound_site_ic = sp_bound_site_ic_planned(
                opcode,
                invoke_kind,
                &method_name,
                direct_calls.iter().any(|(p, _)| *p == pc)
                    || inline_sites.contains_key(&pc)
                    // JIT r22 (lane inline, `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR`):
                    // a constructor the codegen elides (a proven-empty `()V`,
                    // emitted as `Object.<init>`, or `Object.<init>` itself)
                    // has no call to cache.
                    || elide_ctor
                    || (method_name == "<init>" && class_name == "java/lang/Object"),
                || {
                    let cid = if owner_class_id != 0 {
                        Some(owner_class_id)
                    } else {
                        cp_invoke_class_id_resolver.and_then(|r| r(cp_idx))
                    };
                    cid.is_some_and(|cid| {
                        cid != 0
                            && req
                                .class_is_final_resolver
                                .is_some_and(|is_final| is_final(cid))
                    })
                },
            );
            if sp_bound_site_ic && crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] sp-bound-site-ic {}.{}{} @pc={pc} -> {class_name}.{method_name}{descriptor} \
                     (statically bound, {}, no direct entry at compile time)",
                    cached.class_name, cached.method_name, cached.method_descriptor,
                    if bound_pic_any_owner_enabled() { "bound pic, any owner" } else { "final owner" },
                );
            }
            // JIT round 20 wave 11 (lane pLate, W15L1-P1 / W18L4-3): an
            // `invokestatic` the direct bind could not give an entry (the
            // callee compiles after this caller) gets a late-bind
            // `JitCycleEdgeCell` on exactly the closing edge's gates, so
            // `JitCache::put` binds it once the callee publishes instead of
            // leaving it on `jit_invoke_dispatch` for the life of the body.
            // The `JitInvokeInfo` registered below is its dispatch fallback,
            // as for a closing edge. Only a site nothing above planned (no
            // direct row, no inline plan, not recursive).
            if late_static_cell_site(closes_active_compile_cycle, sp_late_static_declined)
                && !is_recursive_call
                && cycle_declaring.is_none()
                && invoke_kind == 3
                && opcode == 0xb8
                && !elide_ctor
                && !inline_sites.contains_key(&pc)
                && !direct_calls.iter().any(|(p, _)| *p == pc)
                && jit_cycle_edge_cell_enabled()
                && jit_cycle_direct_bind_enabled()
                && direct_jit_callee_calls_enabled
                && !crate::offload_hook::keeps_dispatch_helper(
                    &class_name,
                    &method_name,
                    &descriptor,
                )
                && !crate::offload_hook::is_reflect_ctor_dispatch(
                    &class_name,
                    &method_name,
                    &descriptor,
                )
                && !verdicts.direct_call_requires_dispatch_of(
                    cycle_class,
                    &method_name,
                    &descriptor,
                    site_class_id,
                )
                && helpers.self_call_stack_guard != 0
                && helpers.invoke_dispatch != 0
                && req.cycle_edge_admission.is_some_and(|admit| {
                    admit(
                        cached.declaring_class_id.as_u32(),
                        &class_name,
                        &method_name,
                        &descriptor,
                    )
                })
            {
                if let Some(target_class_id) = cp_invoke_class_id_resolver.and_then(|r| r(cp_idx)) {
                    let cell = JitCycleEdgeCell::new(
                        &class_name,
                        &method_name,
                        &descriptor,
                        cratonvm_types::ClassId::new(target_class_id),
                        true,
                    );
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry: Arc::as_ptr(&cell) as usize,
                            needs_context: true,
                            num_params,
                            return_type: ret_type,
                            guard_class_id: JIT_CYCLE_EDGE_CELL_GUARD,
                        },
                    ));
                    owned_cycle_cells.push(cell);
                    if crate::x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] late-static-cell {}.{}{} @pc={pc} -> {class_name}.{method_name}{descriptor} class_id={target_class_id}",
                            cached.class_name, cached.method_name, cached.method_descriptor
                        );
                    }
                }
            }
            let class_name = if elide_ctor {
                if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                {
                    let key = format!("{}.{}", cached.class_name, cached.method_name);
                    if key.contains(&want) {
                        eprintln!(
                            "[jit-elide-ctor] caller={key} pc={pc} rewrote {class_name}.<init>()V -> java/lang/Object.<init>()V"
                        );
                    }
                }
                "java/lang/Object".to_string()
            } else {
                class_name
            };

            let class_ref = intern_jit_invoke_str(&class_name);
            let method_ref = intern_jit_invoke_str(&method_name);
            let desc_ref = intern_jit_invoke_str(&descriptor);

            let info = Box::new(JitInvokeInfo {
                class_name: class_ref,
                method_name: method_ref,
                descriptor: desc_ref,
                num_jit_args,
                return_type: ret_type,
                invoke_kind,
                declaring_class_id: cached.declaring_class_id.as_u32(),
                owner_class_id,
            });
            let info_ptr: *const JitInvokeInfo = &*info;
            owned_invoke_infos.push(info);
            invoke_info.push((pc, info_ptr));

            // Virtual/interface caches do not bake the declared target and
            // therefore must not inherit the direct-call recursion deny set.
            // Each entry is installed later by `jit_invoke_virtual_mic` after
            // resolving the concrete receiver class under its own compile
            // policy. Suppressing the slot here made an abstract interface
            // signature that appeared in the active-cycle registry permanently
            // use blind `jit_invoke_dispatch`, including OSR bodies.
            //
            // A bound site (`sp_bound_site_ic`) is filled only by the VM's
            // bound-site arm of `jit_invoke_virtual_mic` (its `invoke_kind == 1`
            // route), never by virtual selection, and is never profile-seeded
            // (as the optimizing tier's bound sites): its owner is final, so
            // one receiver class reaches it and its PIC never overflows into
            // the megamorphic paths.
            if invoke_kind_uses_inline_cache(invoke_kind) || sp_bound_site_ic {
                // `new_at(pc)`, not `new()`: the artifact keeps only the boxes,
                // so the bci has to travel inside the slot or it is lost at
                // publication. See `JitMICSlot::bci`.
                let mic = Box::new(JitMICSlot::new_at(pc));
                let profile = profile.filter(|_| !sp_bound_site_ic);
                if let Some(prof) = profile {
                    if let Some(receiver_counts) = prof.receivers.get(&pc) {
                        if let Some(dom_class_id) = profile::dominant_receiver(receiver_counts, 80)
                        {
                            mic.prepopulate(dom_class_id);
                        }
                    }
                }

                // HIGH-7 — Eager PIC slot allocation alongside the
                // MIC. See the strategy comment at the `pic_slots`
                // declaration above. The PIC starts empty (all 4
                // entries have class_id == 0), so the inline cascade
                // falls through to the helper on cold sites. The
                // helper (`jit_invoke_virtual_mic`) populates entries
                // on miss, after which subsequent dispatches take
                // the inline fast path.
                //
                // `seed_from_mic` carries forward any profile-driven
                // pre-population we just applied to the MIC, so a
                // site with a known dominant receiver lands in PIC
                // slot 0 with class_id only (entry_ptr stays 0 →
                // first dispatch still rings the helper, which
                // installs entry_ptr; thereafter the cascade hits).
                let pic = Box::new(JitPICSlot::new_at(pc));
                // JIT round 20 wave 11, lane pLate (W18L4-2): a bound site's
                // PIC never overflows into the virtual-selection paths. See
                // `bound_pic_any_owner_enabled`.
                if sp_bound_site_ic && bound_pic_any_owner_enabled() {
                    pic.mark_bound_site();
                }
                // Fresh MIC: `cached_entry_word` is 0, so the seeded entry
                // short-circuits `jit_entry_publishable` before any policy
                // branch. See the matching note on the IR planner's seed.
                pic.seed_from_mic(&mic, false);
                // Round 13 wave 11 (lane mega9, M13-9): as the IR planner's
                // sites (`JitPICSlot::seed_megamorphic_from_profile`).
                if let Some(receiver_counts) = profile.and_then(|p| p.receivers.get(&pc)) {
                    pic.seed_megamorphic_from_profile(receiver_counts);
                }
                // JIT round 21 wave 4, lane ic (IC21-2): the site's
                // compile-time shared-table bind, before any code reads it.
                // SAFETY: `info_ptr` addresses the box just pushed into
                // `owned_invoke_infos`, which outlives the compile.
                offer_mega_site_bind(req, unsafe { &*info_ptr }, &pic);

                let mic_ptr: *const JitMICSlot = &*mic;
                owned_mic_slots.push(mic);
                mic_slots.push((pc, mic_ptr));

                let pic_ptr: *const JitPICSlot = &*pic;
                owned_pic_slots.push(pic);
                pic_slots.push((pc, pic_ptr));
            }
        }
    }

    // invokedynamic-uncommon-trap fix: resolve each `indy_ops` site's target
    // descriptor to the minimal stack-effect info the codegen needs (arg slot
    // count + return type tag) — see the `indy_info` field doc on the x64
    // `Compiler` struct. Mirrors the `field_info`/`invoke_info` resolution
    // blocks above: a `None` from the resolver (absent, or the site can't be
    // resolved) bails the WHOLE compile via `?`, exactly like every other
    // CP-resolved metadata table here — the codegen must never guess an
    // invokedynamic's stack effect.
    let mut indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)> = Vec::new();
    if !scan.indy_ops.is_empty() {
        let Some(resolver) = cp_invokedynamic_descriptor_resolver else {
            jitc_bail!("cp_invokedynamic_descriptor_resolver")
        };
        for &(pc, cp_idx) in &scan.indy_ops {
            let Some((descriptor, bridge_site)) = resolve_indy_site(req, resolver, cp_idx, pc)
            else {
                jitc_bail!("indy_descriptor_resolve")
            };
            let arg_slots = count_param_slots(&descriptor);
            let ret_type = return_type(&descriptor);
            let arg_type_tags = indy_arg_type_tags(&descriptor);
            // `bridge_site` is 0 for every bootstrap the VM cannot bridge,
            // which is the pre-bridge behaviour: the codegen lowers the site to
            // an uncommon trap. This used to be an unconditional 0 here, so the
            // WHOLE-METHOD door trapped even on the `StringConcatFactory` sites
            // the OSR door had been bridging since that fix landed.
            //
            // A BRIDGED site CALLS A HELPER, and every helper call in this
            // backend loads the hidden `SharedVm` pointer out of the frame slot
            // `heap_local_offset` names — a slot that only EXISTS when
            // `needs_heap` is set. Nothing else in a method like
            //
            //     static String f(int i) { return "v=" + i; }
            //
            // asks for it: there is no `new`, no field access, no ordinary
            // invoke, and the indy used to lower to a trap that calls nothing.
            // So `heap_local_offset` stayed 0, the bridge call loaded `[rbp-0]`
            // — the saved RBP — as its VM pointer, and the helper answered
            // with a null String from a garbage `SharedVm`.
            //
            // It reproduced ONLY through the whole-method door: an OSR compile
            // sets `needs_heap` for its own entry stub, which is why the concat
            // bridge ran correctly for the months it was OSR-only, and why
            // `probes/IndyBridgeProbe.java`'s loop-shaped arms all passed while
            // a three-instruction method returned null.
            if bridge_site != 0 {
                needs_heap = true;
            }
            indy_info.push((pc, arg_slots, ret_type, arg_type_tags, bridge_site));
        }
    }

    // JIT round 21 lane sbfuse (SBFUSE21-1): the single-pass fallback uses
    // the same concat bridge for stack-only chains whose operands cannot
    // trap. Keep their arguments on the simulated stack and route the final
    // `toString` through the ordinary invokedynamic staging/call path.
    let mut sb_chain_fusions = single_pass_sb_chain_fusion_plan(req, code, code_len);
    sb_chain_fusions.retain(|_, fusion| {
        let descriptor = fusion.chain.recipe().descriptor;
        let args_safe = descriptor
            .strip_prefix('(')
            .and_then(|tail| tail.split_once(')'))
            .is_some_and(|(args, _)| {
                let mut bytes = args.as_bytes();
                while let Some((&tag, rest)) = bytes.split_first() {
                    bytes = rest;
                    match tag {
                        b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {}
                        b'L' => {
                            let Some(end) = bytes.iter().position(|&b| b == b';') else {
                                return false;
                            };
                            if &bytes[..end] != b"java/lang/String" {
                                return false;
                            }
                            bytes = &bytes[end + 1..];
                        }
                        _ => return false,
                    }
                }
                true
            });
        cached.exception_table.is_empty()
            && args_safe
            && indy_arg_type_tags(&descriptor).len() == fusion.chain.value_count()
    });
    for fusion in sb_chain_fusions.values() {
        let recipe = fusion.chain.recipe();
        let arg_type_tags = indy_arg_type_tags(&recipe.descriptor);
        needs_heap = true;
        indy_info.push((
            fusion.chain.to_string_pc,
            arg_type_tags.len(),
            b'L',
            arg_type_tags,
            fusion.site,
        ));
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sb-chain-fuse singlepass {}.{}{} new_pc={} args={}",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                fusion.chain.new_pc,
                fusion.chain.value_count(),
            );
        }
    }

    // Prologue argument-slot count — includes the implicit `this` for
    // instance methods (see `prologue_param_slots` above).
    let param_slots = prologue_param_slots;

    let branch_hints: std::collections::HashMap<usize, bool> = profile
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

    // PGO: build loop unroll hints from profiled trip counts
    let loop_unroll_hints: std::collections::HashMap<usize, usize> = profile
        .map(|prof| {
            prof.loops
                .iter()
                .filter_map(|(&backedge_pc, trip)| {
                    trip.suggests_unroll_factor(8)
                        .map(|factor| (backedge_pc, factor))
                })
                .collect()
        })
        .unwrap_or_default();

    // Resolve `java/lang/String`'s field layout once, for the String
    // call-site intrinsics. `None` (no resolver, or String not yet
    // loadable) is fine — String intrinsic codegen treats it as "bail to
    // normal dispatch". Resolved here (cheap, once) so neither the
    // intrinsic matcher nor `x64::compile` needs the VM class registry.
    let string_layout: Option<StringFieldLayout> = string_layout_resolver.and_then(|r| r());

    // A protected invokestatic in the narrow precise-handler shape must retain
    // its explicit post-call reason-9 guard. Inlining would move the callee's
    // throwing operations into this body without independently snapshotting
    // each one, invalidating `precise_exception_frame_sites_supported`.
    //
    // Round nio20 wave 4 (lane L3, W3L3-1): under
    // [`sp_splice_precise_per_site_enabled`] the clear is per site, the same
    // two questions the planner asked (`sp_entry_precise_interlock_holds`)
    // and the emitter asks again (`try_emit_inline_site_with`): a site outside
    // every protected range keeps its splice (both precise publishers arm only
    // at a protected pc, and `may_file_by_bci` keeps them frame-less inside
    // any splice), a protected one only a static body that cannot throw
    // (`inline_site_body_cannot_throw`, which is false for every guarded
    // variant: a virtual body is never static). A guarded variant at a
    // protected pc goes (`plan_inline` refuses speculation there anyway:
    // `SpeculationInsideProtectedRange`). The reason-9 guard above is kept at
    // every protected site that still makes a call.
    if precise_exception_frames && sp_splice_precise_per_site_enabled() {
        let table = &cached.exception_table;
        inline_sites.retain(|&pc, site| {
            !pc_in_protected_range(table, pc) || inline_site_body_cannot_throw(site)
        });
        inline_guard_variants.retain(|&pc, _| !pc_in_protected_range(table, pc));
        // A guarded site's `inline_sites` entry is its first variant; keep the
        // two maps agreeing (a variant list without its primary is never
        // emitted, but say so here rather than rely on it).
        inline_guard_variants.retain(|pc, _| inline_sites.contains_key(pc));
    } else if precise_exception_frames {
        inline_sites.clear();
        // `inline_guard_variants` has to go with it. No behaviour change today —
        // `plan_inline` keeps it empty whenever `precise_exception_frames` is
        // set — but the two together are the interlock that keeps a deopt point
        // from ever being published inside a splice, and clearing only one of
        // them left that interlock resting on a policy decision in a different
        // function. The splice path driven by `inline_guard_variants` has been
        // on by default since N8, so a future change to `plan_inline` would have
        // re-opened the hazard silently. See `x64/inlining.rs`'s postcondition
        // and `r10-deoptverify-splice-published-point-would-carry-the-callers-locals`.
        inline_guard_variants.clear();
    }

    // round-7 fix (bug 1): from this point on, any `None` return is a
    // permanent backend bail — the resolver pre-checks all completed
    // successfully and we're about to walk the full
    // scan/IR/lowering pipeline.  Flag it so the outer wrapper adds
    // this method to the bail-list.
    //
    // "From this point on" spans the function boundary the table split
    // introduced: it covers the rest of this resolution AND the backend run
    // `single_pass_tier` performs afterwards, whose `?` is a `None` return with
    // this flag already set. The split moved where the backend is called; it
    // did not move where permanent-bail territory begins.
    *backend_attempted = true;

    // Parameter → JVM-slot layout, so the prologue places long/double params
    // (which span two JVM slots but arrive in one arg register) in the slots
    // the body reads. `param_slots` above is the *argument count* (one per
    // param, used for ABI register indexing); `param_slot_span` is the JVM
    // slot span (category-2 counted as two).
    let (param_jvm_slots, param_slot_span) =
        compute_param_jvm_slots(&cached.method_descriptor, cached.is_static);

    // Stage A.4 (precise oop maps, B-K fix) — seed the local-oop dataflow with
    // reference parameters, but ONLY when the precise gate is on. Off → `0`, so
    // `compute_local_oop_masks` keeps its historical empty entry state and the
    // emitted maps/codegen are byte-identical to the default path.
    // Also seed it under the moving young gen (`CRATONVM_MOVING_YOUNG`): its
    // complete-coverage shadow map must include an oop parameter live across an
    // EARLY safepoint (before any `astore` rewrites its slot), or the moving copy
    // would leave that register/slot stale (HIB-CV-20).
    // Also seed it under `deopt_real_enabled()` (default ON): `local_kinds`
    // (below) classifies `this`/reference params as `LocalKind::Ref`, and
    // `typed_local_frame_value` only trusts that classification when the
    // precise oop mask CONFIRMS it — an unseeded mask leaves every deopt
    // point's `this` (or any live reference parameter) as `FrameValue::
    // Unsupported`, which `resume_real_ir_deopt` cannot resume. Every such
    // deopt then silently falls back to the whole-method re-run, which
    // DOUBLE-EXECUTES every side effect already committed before the trap —
    // e.g. a `stack[ptr--]` decrement already written to the heap. This was
    // the root cause of the JDT `Parser` stack-corruption bug
    // (jasper-jdt-parser-arrayindexoutofbounds.md): an
    // always-deopting reference-array `System.arraycopy` call inside a method
    // with a live `this` made every single invocation re-run from entry.
    let param_oop_mask =
        if x64::precise_jit_maps_enabled() || x64::moving_young_enabled() || deopt_real_enabled() {
            compute_param_oop_mask(&cached.method_descriptor, cached.is_static)
        } else {
            0
        };

    // deopt-osr Step 9 follow-up (c): the per-bci de-spec key for this method
    // (same `"<class>.<method>:<descriptor>"` form the deopt log / method_epochs
    // use). Lets the backend skip a loop-header speculative-BCE guard recorded
    // in this VM's de-spec registry (`despec`). Nothing recorded ⇒ no effect.
    let despec_method_key = format!(
        "{}.{}:{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    );

    // What the single-pass backend is asked to do beyond the tables above: a
    // value handed to the call, so a front-end bail cannot leak it into the
    // next method compiled on this thread.
    let mut backend = x64::BackendRequest {
        c1_branch_counters: req
            .branch_counter_resolver
            .filter(|_| profile::single_pass_branch_recording_enabled())
            .map(|resolve| resolve()),
        direct_helpers: *direct_helpers,
        verified_max_stack: Some(cached.max_stack as usize),
        precise_exception_frames,
        // See the field's doc: the `ACC_SYNCHRONIZED` exclusion is the whole
        // reason this is a flag and not a `!precise_exception_frames` test, and
        // `cached` is the only place that knows it.
        allow_scalar_under_precise_frames: !cached.is_synchronized
            && scalar_under_precise_frames_enabled(),
        // Interpreter round i1 wave 19, lane L1: see the field's doc.
        debugger_observes_locals: req.debugger_observes_locals,
        // t1 wave 2, lane M: keep the locks of non-escaping objects while a
        // monitor observer may exist (see the field's doc).
        monitor_observer_at_compile: req.monitor_observer_at_compile,
        // t1 wave 3, lane M: no scalar replacement while an object observer
        // may exist (see the field's doc).
        scalar_observer_at_compile: req.scalar_observer_at_compile,
        // Interpreter round i1 wave 22, lane L1: read only under a debugger.
        param_slot_tags: if req.debugger_observes_locals {
            param_slot_kind_tags(&cached.method_descriptor, cached.is_static)
        } else {
            Vec::new()
        },
        // Round 13 wave 4 (lane sync2): this is the method-entry door, the
        // only one a self-locking body may come from. `single_pass_tier`
        // retries without it when the backend refuses the body, and (round
        // 13 wave 6, lane sync4) remembers the refusal, so it is not asked
        // again (`self_lock_refusal_memoized`).
        self_lock_receiver: self_lock_admits_method(
            cached,
            code,
            code_len,
            req.self_lock_mirror_slot,
        ) && !self_lock_refusal_memoized(req.verdicts(), cached),
        // Round 13 wave 10 (lane sync6): a `static synchronized` method's
        // monitor is its class mirror, read from this slot; an instance
        // method's is local 0 (`0` here).
        self_lock_mirror_slot: if cached.is_static {
            req.self_lock_mirror_slot
        } else {
            0
        },
        // Interpreter round i2 wave 54, lane L2: see the field's doc.
        method_known_unsynchronized: !cached.is_synchronized,
        // t1 wave 3, lane X: both raw static self-call arms above register a
        // `static_self_call_marker` row, so the emitter may refuse a site
        // that has none (see the field's doc).
        static_self_calls_registered: static_self_call_marker_enabled(),
        ..x64::BackendRequest::default()
    };
    // The adapter fold, armed by the constant pool rather than by the method's
    // name (interpreter round i1 wave 10): see `lambda_int_to_double_runs_proven`.
    backend.lambda_int_to_double_proven = match (cp_invoke_resolver, cp_class_name_resolver) {
        (Some(invoke), Some(class_name)) => {
            lambda_int_to_double_runs_proven(code, code_len, invoke, class_name)
        }
        _ => false,
    };
    // Unlike the flag above the protected ranges go with EVERY method that has
    // handlers, not just the precise-frame ones: the sibling tail-call they
    // suppress escapes a handler regardless of how that handler reconstructs
    // its locals.
    //
    // Staged by the one helper every single-pass door shares, together with
    // the handler edges and catch-all bits below (interpreter round i1 wave
    // 28, lane L2; `x64::BackendRequest::stage_exception_table`).
    backend.stage_exception_table(&cached.exception_table);
    // Pure-kernel GPR local homes: this is the METHOD-ENTRY compile path
    // (OSR artifacts go through the interpreter's `compile_osr_artifact`,
    // which never sets this), so request the kernel register homes. The
    // backend engages them only for call/field/alloc/typecheck-free bodies
    // with no speculative-BCE guards, keeps reference locals frame-homed,
    // and publishes the body without OSR entry points — see
    // `x64::kernel_reg_locals_enabled` for the safety argument.
    backend.kernel_reg_homes = true;
    // The JMM's StoreLoad fence after every volatile `putfield`. See
    // `x64::BackendRequest::volatile_field_pcs`.
    backend.volatile_field_pcs = volatile_field_pcs;
    // The one-shot first-execution guards of the call sites the compile door
    // found unresolved (interpreter round i1 wave 50, lane L1;
    // `JitFirstCallCell`). The planner above treats such a site as any other;
    // the guard goes in front of whatever it chose.
    backend.first_call_cells = first_call_cells_of(req, &scan.invoke_ops);
    // The field sites the door could not resolve, deferred to their first
    // execution (interpreter round i4 wave 92, lane L2;
    // `x64::BackendRequest::deferred_field_sites`). Empty unless
    // `CRATONVM_JIT_DEFERRED_FIELD_SITES` gave the request its resolver.
    if !deferred_field_sites.is_empty() && x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] deferred-field-sites {}.{}{}: {} site(s) at pcs {:?}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            deferred_field_sites.len(),
            deferred_field_sites
                .iter()
                .map(|&(pc, _)| pc)
                .collect::<Vec<_>>(),
        );
    }
    backend.deferred_field_sites = deferred_field_sites;
    // GC-maintained `ldc` constant slots (ldc7 request A.3); read by
    // `x64/objects.rs::ldc_site_slot`.
    backend.ldc_slots = ldc_slots;
    // The callee info for this method's raw self-recursive `CALL` sites. Set
    // only when the scan loop bound one of the two self-call shapes; see
    // `x64::BackendRequest::self_call_invoke_info`.
    backend.self_call_invoke_info = self_call_invoke_info;
    // Exception-handler entry edges are invisible to the backend's bytecode
    // branch decoding, so this method's table was staged above
    // (`stage_exception_table`) for `find_bypassable_loop_headers`, as
    // `(start_pc, end_pc, handler_pc)`: a handler that can be entered from
    // outside a loop lands in the body without running its pre-header. With
    // it, which entries are catch-alls (interpreter round i1 wave 27, lane
    // L2): see `x64::BackendRequest::exception_catch_all`. An empty exception
    // table stages empty vecs → unchanged codegen.
    // The same table again, with the one thing a compiled `catch` needs that
    // no liveness analysis does: WHICH throwables each entry takes.
    //
    // Staged only when the feature is on and the front end can name every
    // catch type — a table with one unresolvable entry is staged as nothing at
    // all, because a site built from a partial table would silently answer
    // "propagate" where the real table has a match. `catch_type == 0` is a
    // catch-all and carries the empty name rather than a resolution.
    //
    // `local_handlers_disarmed` is the retry: a compile that
    // refused with handler bodies in the image comes back through here once
    // with them suppressed, so the feature can cost a method throughput but
    // never its artifact.
    let local_handlers_requested =
        local_handlers_enabled() && !cached.exception_table.is_empty() && !local_handlers_disarmed;
    if local_handlers_requested {
        let mut table: Vec<(usize, usize, usize, &'static str)> =
            Vec::with_capacity(cached.exception_table.len());
        let mut resolvable = true;
        for e in cached.exception_table.iter() {
            let name: &'static str = if e.catch_type == 0 {
                ""
            } else {
                match cp_class_name_resolver
                    .as_ref()
                    .and_then(|r| r(e.catch_type))
                {
                    Some(n) => intern_catch_type_name(&n),
                    None => {
                        resolvable = false;
                        break;
                    }
                }
            };
            table.push((
                // Widening: classfile pcs are u16.
                e.start_pc as usize,
                e.end_pc as usize,
                e.handler_pc as usize,
                name,
            ));
        }
        if resolvable {
            backend.local_handler_table = table;
            backend.local_handler_class = cached.declaring_class_id.as_u32();
        }
    }
    // Only elide `<init>` calls proved empty by `cp_elidable_init_resolver` (none if absent);
    // see `docs/feature-designs/jit-ir-optimizations.md`.
    let elidable_init_pcs: Option<std::collections::HashSet<usize>> = cp_elidable_init_resolver
        .map(|resolver| {
            scan.invoke_ops
                .iter()
                .filter(|&&(_, _, opcode)| opcode == 0xb7)
                .filter(|&&(_, cp_idx, _)| resolver(cp_idx))
                .map(|&(pc, _, _)| pc)
                .collect()
        });
    // Intern spliced callees' invoke targets into this compile's `SinglePassParts` arenas so
    // they outlive emitted code; done after `precise_exception_frames` may clear `inline_sites`.
    for site in inline_sites.values_mut() {
        intern_inline_invoke_targets(
            site,
            &mut owned_strings,
            &mut owned_invoke_infos,
            &mut direct_callee_entries,
        );
    }
    for variants in inline_guard_variants.values_mut() {
        for (_, site) in variants.iter_mut() {
            intern_inline_invoke_targets(
                site,
                &mut owned_strings,
                &mut owned_invoke_infos,
                &mut direct_callee_entries,
            );
        }
    }

    // 2026-10-07 (item 1 of the i24-L6 page): what the artifact will keep of
    // its calls, as `jit_bridge::wrapped_body_calls_nothing` reads it.
    backend.artifact_keeps_a_call_record = !owned_invoke_infos.is_empty()
        || !owned_mic_slots.is_empty()
        || !owned_pic_slots.is_empty()
        || !owned_cycle_cells.is_empty()
        || !direct_callee_entries.is_empty();

    // The resolved plan. Both halves are moves, not copies: every table above
    // is handed over whole, and the split between the two structs is the split
    // between "the backend consumes this" and "the artifact is decorated with
    // this afterwards". See `SinglePassTables`' `Clone` note for why the raw
    // pointers in the first half and the arenas in the second have to stay
    // together.
    Some((
        SinglePassTables {
            param_slots,
            mna_info,
            field_info,
            typecheck_info,
            static_field_info,
            new_info,
            new_deferred_info,
            anewarray_info,
            anewarray_deferred_info,
            invoke_info,
            direct_calls,
            mic_slots,
            pic_slots,
            ldc_info,
            ldc_string_info,
            ldc_class_info,
            ldc2w_info,
            ldc_fp_pcs,
            branch_hints,
            loop_unroll_hints,
            inline_sites,
            inline_guard_variants,
            string_layout,
            param_oop_mask,
            compact_field_info,
            indy_info,
            sb_chain_fusions,
            elidable_init_pcs,
            backend,
            sync_self_call_fallback,
        },
        SinglePassParts {
            needs_heap,
            param_jvm_slots,
            param_slot_span,
            despec_method_key,
            owned_strings,
            owned_invoke_infos,
            owned_mic_slots,
            owned_pic_slots,
            owned_cycle_cells,
            owned_vh_judged_sites,
            direct_callee_entries,
            inlined_methods,
            inline_tally,
        },
    ))
}
