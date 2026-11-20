#[allow(unused_imports)]
use super::super::*;

/// The NESTED guarded sites (combined caller pcs) whose first or later body is
/// over the cold cap, at most `slots` of them, for
/// `IrBuilder::add_guarded_hot_cap_site` (JIT round 20, lane splice). Only the
/// VM's nested resolver admits such a body ([`ir_nested_guarded_body_cap`],
/// or the forwarder rule of [`ir_splice_nested_exact_size_cap`]); without the
/// registration the builder's guarded plan held it to 35 bytes and kept the
/// call. A site past `slots` keeps the cold cap (its body is then the exact
/// row it always was: spliced only for a proven receiver).
pub(crate) fn ir_nested_hot_cap_pcs(
    nested_rows: &[(usize, u32)],
    body_len: impl Fn(usize) -> Option<usize>,
    slots: usize,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    if !ir_nested_guarded_hot_cap_enabled() || !ir::ir_nested_guarded_splice_enabled() {
        return out;
    }
    for &(key, _) in nested_rows {
        let caller = ir::inline_site_caller_pc(key);
        if out.contains(&caller) {
            continue;
        }
        if body_len(key).is_some_and(|len| len > ir::GUARDED_SPLICE_MAX_BODY_BYTES) {
            if out.len() >= slots {
                break;
            }
            out.push(caller);
        }
    }
    out
}

// Round 16 (lane mega): the bodies of a POLYMORPHIC guarded splice at the
// caller's site, best class first, or the reason there are none (an
// `ir-polymorphic-census` token). `classes` is
// [`ir::profile_polymorphic_receiver_classes`]' answer. Same site rule and
// per-body filter as [`ir_profile_bimorphic_sites`]; the bodies are resolved
// in order and the first refusal ends the list, so a site keeps every class
// before it (the rest take the call). At least two bodies, or none.
// JIT round 19 wave 1 (lane osrpoly19): the planner now asks
// `ir_profile_polymorphic_sites_when`; this form is the tests' (process switch).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ir_profile_polymorphic_sites(
    opcode: u8,
    own_site: bool,
    classes: Option<Vec<u32>>,
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    cp_class: &str,
    name: &str,
    desc: &str,
) -> Result<Vec<IrGuardedBody>, &'static str> {
    ir_profile_polymorphic_sites_when(
        ir::ir_polymorphic_guarded_splice_enabled(),
        opcode,
        own_site,
        classes,
        resolver,
        cp_class,
        name,
        desc,
    )
}

/// [`ir_profile_polymorphic_sites`] with the switch answer passed in: JIT round
/// 19 wave 1 (lane osrpoly19) lets an OSR-door compile answer it with
/// [`ir::osr_door_ir_polymorphic_splice_enabled`] as well. nio21 wave 7 (lane
/// L4): the planner asks [`ir_profile_polymorphic_sites_capped`]; this
/// cold-cap form is the tests'.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ir_profile_polymorphic_sites_when(
    enabled: bool,
    opcode: u8,
    own_site: bool,
    classes: Option<Vec<u32>>,
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    cp_class: &str,
    name: &str,
    desc: &str,
) -> Result<Vec<IrGuardedBody>, &'static str> {
    ir_profile_polymorphic_sites_capped(
        enabled,
        opcode,
        own_site,
        classes,
        resolver,
        cp_class,
        name,
        desc,
        ir::GUARDED_SPLICE_MAX_BODY_BYTES,
    )
}

/// [`ir_profile_polymorphic_sites_when`] with the site's byte cap passed in
/// (nio21 wave 7, lane L4): each body may take [`ir_guarded_multi_body_cap`]'s
/// share of it, the share computed over every class the profile names.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_profile_polymorphic_sites_capped(
    enabled: bool,
    opcode: u8,
    own_site: bool,
    classes: Option<Vec<u32>>,
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    cp_class: &str,
    name: &str,
    desc: &str,
    site_cap: usize,
) -> Result<Vec<IrGuardedBody>, &'static str> {
    if !matches!(opcode, 0xb6 | 0xb9) || !own_site {
        return Err("not-an-own-virtual-site");
    }
    if !enabled {
        return Err("switched-off");
    }
    let classes = classes.ok_or("profile-not-polymorphic")?;
    let cap = ir_guarded_multi_body_cap(site_cap, classes.len());
    let mut bodies: Vec<IrGuardedBody> = Vec::with_capacity(classes.len());
    for class_id in classes {
        match ir_profile_guarded_site_capped(resolver, class_id, cp_class, name, desc, cap) {
            Some(body) => bodies.push(body),
            None => break,
        }
    }
    match bodies.len() {
        0 => Err("first-body-refused"),
        1 => Err("second-body-refused"),
        _ => Ok(bodies),
    }
}

/// Round 14 wave 2 (lane guard, GS-1): the second body of a bimorphic site
/// goes through the same per-site preparation the planner gives the first
/// (the nested-body strips and prunes, the fence relaxation, the store-fence
/// refusal, the single-pass-only veto), minus the diagnostic lines. `Err` is
/// the refusal token; the planner then drops the whole site.
pub(crate) fn ir_bimorphic_prepare_second_body(
    mut site: InlineSite,
    fence_flags: ir::SpliceFenceRelax,
    chain_fences: bool,
    plan_store_fence: bool,
    committed_store_admitted: bool,
) -> Result<(InlineSite, ir::SpliceFenceRelax), &'static str> {
    if x64::single_pass_only_lowering(&site.callee_code, site.callee_code_len, &[]).is_some() {
        return Err("ir-splice-single-pass-only");
    }
    // JIT round 20 (lane splice): the first body's handler-row gate (the
    // call-site edges' coverage, `set_splice_handler_edges`, the covered
    // nested strip) never ran for this body; the builder refused such a tree
    // at a spliced pc, which cost a W5-2 rebuild to reach the same answer.
    if ir_site_tree_has_handler_rows(&site) {
        return Err("ir-splice-callee-handlers-second-body");
    }
    if !site.nested_sites.is_empty() {
        ir_splice_strip_unadmitted_exact_nested(&mut site, chain_fences);
    }
    if !site.nested_sites.is_empty() {
        ir_splice_strip_string_prefix_nested(&mut site);
    }
    if chain_fences && !site.nested_sites.is_empty() {
        ir_splice_prune_cold_nested(&mut site);
    }
    if !site.nested_sites.is_empty() {
        ir_splice_prune_refused_nested(&mut site, fence_flags, chain_fences);
    }
    let relax = if ir_splice_tree_admits_fence_relaxation(&site) {
        fence_flags
    } else {
        ir::SpliceFenceRelax::NONE
    };
    if plan_store_fence
        && !chain_fences
        && ir_splice_store_fence_refuses_site(&site, committed_store_admitted)
    {
        return Err("ir-splice-trap-after-store");
    }
    Ok((site, relax))
}

/// rx27 lane bimodal: the receiver evidence at `pc` as one DBG word list --
/// `no-profile`, `no-row`, or `obs=N types=T top=C:N` -- for the
/// `ir-profile-guard-miss` census line.
pub(crate) fn ir_receiver_evidence_note(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> String {
    let Some(p) = profile else {
        return "no-profile".to_string();
    };
    match p.receiver_summary(pc) {
        None => "no-row".to_string(),
        Some(s) => match s.top {
            Some((c, n)) => format!("obs={} types={} top={c}:{n}", s.observations, s.types),
            None => format!("obs={} types={}", s.observations, s.types),
        },
    }
}

/// Round 14 wave 2 (lane guard, GS-1): the `[cratonvm-jitc]
/// ir-bimorphic-census` measurement of the site at `pc` -- `obs=N types=T
/// top=C:P% second=C:P%` -- for a site with at least
/// [`MIN_GUARDED_RECEIVER_EVIDENCE`] samples split over two or more classes,
/// the population GS-1 sizes. `None` for every other site.
pub(crate) fn ir_bimorphic_census_line(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> Option<String> {
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    let s = evidence.summary();
    if s.observations < MIN_GUARDED_RECEIVER_EVIDENCE || s.types < 2 {
        return None;
    }
    let pct = |n: u32| u64::from(n) * 100 / u64::from(s.observations.max(1));
    let (top, top_n) = s.top?;
    let (second, second_n) = s.second?;
    Some(format!(
        "obs={} types={} top={top}:{}% second={second}:{}%",
        s.observations,
        s.types,
        pct(top_n),
        pct(second_n),
    ))
}

/// Append one resolved callee body — and, recursively, its own spliced callees
/// — to `combined`, recording every side-table row the relocated body needs.
///
/// `caller_pc` is the pc of the `invoke` this body replaces, already in
/// COMBINED-buffer coordinates: the compiling method's own code for a top-level
/// site, the enclosing relocated body for a nested one. Both spaces are the
/// same space, which is what lets one function serve both.
///
/// Returns `Err(reason)` when the site's data is inconsistent with itself, when
/// a nested site needs a guard the IR tier cannot emit, when the walk budget
/// is spent, or when a splice rule refuses the body. The caller rolls
/// `combined` back — a half-added body is worse than none, because
/// `tables.sites` would then name a body whose `field_info` rows are missing
/// and the builder would bail the whole METHOD at its first `getfield`.
///
/// The reason (round 12 wave 5, lane iropt4, proposal W4-4) is a fixed token
/// the planner prints under `CRATONVM_DBG_JITC` as
/// `[cratonvm-jitc] ir-splice-refused <reason> ...`. Until then the six-plus
/// refusals answered one indistinguishable `false`, and the replay fence's
/// refusal of `HashMap.put`/`get` was found by reading instead of from a trace.
/// A nested body's refusal refuses its enclosing body with the same reason,
/// unless the nested call can stay a call (round 13 wave 5; see the nested loop
/// below and `ir_splice_prune_refused_nested`).
///
/// `fence_relax` is the outermost site's `ir::SpliceFenceRelax` (see
/// [`ir_splice_fence_refuses_site`]), handed down unchanged to nested bodies.
#[allow(clippy::too_many_arguments)]
/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the exact class each operand of
/// the compiling method's call at `pc` carries into the callee, by the
/// callee's local slot -- a `getstatic` of a trusted `static final` reference
/// whose object the VM filed a class for (`static_final_ref_class`), pushed by
/// the straight-line run before the call
/// ([`ir::splice_call_operand_sources`]). Empty when nothing is known. The
/// VM's resolver takes it ([`ir::take_splice_arg_classes`]) and keys the
/// body's nested exact rows on it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_root_site_arg_classes(
    builder: &ir::IrBuilder,
    static_final_ref_class: Option<&dyn Fn(u32, u32) -> Option<u32>>,
    new_locals: &HashMap<usize, u32>,
    indy_classes: &HashMap<usize, u32>,
    indy_shapes: &HashMap<usize, (usize, u8)>,
    invoke_ops: &[(usize, u16, u8)],
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
    pc: usize,
    opcode: u8,
    desc: &str,
) -> Vec<Option<u32>> {
    let Some(slots) = ir::descriptor_operand_slots(desc, opcode != 0xb8) else {
        return Vec::new();
    };
    let call_effect = |at: usize| -> Option<(usize, usize)> {
        if let Some(&(args, ret)) = indy_shapes.get(&at) {
            return Some((args, usize::from(ret != b'V')));
        }
        let &(_, cp_idx, op) = invoke_ops.iter().find(|&&(p, _, _)| p == at)?;
        let (_, _, d) = cp_invoke_resolver?(cp_idx)?;
        let pops = count_param_slots(&d) + usize::from(op != 0xb8);
        let pushes = usize::from(return_type(&d) != b'V');
        Some((pops, pushes))
    };
    let Some(sources) = ir::splice_call_operand_sources_with_calls(
        code,
        code_len,
        pc,
        slots.len(),
        handler_pcs,
        &call_effect,
    ) else {
        return Vec::new();
    };
    let mut out: Vec<Option<u32>> = Vec::new();
    for (&slot, source) in slots.iter().zip(sources) {
        let class = match source {
            // A write-once local holding a `new` (or a static final / indy constant ref).
            ir::SpliceOperandSource::Local(k) => new_locals.get(&k).copied().or_else(|| {
                super::splice_args::write_once_static_or_indy_local_class(
                    builder,
                    static_final_ref_class,
                    indy_classes,
                    code,
                    code_len,
                    handler_pcs,
                    pc,
                    k,
                )
            }),
            ir::SpliceOperandSource::GetStatic(gpc) => match builder.static_field_row(gpc) {
                Some((class_id, index, b'L' | b'[', false)) => {
                    static_final_ref_class.and_then(|class_of| {
                        u32::try_from(index)
                            .ok()
                            .and_then(|i| class_of(class_id, i))
                            // `0`: a null static final, no class to carry.
                            .filter(|&c| c != 0 && c != ir::STATIC_FINAL_OBJECT_FILED)
                    })
                }
                _ => None,
            },
            ir::SpliceOperandSource::Indy(indy_pc) => {
                indy_classes.get(&indy_pc).copied().filter(|&c| c != 0)
            }
            // Carried as objects (`ir_root_site_arg_objects`).
            ir::SpliceOperandSource::LocalField(..)
            | ir::SpliceOperandSource::StaticField(..)
            | ir::SpliceOperandSource::Other => None,
        };
        let Some(class) = class else {
            continue;
        };
        if out.len() <= slot {
            out.resize(slot + 1, None);
        }
        out[slot] = Some(class);
    }
    out
}

/// `CRATONVM_FROZEN_PAGES`: the frozen object each operand of the compiling
/// method's call at `pc` is expected to be, as `(class id, address)` by the
/// callee's local slot: the receiver itself (`this_addr`, the interpreter's
/// evidence), a trusted `static final`, or a reference field of either as
/// the snapshot holds it. Empty when nothing is known.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_root_site_arg_objects(
    builder: &ir::IrBuilder,
    facts: &frozen_facts::FrozenFacts,
    this_addr: Option<usize>,
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
    pc: usize,
    opcode: u8,
    desc: &str,
) -> Vec<Option<(u32, usize)>> {
    let Some(slots) = ir::descriptor_operand_slots(desc, opcode != 0xb8) else {
        return Vec::new();
    };
    let Some(sources) =
        ir::splice_call_operand_sources(code, code_len, pc, slots.len(), handler_pcs)
    else {
        return Vec::new();
    };
    let this = this_addr.filter(|_| ir::body_writes_local(code, code_len, 0) == Some(false));
    let root = |gpc: usize| match builder.static_field_row(gpc) {
        Some((class_id, index, b'L', false)) => {
            let id = *facts.roots.get(&(class_id, u32::try_from(index).ok()?))?;
            facts.objects.get(id as usize).map(|o| (o.class_id, o.addr))
        }
        _ => None,
    };
    let field = |base: usize, fpc: usize| match builder.field_row(fpc) {
        Some((index, b'L')) => facts.field_target(base, index),
        _ => None,
    };
    let mut out: Vec<Option<(u32, usize)>> = Vec::new();
    for (&slot, source) in slots.iter().zip(sources) {
        let object = match source {
            // The class of `this` is not claimed: only where it lives.
            ir::SpliceOperandSource::Local(0) => this.map(|addr| (0, addr)),
            ir::SpliceOperandSource::GetStatic(gpc) => root(gpc),
            ir::SpliceOperandSource::LocalField(0, fpc) => this.and_then(|addr| field(addr, fpc)),
            ir::SpliceOperandSource::StaticField(gpc, fpc) => {
                root(gpc).and_then(|(_, addr)| field(addr, fpc))
            }
            _ => None,
        };
        if object.is_some() {
            if out.len() <= slot {
                out.resize(slot + 1, None);
            }
            out[slot] = object;
        }
    }
    out
}

/// JIT round 25 wave 10 (W10-6): may `site`'s `anewarray` rows be rebased?
/// `Ok` when the body has none and no row, or when the switch
/// ([`ir::ir_splice_anewarray_enabled`]) is on and the rows are exactly the
/// body's `anewarray`s, including a resolved component class id of zero. The answer
/// the rebase in [`append_ir_inline_site`] spends.
pub(crate) fn ir_splice_anewarray_rows_ok(site: &InlineSite) -> Result<(), &'static str> {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut sites: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < len {
        if code.get(pc) == Some(&0xbd) {
            sites.push(pc);
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    if sites.is_empty() && site.ir_anewarray_info.is_empty() {
        return Ok(());
    }
    if !ir::ir_splice_anewarray_enabled() {
        return Err("ir-splice-anewarray-off");
    }
    let mut rows: Vec<usize> = site.ir_anewarray_info.iter().map(|&(cpc, _)| cpc).collect();
    rows.sort_unstable();
    rows.dedup();
    if rows != sites || rows.len() != site.ir_anewarray_info.len() {
        return Err("ir-splice-anewarray-row-mismatch");
    }
    Ok(())
}

#[inline]
pub(crate) fn ir_splice_keeps_gpu_dispatch_helper(
    callee_is_static: bool,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    callee_is_static
        && crate::offload_hook::keeps_dispatch_helper(class_name, method_name, descriptor)
}

pub(crate) fn append_ir_inline_site(
    mut site: InlineSite,
    caller_pc: usize,
    combined: &mut Vec<u8>,
    tables: &mut ir::IrInlineTables,
    owned_strings: &mut Vec<Box<str>>,
    owned_invoke_infos: &mut Vec<Box<JitInvokeInfo>>,
    direct_callee_entries: &mut Vec<(usize, u64)>,
    budget: &mut usize,
    // `(combined pc, num_jit_args)` for each virtual/interface call the spliced
    // bodies leave behind. The planner gives each its own MIC/PIC — without one
    // a surviving call falls back to a blind name resolution, and that trade
    // costs far more than the frame the splice removed (measured: 47 ns to
    // 11 000 ns per element on the `VoxelAlloc2` control arm).
    virtual_call_sites: &mut Vec<(usize, usize)>,
    // Rebased compact-layout rows for the spliced bodies' field accesses; goes
    // to `ir_lower`, not to the builder, which is why it is not in
    // `IrInlineTables`.
    compact_fields_out: &mut HashMap<(usize, bool), (u32, bool, u8)>,
    // The compiling method's value tier, for the spliced `getstatic` rows.
    // `getstatic` is polymorphic and is listed by neither `is_category2_opcode`
    // nor `is_float_opcode`, so a callee whose only wide or floating-point
    // content is a static read reaches here inside a method admitted through
    // the INT clause. The caller's own sites are gated on exactly this pair in
    // `try_compile_inner`; a spliced site gated differently would put a `Long`
    // node in a graph the long tier is switched off for.
    ir_emit_long: bool,
    ir_emit_fp: bool,
    // Declaring classes of the spliced `getstatic` sites. Compiled code reads
    // static storage directly, so every one of them is an ensure-init
    // obligation this artifact owes before its first entry — the same
    // obligation `ir_static_init_classes` records for the caller's own sites.
    // Collected here rather than derived later because this is the only place
    // that sees the callee's resolved rows.
    static_init_classes_out: &mut Vec<u32>,
    // `combined pc -> (callee entry, callee_needs_ctx)` for each surviving
    // STATICALLY BOUND call in the spliced bodies. Goes to `ir_direct_calls`,
    // which is what `ir_lower`'s `Op::Call` arm consults before falling
    // through to the dispatch helper.
    direct_call_sites: &mut Vec<(usize, (usize, bool))>,
    // Set when a spliced body carries a `checkcast`. The artifact then owes
    // `has_dispatch`, exactly as it does for one of the caller's own: a
    // definitive refusal publishes its `ClassCastException` through the
    // `JIT_THREAD` TLS, and the `!has_dispatch` fast entry never sets that TLS.
    // An obligation, not an optimization -- see `ir_needs_dispatch_for_checkcast`.
    spliced_checkcast_seen: &mut bool,
    // Combined-buffer pcs of the spliced bodies' volatile `getfield`s, for
    // `IrBuilder::add_spliced_volatile_field_pcs`. The caller merges it only on
    // success, like every other per-site output.
    volatile_pcs_out: &mut Vec<usize>,
    // Combined-buffer pc and owned late-static cell for an admitted unbound
    // `invokestatic` that survives inside the spliced body. The caller merges
    // these only when the whole site commits, then transfers ownership to the
    // compiled artifact.
    cycle_cell_sites_out: &mut Vec<(usize, Arc<JitCycleEdgeCell>)>,
    cycle_cell_admitter: Option<&dyn Fn(&InlineInvokeTarget) -> bool>,
    // Round 13 wave 10 (lane iropt8, HS-2): `(combined pc, class id)` of every
    // nested exact-receiver body appended ([`ir_splice_nested_exact_admitted`]),
    // for `IrBuilder::add_exact_receiver_site`. Merged only on success, like
    // every other per-site output: the builder would splice a pc in `sites`
    // that is not registered here UNGUARDED.
    exact_nested_out: &mut Vec<(usize, u32)>,
    // Bounded self-recursive inlining (`ir::ir_recursive_inline_enabled`): the
    // plan for this chain, or `None` -- flag off, or the compiling method has no
    // direct self-call row -- which is exactly the behaviour before it existed.
    self_rec: Option<&ir::IrSelfRecursion>,
    // The thin VM-side helper a surviving virtual/special call in a spliced
    // body may bind instead of the inline-cache cascade: `Some(entry)` for a
    // `(class, method, descriptor)` the caller's own-site ladder would also
    // bind (today the `Unsafe` accessor/CAS family), already policy-admitted
    // and counted. See the use below for why the caller's ladder alone did not
    // reach these sites.
    thin_helper_entry: &dyn Fn(&str, &str, &str) -> Option<usize>,
    fence_relax: ir::SpliceFenceRelax,
    // Round 13 wave 5 (lane hashsplice): whether the BUILDER will relax the
    // replay fences by chain snapshots for this compile -- the planner's
    // `ir_chain_fences`, which is off for an `ACC_SYNCHRONIZED` method (no
    // `Graph::scope_method_key`, so no chain snapshot). Reading
    // `ir_splice_chain_fences_enabled()` here instead skipped the call fence
    // for such a method while its builder still applied it, so every body the
    // fence refuses refused the whole build (a W5-2 rebuild each, then the
    // method's optimizing compile).
    chain_fences: bool,
) -> Result<(), &'static str> {
    let code_len = site.callee_code_len;
    if code_len == 0 || site.callee_code.len() < code_len {
        return Err("ir-splice-code-length");
    }
    let live_len = ir_splice_live_code_len(&site);
    if live_len > *budget {
        return Err("ir-splice-budget-spent");
    }
    if ir_splice_keeps_gpu_dispatch_helper(
        site.callee_is_static,
        &site.class_name,
        &site.method_name,
        &site.descriptor,
    ) {
        return Err("ir-splice-gpu-kernel");
    }
    let (arg_local_slots, _span) = compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
    let Some((desc_args, ret)) = static_call_shape(&site.descriptor) else {
        return Err("ir-splice-descriptor-shape");
    };
    let num_args = desc_args + usize::from(!site.callee_is_static);
    // `compute_param_jvm_slots` counts one entry per VALUE and `static_call_shape`
    // counts one arg per value, so these agree unless the descriptor was parsed
    // two different ways — which is exactly the disagreement that would install
    // an argument in the wrong local.
    if crate::offload_hook::is_reflect_ctor_dispatch(
        &site.class_name,
        &site.method_name,
        &site.descriptor,
    ) {
        return Err("reflect-ctor-dispatch");
    }
    if arg_local_slots.len() != num_args {
        return Err("ir-splice-argument-slots-disagree");
    }
    if arg_local_slots.iter().any(|&s| s >= site.callee_max_locals) {
        return Err("ir-splice-argument-slot-outside-frame");
    }
    // Round 11 wave 11 (lane `calls`). Is this body a bounded self-recursive
    // COPY of the method being compiled? Such a copy is spliced only when it
    // can neither trap nor call anything but itself (see
    // `ir_self_copy_body_is_trap_free` for the whole-method refusal that
    // prevents); otherwise the site stays the direct self-call it was.
    //
    // Round 11 wave 15 (lane `fib`): or when a trap inside it re-runs nothing
    // a replay could observe (`ir_self_copy_body_replays_safely`), which is
    // what gives binary-trees' `itemCheck` and `bottomUpTree` their two levels
    // back after the merge with dev. `fib` still takes the trap-free route.
    let self_copy = self_rec.is_some_and(|plan| {
        !matches!(
            plan.classify(
                &site.class_name,
                &site.method_name,
                &site.descriptor,
                site.class_id,
                site.callee_is_static,
            ),
            ir::IrSelfSplice::NotSelf
        )
    });
    if self_copy
        && !ir_self_copy_body_is_trap_free(&site)
        && !(ir_self_copy_replay_safe_enabled() && ir_self_copy_body_replays_safely(&site))
    {
        return Err("ir-splice-self-copy-may-trap");
    }
    // A body with several `return`s is spliced under the general
    // (default-off) switch, or as such a copy -- nowhere else. The resolver
    // admits a self-recursive multi-return body for ANY caller, because it
    // cannot tell who is compiling; every caller other than the method itself
    // keeps the call, exactly as before that admission existed.
    // Round 14 wave 5 (lane sync5, SS8-1 b): or as a synchronized splice
    // whose scan admits the returns (the builder closes its window after
    // their join).
    let sync_multi_return = site.callee_is_synchronized
        && ir::ir_sync_splice_enabled()
        && ir::ir_sync_splice_multi_return_enabled()
        && ir_sync_splice_body_scan(&site).is_ok();
    // Round 14 wave 6 (lane sync6, SS8-1 a): may this synchronized body's
    // nested synchronized bodies be appended? Only when its scan admitted every
    // nested row as a same-receiver body (`ir_sync_splice_nested_held_site`).
    // Asked here, before the nested rows are taken below (the scan reads
    // them). Not under a self-recursion plan, whose copies are not modelled.
    let sync_held_admitted = site.callee_is_synchronized
        && !site.callee_is_static
        && self_rec.is_none()
        && ir::ir_sync_splice_nested_same_receiver_enabled()
        && ir_sync_splice_body_scan(&site).is_ok();
    // SS-7's answer, likewise read while the nested rows are still on `site`
    // (used by the unbindable-call refusal after the nested loop).
    let folds_every_call = ir_sync_splice_folds_every_call(&site);
    // Round 14 wave 7 (lane sync7, SS8-5): a `CONSTANT_Class` `ldc` row is
    // admitted only as the self-class `holdsLock` pair of a `static
    // synchronized` body its scan admits; the builder has no other use for
    // one (it would bail the METHOD at that pc), so any other body carrying a
    // row is refused here and keeps its call.
    if !site.ir_ldc_class_info.is_empty()
        && !(site.callee_is_synchronized
            && site.callee_is_static
            && self_rec.is_none()
            && ir_sync_splice_body_scan(&site).is_ok())
    {
        return Err("ir-splice-ldc-class");
    }
    if !self_copy
        && !sync_multi_return
        && !ir::ir_splice_multi_return_enabled()
        && ir_splice_site_has_several_returns(&site)
    {
        return Err("ir-splice-several-returns");
    }
    // A trap after a call that stays a call would re-run the call (round 11
    // wave 16, lane `replay`). Refuse the site here, so the method still
    // compiles; the builder would otherwise refuse the whole method. See
    // `ir_splice_body_may_trap_after_a_call`, and, for the round-12-wave-5
    // relaxations, `ir_splice_fence_refuses_site`. Round 13 wave 3 (lane
    // framestate, W5-3 stage 3): not under `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES`,
    // where a trap after the call resumes the callee through its caller chain
    // and re-runs nothing. The builder stays the gate: a pc it has no chain
    // snapshot for keeps the fence, and a refusal there costs a W5-2 rebuild.
    if !chain_fences && ir_splice_fence_refuses_site(&site, self_rec, fence_relax) {
        return Err("ir-splice-trap-after-call");
    }

    // `CRATONVM_JIT_IR_SPLICE_SWITCH`: a body with a switch starts at a
    // 4-aligned pc, where its operand padding is the callee's own. The pad
    // is `nop`s no walk enters.
    if ir::ir_splice_switch_enabled()
        && ir::body_has_switch(&site.callee_code, code_len) == Some(true)
    {
        while combined.len() % 4 != 0 {
            combined.push(0x00);
        }
    }
    let base = combined.len();
    combined.extend_from_slice(&site.callee_code[..code_len]);
    if let Some(original) = &site.callee_original_code {
        tables.original_body_codes.insert(base, original.clone());
    }
    // rx27 (lane matcher): see `splice_scope_sources` / `spliced_body_codes`.
    if !site.callee_original_handler_rows.is_empty() {
        tables
            .original_body_handler_rows
            .insert(base, site.callee_original_handler_rows.clone());
    }
    *budget -= live_len;
    tables.splice_caller_frames.insert(
        base,
        (site.ir_caller_mirror_slot, site.ir_security_walk_ignored),
    );

    tables.sites.insert(
        caller_pc,
        ir::IrInlineSite {
            base,
            code_len,
            num_args,
            max_locals: site.callee_max_locals,
            arg_local_slots: arg_local_slots.iter().map(|&s| s as u32).collect(),
            returns_value: ret != b'V',
            // The receiver the deleted invoke used to null-check. See
            // `ir::IrInlineSite::receiver_is_arg0`.
            receiver_is_arg0: !site.callee_is_static,
            // Whose bytecode this body is. The single-pass inliner has recorded
            // the same pair since 2026-09-01 (`x64::inlining`'s
            // `inline_site_label` / `InlineSite::class_id`); this tier spliced
            // the body and dropped the identity, so every callee it inlined
            // contributed no frame to a stack trace. Same string shape, built
            // from the same three fields, so the two tiers' chains are
            // indistinguishable to the consumer.
            method_key: format!(
                "{}.{}:{}",
                site.class_name, site.method_name, site.descriptor
            ),
            class_id: site.class_id,
        },
    );
    // Round nio21 wave 4 (lane L2): the admitted exception rows, rebased.
    // Whether this compile may cover them is the builder's question
    // (`IrBuilder::set_splice_handler_edges`, refused at the `invoke`).
    if !site.ir_handler_rows.is_empty() {
        let orig_code = site
            .callee_original_code
            .as_deref()
            .unwrap_or(&site.callee_code);
        let rows: Vec<ir::IrSpliceHandlerRow> = site
            .ir_handler_rows
            .iter()
            .map(
                |(start, end, handler, name, declaring)| ir::IrSpliceHandlerRow {
                    start_pc: base + start,
                    end_pc: base + end,
                    handler_pc: base + handler,
                    catch_name: intern_catch_type_name(name),
                    declaring_class_id: *declaring,
                    bare_athrow: ir::splice_handler_is_bare_athrow(orig_code, *handler),
                },
            )
            .collect();
        tables.splice_handler_rows.insert(base, rows);
    }
    // JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`):
    // the callee's never-taken branches, rebased, with the code only their
    // taken edges reach. `try_compile_inner` drops withdrawn rows.
    let prune_feed = ir_splice_prune_feed_enabled();
    let never_rows: Vec<usize> = if !site.callee_never_taken_pcs.is_empty() && prune_feed {
        let key = format!(
            "{}.{}:{}",
            site.class_name, site.method_name, site.descriptor
        );
        let rows: Vec<usize> = site
            .callee_never_taken_pcs
            .iter()
            .copied()
            .filter(|&pc| never_taken_prune_candidate(&site.callee_code, code_len, pc))
            .collect();
        for &pc in &rows {
            if let Ok(bci) = u32::try_from(pc) {
                tables
                    .spliced_never_taken
                    .push((base + pc, key.clone(), bci));
            }
        }
        rows
    } else {
        Vec::new()
    };
    let always_rows: Vec<usize> = if !site.callee_always_taken_pcs.is_empty() && prune_feed {
        let key = format!(
            "{}.{}:{}",
            site.class_name, site.method_name, site.descriptor
        );
        let rows: Vec<usize> = site
            .callee_always_taken_pcs
            .iter()
            .copied()
            .filter(|&pc| never_taken_prune_candidate(&site.callee_code, code_len, pc))
            .collect();
        for &pc in &rows {
            if let Ok(bci) = u32::try_from(pc) {
                tables
                    .spliced_always_taken
                    .push((base + pc, key.clone(), bci));
            }
        }
        rows
    } else {
        Vec::new()
    };
    let switch_rows: Vec<(usize, usize)> =
        if !site.callee_pruned_switch_edges.is_empty() && prune_feed {
            let key = format!(
                "{}.{}:{}",
                site.class_name, site.method_name, site.descriptor
            );
            let rows: Vec<(usize, usize)> = site
                .callee_pruned_switch_edges
                .iter()
                .copied()
                .filter(|&(sw_pc, target_pc)| sw_pc < code_len && target_pc < code_len)
                .collect();
            for &(sw_pc, target_pc) in &rows {
                if let Ok(bci) = u32::try_from(sw_pc) {
                    tables.spliced_pruned_switch_edges.push((
                        base + sw_pc,
                        base + target_pc,
                        key.clone(),
                        bci,
                    ));
                }
            }
            rows
        } else {
            Vec::new()
        };
    if !never_rows.is_empty() || !always_rows.is_empty() || !switch_rows.is_empty() {
        if let Some((live, _)) = splice_live_pcs_without_pruned_and_switch(
            &site.callee_code,
            code_len,
            &always_rows,
            &never_rows,
            &switch_rows,
        ) {
            let dead_iter = (0..code_len)
                .filter(|&pc| !live.get(pc).copied().unwrap_or(false))
                .map(|pc| base + pc);
            if !always_rows.is_empty() {
                tables.spliced_always_taken_dead.extend(dead_iter);
            } else if !never_rows.is_empty() {
                tables.spliced_never_taken_dead.extend(dead_iter);
            } else {
                tables.spliced_pruned_switch_dead.extend(dead_iter);
            }
        }
    }
    // JIT round 23 wave 8 (lane W8-L2, CF-W8-3): the callee's rarely taken
    // arms, rebased, for the acceptance gate's pricing only.
    if !site.callee_cold_arm_ranges.is_empty() && ir_evidence::cold_blind_dispatch_weight_enabled()
    {
        tables.spliced_cold_ranges.extend(
            site.callee_cold_arm_ranges
                .iter()
                .filter(|&&(s, e, _)| s < e && e <= code_len)
                .map(|&(s, e, w)| (base + s, base + e, w)),
        );
    }
    // nio21 wave 11, lane L3 (W10L3-1): constant-word lambda sites, rebased.
    // Every row is re-checked against the relocated bytes and the one shape
    // the builder admits inside a splice (`(0, L)`, a non-zero site); a body
    // with handler rows keeps its call (a covered range admits only armed
    // call-site edges, and the bridge's cold exit is not one).
    if !site.ir_indy_rows.is_empty() {
        if !ir::ir_splice_indy_enabled() {
            return Err("ir-splice-indy-off");
        }
        if !site.ir_handler_rows.is_empty() {
            return Err("ir-splice-indy-with-handlers");
        }
        if site.callee_is_synchronized {
            return Err("ir-splice-indy-synchronized");
        }
        for &(cpc, arg_entries, ret_tag, bridge_site) in &site.ir_indy_rows {
            // j27 wave 4 (lane SPLICECAT): or `(n > 0, L)`, a concat bridge
            // row, under `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`.
            if site.callee_code.get(cpc).copied() != Some(0xba)
                || cpc + 5 > code_len
                || !splice_concat_bridge::indy_row_shape_admitted(arg_entries, ret_tag)
                || bridge_site == 0
            {
                return Err("ir-splice-indy-row-mismatch");
            }
            tables
                .indy_trap_sites
                .insert(base + cpc, (arg_entries, ret_tag));
            tables.indy_bridge_sites.insert(base + cpc, bridge_site);
        }
    }
    // JIT round 20 wave 19 (lane ySplice, SPLICE20-1): linked CAPTURING
    // lambda sites, rebased for `IrBuilder::try_indy_capture_new`. Re-checked
    // against the relocated bytes; a body with handler rows (an allocation's
    // exit inside a covered range is not an armed call-site edge) or a
    // synchronized one keeps its call, as the constant-word rows.
    if !site.ir_indy_capture_rows.is_empty() {
        if !ir::ir_splice_indy_capture_enabled() {
            return Err("ir-splice-indy-capture-off");
        }
        if !site.ir_handler_rows.is_empty() {
            return Err("ir-splice-indy-with-handlers");
        }
        if site.callee_is_synchronized {
            return Err("ir-splice-indy-synchronized");
        }
        for (cpc, proxy_class_id, capture_tags) in &site.ir_indy_capture_rows {
            let cpc = *cpc;
            if site.callee_code.get(cpc).copied() != Some(0xba)
                || cpc + 5 > code_len
                || *proxy_class_id == 0
                || capture_tags.is_empty()
                || site.ir_indy_rows.iter().any(|r| r.0 == cpc)
            {
                return Err("ir-splice-indy-row-mismatch");
            }
            tables
                .indy_trap_sites
                .insert(base + cpc, (capture_tags.len(), b'L'));
            tables.indy_capture_news.insert(
                base + cpc,
                ir::IndyCaptureNew {
                    proxy_class_id: *proxy_class_id,
                    capture_tags: capture_tags.clone(),
                },
            );
        }
    }
    for &(cpc, field_index, type_tag) in &site.field_info {
        tables
            .field_info
            .insert(base + cpc, (field_index, type_tag));
    }
    // JIT r20 wave 18 (lane xLib, THROW20-4): the `Reference.referent` reads
    // the VM admitted into this optimizing splice, rebased. Each must be a
    // `getfield` of slot 0 with its reference `field_info` row; anything else
    // refuses the BODY (dropping the pc would let an inline arm serve the
    // load without the keep-alive). The two loops below never give such a pc
    // a declaring-class identity or a compact row.
    for &cpc in &site.referent_load_pcs {
        if site.callee_code.get(cpc).copied() != Some(0xb4)
            || !site
                .field_info
                .iter()
                .any(|&(fpc, field_index, tag)| fpc == cpc && field_index == 0 && tag == b'L')
        {
            return Err("ir-splice-referent-row-mismatch");
        }
        tables.referent_load_pcs.insert(base + cpc);
    }
    // Round 15 wave 3 (lane chain3, CHN15-2 producer): the declaring classes,
    // rebased, each only beside the `field_info` row (same pc, same index) it
    // was resolved with; anything else is dropped (no identity), never guessed.
    for &(cpc, decl_index, decl) in &site.field_decl_info {
        if site.referent_load_pcs.contains(&cpc) {
            continue;
        }
        if decl != 0
            && site
                .field_info
                .iter()
                .any(|&(fpc, field_index, _)| fpc == cpc && field_index == decl_index)
        {
            tables.field_decl_class.insert(base + cpc, decl);
        }
    }
    // Volatile reads, rebased. The builder orders each one (an Acquire
    // `Op::Load`); a pc it is not told about would build as a plain load
    // that LICM may hoist. Only `getfield`: the resolver refuses a body with
    // a volatile `putfield`. With the IR's volatile modelling switched off
    // the builder would bail the whole METHOD at this pc, so refuse the body.
    if !site.volatile_field_pcs.is_empty() && !ir::ir_volatile_fields_enabled() {
        return Err("ir-splice-volatile-read-modelling-off");
    }
    // JIT round 21 wave 13 (lane cf): a volatile `putfield` too, under
    // `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` (`ir::ir_splice_volatile_store_enabled`;
    // the resolver admits the row under the same reader). The builder builds
    // it as an ordered `Op::Store` and `ir_lower` fences it.
    let volatile_stores_admitted = site
        .volatile_field_pcs
        .iter()
        .any(|&cpc| site.callee_code.get(cpc).copied() == Some(0xb5))
        // JIT r23 wave 11b (lane L5): or `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE_FRESH`,
        // whose receiver rule the builder's `putfield` arm enforces.
        && ir::ir_splice_volatile_store_admitted();
    for &cpc in &site.volatile_field_pcs {
        ir::spliced_volatile_row_admitted(
            site.callee_code.get(cpc).copied(),
            volatile_stores_admitted,
        )?;
        volatile_pcs_out.push(base + cpc);
    }
    // nio21 wave 2 (lane L3): final reads, rebased. Only a `getfield` with a
    // `field_info` row at the same pc is kept; anything else is dropped (not
    // forwarded), never guessed.
    for &cpc in &site.final_field_pcs {
        if site.callee_code.get(cpc).copied() == Some(0xb4)
            && site.field_info.iter().any(|&(fpc, _, _)| fpc == cpc)
        {
            tables.final_field_pcs.insert(base + cpc);
        }
    }
    // nio21 wave 7 (lane L4, W5L4-3): `byte[]`-valued reads, rebased on the
    // same terms (a `getfield` with its `field_info` row, else dropped), and
    // only under the switch the lowering reads.
    if ir::ir_scoped_byte_array_base_enabled() {
        for &cpc in &site.byte_array_field_pcs {
            if site.callee_code.get(cpc).copied() == Some(0xb4)
                && site
                    .field_info
                    .iter()
                    .any(|&(fpc, _, tag)| fpc == cpc && tag == b'[')
            {
                tables.byte_array_field_pcs.insert(base + cpc);
            }
        }
    }
    // The compact byte offset for each of those fields, rebased the same way.
    // Without it `ir_lower` routes every spliced field access through the
    // checked `jit_getfield` helper — a boundary note, a region walk and a
    // 16-byte atomic cell read per access, which the single-pass backend
    // measured at 4.7x. The descriptor tag comes from the `field_info` row
    // above, so the two cannot disagree about what kind of field this is; a
    // compact row with no matching `field_info` row is dropped rather than
    // guessed.
    for &(cpc, byte_offset, is_ref) in &site.compact_field_info {
        if site.referent_load_pcs.contains(&cpc) {
            continue;
        }
        if let Some(&(_, _, type_tag)) = site.field_info.iter().find(|(fpc, _, _)| *fpc == cpc) {
            compact_fields_out.insert((base + cpc, is_ref), (byte_offset, is_ref, type_tag));
        }
    }
    for &(cpc, class_id, num_fields, finalizable) in &site.ir_new_info {
        // JIT round 20 wave 19 (lane ySplice, SPLICE20-2): UNLOADED_SPLICE_CLASS_ID is a
        // `new` whose class the resolver found NOT loaded, admitted only on an
        // arm the callee's profile never ran. It becomes a precise exit at the
        // `new` (the builder's `0xbb` arm, `IrBuilder::plant_unloaded_typecheck_exit`),
        // which resumes through a chain snapshot: without chain fences or site
        // traps there is none, so the body keeps its call (as the type checks).
        if class_id == crate::UNLOADED_SPLICE_CLASS_ID {
            if !ir::ir_splice_unresolved_new_exit_enabled()
                || !unloaded_static_call_trap_admitted(chain_fences)
            {
                return Err("ir-splice-new-site-unresolved");
            }
            if site.callee_code.get(cpc).copied() != Some(0xbb) {
                return Err("ir-splice-unresolved-new-row-mismatch");
            }
            tables.unloaded_static_call_pcs.insert(base + cpc);
            continue;
        }
        tables.new_info.insert(base + cpc, (class_id, num_fields));
        // JIT round 20 wave 12 (lane hFin): see `IrInlineTables::finalizable_new_classes`.
        if finalizable {
            tables.finalizable_new_classes.insert(class_id);
        }
    }
    // JIT round 25 wave 10 (W10-6, `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`): the
    // body's resolved reference-array allocations, rebased. Every row must
    // sit on an `anewarray` of the relocated bytes and name a class, and
    // every `anewarray` of the bytes must have its row: the builder's `0xbd`
    // arm bails the whole METHOD at a spliced `anewarray` without one, so a
    // body that cannot vouch for each of them is refused here and keeps its
    // call.
    if let Err(why) = ir_splice_anewarray_rows_ok(&site) {
        return Err(why);
    }
    for &(cpc, component_class_id) in &site.ir_anewarray_info {
        tables.anewarray_info.insert(base + cpc, component_class_id);
    }
    // Constants, rebased with the type tag the builder needs. The resolver has
    // already refused any `ldc` whose constant-pool entry is not an
    // Integer/Float (or Long/Double for `ldc2_w`), so a pc present here is one
    // of exactly those four kinds and `ldc_fp_pcs` splits them.
    for &(cpc, val) in &site.ldc_info {
        tables
            .ldc_info
            .insert(base + cpc, (val, site.ldc_fp_pcs.contains(&cpc)));
    }
    for &(cpc, val) in &site.ldc2w_info {
        tables
            .ldc2w_info
            .insert(base + cpc, (val, site.ldc_fp_pcs.contains(&cpc)));
    }
    // Round 14 wave 7 (lane sync7, SS8-5): admitted above only as self-class
    // `holdsLock` pairs.
    for &(cpc, class_id) in &site.ir_ldc_class_info {
        tables
            .ldc_self_class_holds_lock
            .insert(base + cpc, class_id);
    }
    // JIT r20 wave 19 (lane zLdc, THROW20-4 `date`): resolved String / Class
    // constants, rebased. Each row must sit on an `ldc`/`ldc_w` of the
    // relocated bytes and carry its slot; anything else refuses the BODY
    // (the builder would bail the whole METHOD at a spliced `ldc` with no
    // row).
    if !site.ir_ldc_ref_rows.is_empty() {
        if !ir::ir_splice_ref_ldc_enabled() {
            return Err("ir-splice-ref-ldc-off");
        }
        for &(cpc, holder, cp_idx, is_class, slot_addr) in &site.ir_ldc_ref_rows {
            // JIT r23 wave 9, lane L6: a String row with slot `0` is an
            // unresolved constant the resolver admitted on a never-run arm
            // only. It becomes the builder's precise exit
            // (`IrBuilder::plant_unresolved_ldc_exit`), as the class-id-0
            // `new` row does.
            match splice_unresolved_ldc_row_verdict(
                site.callee_code.get(cpc).copied(),
                cpc < code_len,
                holder,
                is_class,
                slot_addr,
                ir::ir_splice_unresolved_ldc_exit_enabled(),
                unloaded_static_call_trap_admitted(chain_fences),
            ) {
                UnresolvedLdcRow::NotUnresolved => {}
                UnresolvedLdcRow::Exit => {
                    tables.unloaded_static_call_pcs.insert(base + cpc);
                    continue;
                }
                UnresolvedLdcRow::Refuse(why) => return Err(why),
            }
            if !matches!(site.callee_code.get(cpc).copied(), Some(0x12 | 0x13))
                || cpc >= code_len
                || holder == 0
                || slot_addr == 0
            {
                return Err("ir-splice-ref-ldc-row-mismatch");
            }
            if is_class {
                tables.ldc_ref_class.insert(base + cpc, (holder, cp_idx));
            } else {
                tables.ldc_ref_string.insert(base + cpc, (holder, cp_idx));
            }
            tables.ldc_ref_slots.insert((holder, cp_idx), slot_addr);
        }
        // Only beside a class row at the `ldc` right after the invoke; a row
        // that does not line up is dropped (the call is built), never guessed.
        for &(cpc, class_id) in &site.ir_getclass_compares {
            let ldc_pc = cpc + 3;
            if class_id != 0
                && site.callee_code.get(cpc).copied() == Some(0xb6)
                && site
                    .ir_ldc_ref_rows
                    .iter()
                    .any(|&(p, _, _, is_class, _)| p == ldc_pc && is_class)
            {
                tables.splice_getclass_compares.insert(base + cpc, class_id);
            }
        }
    }
    // Static reads, rebased. `InlineSite::static_field_info` has carried these
    // resolved rows for the single-pass inliner all along; the optimizing tier
    // refused the callee outright (`ir-splice-static-field`) because nothing
    // put them where the builder's `0xb2` arm looks.
    //
    // Two guards, and neither is redundant with the resolver's:
    //
    // * The OPCODE is re-read from the relocated bytes. `static_field_info` is
    //   a `getstatic`-and-`putstatic` list on the single-pass side, and the
    //   builder's `putstatic` arm (round 11 wave 10, `Op::StoreStatic`) bails
    //   inside a splice — one such row reaching the map would be a `getstatic`
    //   lowering planted at a store. The resolver
    //   refuses a `putstatic`-bearing body, so this can only fire on a defect,
    //   which is the case worth having a check for.
    // * The VALUE TIER, exactly as `try_compile_inner` gates the caller's own
    //   feed. A gated-off site cannot simply be omitted the way the caller's
    //   is: omitting it there bails the method to single-pass, but omitting it
    //   HERE bails the method after the splice has already been committed to.
    //   Refusing the body instead rolls `combined` back and leaves the caller
    //   compiling with one less inline.
    if ir::ir_splice_getstatic_enabled() {
        for &(cpc, class_id, field_index, type_tag, is_volatile) in &site.static_field_info {
            // Round 14 wave 2 (lane syncsplice): the one `putstatic` a
            // splice may carry, a `static synchronized` body's non-volatile
            // write to its own class (`ir_sync_splice_body_scan`; the builder's
            // `0xb3` arm asks the same of its open frame).
            let sync_own_putstatic = site.callee_is_synchronized
                && site.callee_is_static
                && class_id != 0
                && class_id == site.class_id
                && !is_volatile
                && site.callee_code.get(cpc).copied() == Some(0xb3);
            // JIT round 21 (lane staticfwd, FG16-3): a plain non-volatile
            // `putstatic` the resolver admitted (its class initialised); the
            // builder commits it behind the replay fence.
            let plain_putstatic = ir::ir_splice_putstatic_enabled()
                && class_id != 0
                && !is_volatile
                && site.callee_code.get(cpc).copied() == Some(0xb3);
            if site.callee_code.get(cpc).copied() != Some(0xb2)
                && !sync_own_putstatic
                && !plain_putstatic
            {
                return Err("ir-splice-static-row-not-a-getstatic");
            }
            // Class id 0: a `getstatic` whose owner the resolver found NOT
            // loaded, on an arm the callee's profile never ran. A precise exit,
            // as the class-id-0 `new` row below; the row stays for its tag.
            if class_id == crate::UNLOADED_SPLICE_CLASS_ID {
                if !ir::ir_splice_unresolved_new_exit_enabled()
                    || !unloaded_static_call_trap_admitted(chain_fences)
                {
                    return Err("ir-splice-static-field-unresolved");
                }
                tables.unloaded_static_call_pcs.insert(base + cpc);
            }
            // ...but not in a body that may loop: the back edge the walk
            // reaches after the commit can only be refused by the builder's
            // replay fence, which costs a whole build (and a W5-2 rebuild)
            // instead of this one body. j27 wave 7 (lane T1STATICFWD,
            // STATICFWD21-1, `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY`):
            // unless a replay re-runs the body harmlessly, in which case the
            // builder commits nothing and raises no fence.
            if plain_putstatic
                && !sync_own_putstatic
                && ir_splice_body_may_loop(&site)
                && !ir_splice_static_replay::planner_admits_loop_site(
                    &site,
                    &tables.sites,
                    caller_pc,
                )
                // j27 wave 8 (lane W8T1, STATICFWD21-1 reference half,
                // `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN`): or the
                // builder's chain fences resume every later trap in the callee.
                && !ir_splice_static_chain::planner_admits_chain_loop_site(
                    &site,
                    caller_pc,
                    chain_fences,
                )
            {
                return Err("ir-splice-putstatic-loop");
            }
            let admitted_by_value_tier = match type_tag {
                b'J' => ir_emit_long,
                b'D' | b'F' => ir_emit_fp,
                _ => true,
            };
            if !admitted_by_value_tier {
                return Err("ir-splice-static-value-tier-off");
            }
            tables
                .static_field_info
                .insert(base + cpc, (class_id, field_index, type_tag, is_volatile));
            if class_id != crate::UNLOADED_SPLICE_CLASS_ID {
                static_init_classes_out.push(class_id);
            }
        }
    } else if !site.static_field_info.is_empty() {
        // The kill switch is set and the VM-side scanner mirror should already
        // have refused this body. Refuse here too rather than splice a body
        // whose statics have no rows: the two halves must not be able to
        // disagree. See `ir::ir_splice_getstatic_enabled`.
        return Err("ir-splice-getstatic-off");
    }
    // Type checks, rebased. The resolver has already refused the body unless
    // every site's target class was loaded and named, so a row here is
    // complete; interning is done at this point rather than there because this
    // is where the combined-buffer pc is known, and because
    // `intern_typecheck_target` is the jit crate's table.
    //
    // The interned `(ptr, len)` is process-lifetime and deliberately NOT owned
    // by this compile -- see `intern_typecheck_class_name` on why the pair the
    // type-check helpers memoize on must never be recycled for another class --
    // so unlike the invoke rows these need no keep-alive entry on the artifact.
    if ir::ir_splice_typecheck_enabled() {
        for (cpc, class_id, name, is_checkcast) in &site.ir_typecheck_info {
            let expected = if *is_checkcast { 0xc0 } else { 0xc1 };
            if site.callee_code.get(*cpc).copied() != Some(expected) {
                return Err("ir-splice-typecheck-row-mismatch");
            }
            // Round nio20 wave 1, lane L1 (NIO19-3): class id 0 is a target
            // the resolver found NOT loaded, admitted only on an arm the
            // callee's profile never ran. It becomes a precise exit
            // (`IrBuilder::plant_unloaded_typecheck_exit`), which resumes
            // through a chain snapshot: without chain fences there is none,
            // so the body keeps its call.
            if *class_id == crate::UNLOADED_SPLICE_CLASS_ID {
                // JIT round 19 wave 11 (lane tail11): and only with site
                // traps on -- the builder's exit refuses the whole build
                // without them (`plant_unloaded_typecheck_exit`).
                if !unloaded_static_call_trap_admitted(chain_fences) {
                    return Err("ir-splice-typecheck-target-not-loaded");
                }
                tables.unloaded_static_call_pcs.insert(base + *cpc);
                continue;
            }
            let (ptr, len) = intern_typecheck_target_with_finality(
                name,
                Some(*class_id),
                site.ir_typecheck_final.contains(cpc),
            );
            let row = (ptr as usize, len);
            if *is_checkcast {
                tables.checkcast_info.insert(base + *cpc, row);
                *spliced_checkcast_seen = true;
            } else {
                tables.instanceof_info.insert(base + *cpc, row);
            }
        }
    } else if !site.ir_typecheck_info.is_empty() {
        // Kill switch set; the scanner mirror should already have refused.
        // Refuse here too -- the halves must not be able to disagree. See
        // `ir::ir_splice_typecheck_enabled`.
        return Err("ir-splice-typecheck-off");
    }
    // The resolver PROVED these bodies are no-ops, which is what
    // `object_init_pcs` means — elidable on any receiver, not only on a fresh
    // `Op::New`. `trivial_init_pcs` is the narrower set and would refuse the
    // `super()` on `this` every constructor starts with.
    for &cpc in &site.elided_invoke_pcs {
        tables.object_init_pcs.insert(base + cpc);
    }

    // Nested splices first, so their callee pcs are known before the remaining
    // invokes are interned as ordinary calls. A pc carries BOTH a nested site
    // and an `invoke_targets` entry — the resolver records them additively so
    // the single-pass emitter can fall back mid-body. `IrBuilder` has no such
    // fallback: it checks `inline_sites` first and never consults `invoke_info`
    // for that pc, so installing both would leave a dead row, and installing
    // the call instead of the splice would leave the allocation escaping.
    let nested = std::mem::take(&mut site.nested_sites);
    let mut nested_pcs: std::collections::HashSet<usize> =
        std::collections::HashSet::with_capacity(nested.len());
    for n in nested {
        // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): a lambda impl
        // the compile door grafted at a SAM `invokeinterface`
        // (`ir_graft_spliced_lambda_impls`). Appended into scratch outputs and
        // merged only on success, like a degradable nested body, but ALWAYS
        // degradable: a refusal leaves the SAM call the inline-cache call it
        // was (its `invoke_targets` row is kept) and never refuses this body.
        // The builder's row is registered only for an appended impl, so it
        // never meets a planned pc without the impl's body there.
        if let Some(&(_, indy_pc, captures, impl_params)) =
            site.ir_lambda_impl_rows.iter().find(|r| r.0 == n.callee_pc)
        {
            if self_rec.is_some()
                || n.guard_class_id != 0
                || n.site.callee_is_synchronized
                || n.bimorphic_second.is_some()
            {
                continue;
            }
            let mark = combined.len();
            let budget_mark = *budget;
            let entries_mark = direct_callee_entries.len();
            let mut n_tables = ir::IrInlineTables::default();
            let mut n_vcalls: Vec<(usize, usize)> = Vec::new();
            let mut n_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            let mut n_static_init: Vec<u32> = Vec::new();
            let mut n_direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
            let mut n_checkcast_seen = false;
            let mut n_volatile: Vec<usize> = Vec::new();
            let mut n_cycle_cells: Vec<(usize, Arc<JitCycleEdgeCell>)> = Vec::new();
            let mut n_exact: Vec<(usize, u32)> = Vec::new();
            let appended = append_ir_inline_site(
                n.site,
                base + n.callee_pc,
                combined,
                &mut n_tables,
                owned_strings,
                owned_invoke_infos,
                direct_callee_entries,
                budget,
                &mut n_vcalls,
                &mut n_compact,
                ir_emit_long,
                ir_emit_fp,
                &mut n_static_init,
                &mut n_direct_calls,
                &mut n_checkcast_seen,
                &mut n_volatile,
                &mut n_cycle_cells,
                cycle_cell_admitter,
                &mut n_exact,
                None,
                thin_helper_entry,
                fence_relax,
                chain_fences,
            );
            if appended.is_ok() && n_tables.sites.contains_key(&(base + n.callee_pc)) {
                merge_ir_inline_tables(tables, n_tables);
                virtual_call_sites.extend(n_vcalls);
                compact_fields_out.extend(n_compact);
                static_init_classes_out.extend(n_static_init);
                direct_call_sites.extend(n_direct_calls);
                cycle_cell_sites_out.extend(n_cycle_cells);
                *spliced_checkcast_seen |= n_checkcast_seen;
                volatile_pcs_out.extend(n_volatile);
                exact_nested_out.extend(n_exact);
                nested_pcs.insert(n.callee_pc);
                tables
                    .lambda_impl_sites
                    .insert(base + n.callee_pc, (base + indy_pc, captures, impl_params));
            } else {
                combined.truncate(mark);
                *budget = budget_mark;
                direct_callee_entries.truncate(entries_mark);
                if x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-lambda-impl-in-splice refused at combined pc={}: {} \
                         (the SAM call stays its inline cache)",
                        base + n.callee_pc,
                        appended.err().unwrap_or("no site row"),
                    );
                }
            }
            continue;
        }
        // Round 14 wave 2 (lane syncsplice): a synchronized body is spliced
        // only at the compiling method's own sites, with its monitor pair.
        // Round 14 wave 6 (lane sync6, SS8-1 a): or, with NO pair, inside a
        // synchronized body on that body's own receiver: its combined pc is
        // registered as held (`ir::IrInlineTables::sync_held_sites`), where
        // the builder splices it only under the enclosing window with its
        // receiver proven that window's monitor. Appended strictly (a refusal
        // refuses this body: a call kept in its place would sit in the window).
        // Any other nested synchronized body can only be a defect.
        if n.site.callee_is_synchronized {
            if !sync_held_admitted {
                return Err("ir-sync-splice-nested");
            }
            nested_pcs.insert(n.callee_pc);
            tables.sync_held_sites.insert(base + n.callee_pc);
            append_ir_inline_site(
                n.site,
                base + n.callee_pc,
                combined,
                tables,
                owned_strings,
                owned_invoke_infos,
                direct_callee_entries,
                budget,
                virtual_call_sites,
                compact_fields_out,
                ir_emit_long,
                ir_emit_fp,
                static_init_classes_out,
                direct_call_sites,
                spliced_checkcast_seen,
                volatile_pcs_out,
                cycle_cell_sites_out,
                cycle_cell_admitter,
                exact_nested_out,
                None,
                thin_helper_entry,
                fence_relax,
                chain_fences,
            )?;
            continue;
        }
        // Round 13 wave 10 (lane iropt8, HS-2): an exact-receiver row -- a
        // virtual call the resolver resolved for its constant-pool class --
        // is appended, registered as an exact-receiver site, and its call row
        // KEPT (the pc stays out of `nested_pcs`, so the interning below gives
        // it `invoke_info` and an inline cache): the builder splices the body
        // only when it proves the receiver node exactly that class, and lowers
        // the call otherwise. A body refused during its append stays that call
        // (the builder owns the fences here, see the admission).
        let exact_nested = n.guard_class_id != 0
            && ir_splice_nested_exact_admitted_in(Some(&site), &n, chain_fences)
            && ir_splice_nested_call_survives(&site, n.callee_pc);
        // A guarded nested site needs a receiver class-id compare the IR tier
        // has no node for. `resolve_ir_inline_site` never produces one (it
        // routes every kind through the statically-bound arm and refuses a
        // target that is not provably monomorphic); refuse rather than splice
        // it unguarded, which would be silent wrong code at an overriding site.
        if n.guard_class_id != 0 && !exact_nested {
            return Err("ir-splice-guarded-nested-site");
        }
        if exact_nested {
            // A self-recursion plan's copies and leaves are not modelled for
            // these rows: the call stays.
            if self_rec.is_some() {
                continue;
            }
            let mark = combined.len();
            let budget_mark = *budget;
            let entries_mark = direct_callee_entries.len();
            let mut n_tables = ir::IrInlineTables::default();
            let mut n_vcalls: Vec<(usize, usize)> = Vec::new();
            let mut n_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            let mut n_static_init: Vec<u32> = Vec::new();
            let mut n_direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
            let mut n_checkcast_seen = false;
            let mut n_volatile: Vec<usize> = Vec::new();
            let mut n_cycle_cells: Vec<(usize, Arc<JitCycleEdgeCell>)> = Vec::new();
            let mut n_exact: Vec<(usize, u32)> = Vec::new();
            let class_id = n.guard_class_id;
            // Round rx27 wave 3 (lane chain): a chain site's pair also under
            // `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`.
            let pair_admitted = r25_w7b_chain_splice::nested_pair_admitted_row(&site, &n);
            let second = n.bimorphic_second;
            let appended = append_ir_inline_site(
                n.site,
                base + n.callee_pc,
                combined,
                &mut n_tables,
                owned_strings,
                owned_invoke_infos,
                direct_callee_entries,
                budget,
                &mut n_vcalls,
                &mut n_compact,
                ir_emit_long,
                ir_emit_fp,
                &mut n_static_init,
                &mut n_direct_calls,
                &mut n_checkcast_seen,
                &mut n_volatile,
                &mut n_cycle_cells,
                cycle_cell_admitter,
                &mut n_exact,
                None,
                thin_helper_entry,
                fence_relax,
                chain_fences,
            );
            if appended.is_ok() {
                merge_ir_inline_tables(tables, n_tables);
                virtual_call_sites.extend(n_vcalls);
                compact_fields_out.extend(n_compact);
                static_init_classes_out.extend(n_static_init);
                direct_call_sites.extend(n_direct_calls);
                cycle_cell_sites_out.extend(n_cycle_cells);
                *spliced_checkcast_seen |= n_checkcast_seen;
                volatile_pcs_out.extend(n_volatile);
                exact_nested_out.extend(n_exact);
                exact_nested_out.push((base + n.callee_pc, class_id));
                // JIT round 19 wave 2 (lane splice2, SPLICE19-4): a second
                // class's body, appended after the first into its own scratch
                // outputs and re-keyed under `ir::bimorphic_second_site_key`
                // (the site table holds one body per pc), exactly as the
                // planner appends a top-level bimorphic site's. Registered for
                // the builder as a TAGGED `exact_nested_out` row, which the
                // registration loop turns into `add_bimorphic_receiver_site`.
                // A refusal drops the second body only: the first stays the
                // one-class nested row it was.
                if let Some(second) = second.filter(|_| pair_admitted) {
                    let mark2 = combined.len();
                    let budget_mark2 = *budget;
                    let entries_mark2 = direct_callee_entries.len();
                    let second_class = second.guard_class_id;
                    let mut s_tables = ir::IrInlineTables::default();
                    let mut s_vcalls: Vec<(usize, usize)> = Vec::new();
                    let mut s_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
                    let mut s_static_init: Vec<u32> = Vec::new();
                    let mut s_direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
                    let mut s_checkcast_seen = false;
                    let mut s_volatile: Vec<usize> = Vec::new();
                    let mut s_cycle_cells: Vec<(usize, Arc<JitCycleEdgeCell>)> = Vec::new();
                    let mut s_exact: Vec<(usize, u32)> = Vec::new();
                    // The relaxation this tree was planned under must hold
                    // for the second body's tree too, which the tree-wide
                    // rule never saw (`ir_splice_tree_admits_fence_relaxation`).
                    let appended = if fence_relax.any()
                        && !ir_splice_tree_admits_fence_relaxation(&second.site)
                    {
                        Err("ir-splice-nested-bimorphic-relaxation")
                    } else {
                        append_ir_inline_site(
                            second.site,
                            base + n.callee_pc,
                            combined,
                            &mut s_tables,
                            owned_strings,
                            owned_invoke_infos,
                            direct_callee_entries,
                            budget,
                            &mut s_vcalls,
                            &mut s_compact,
                            ir_emit_long,
                            ir_emit_fp,
                            &mut s_static_init,
                            &mut s_direct_calls,
                            &mut s_checkcast_seen,
                            &mut s_volatile,
                            &mut s_cycle_cells,
                            cycle_cell_admitter,
                            &mut s_exact,
                            None,
                            thin_helper_entry,
                            fence_relax,
                            chain_fences,
                        )
                    };
                    match appended {
                        Ok(()) => {
                            if let Some(row) = s_tables.sites.remove(&(base + n.callee_pc)) {
                                s_tables
                                    .sites
                                    .insert(ir::bimorphic_second_site_key(base + n.callee_pc), row);
                            }
                            merge_ir_inline_tables(tables, s_tables);
                            virtual_call_sites.extend(s_vcalls);
                            compact_fields_out.extend(s_compact);
                            static_init_classes_out.extend(s_static_init);
                            direct_call_sites.extend(s_direct_calls);
                            cycle_cell_sites_out.extend(s_cycle_cells);
                            *spliced_checkcast_seen |= s_checkcast_seen;
                            volatile_pcs_out.extend(s_volatile);
                            exact_nested_out.extend(s_exact);
                            exact_nested_out.push((
                                ir::bimorphic_second_site_key(base + n.callee_pc),
                                second_class,
                            ));
                        }
                        Err(why) => {
                            combined.truncate(mark2);
                            *budget = budget_mark2;
                            direct_callee_entries.truncate(entries_mark2);
                            if x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-nested-bimorphic second body of class {second_class} \
                                     at combined pc={} refused: {why}",
                                    base + n.callee_pc,
                                );
                            }
                        }
                    }
                }
            } else {
                combined.truncate(mark);
                *budget = budget_mark;
                direct_callee_entries.truncate(entries_mark);
            }
            continue;
        }
        // Bounded self-recursive inlining. A self site with a copy left is
        // spliced with a deeper plan; one past the budget is NOT spliced -- its
        // call stays, bound to this method's own direct self-call row
        // (`invoke_kind == 4`), so the recursion continues in the body being
        // compiled rather than in a previous artifact. `nested_pcs` keeps that
        // pc out of the ordinary-call rows and the unbindable-call refusal
        // below: this row replaces them.
        let nested_self_rec: Option<ir::IrSelfRecursion> = match self_rec {
            None => None,
            Some(plan) => match plan.classify(
                &n.site.class_name,
                &n.site.method_name,
                &n.site.descriptor,
                n.site.class_id,
                n.site.callee_is_static,
            ) {
                ir::IrSelfSplice::NotSelf => Some(plan.clone()),
                ir::IrSelfSplice::Splice(deeper) => Some(deeper),
                ir::IrSelfSplice::Leaf(row) => {
                    // Only inside an admitted self copy (round 11 wave 16,
                    // lane `replay`). The builder exempts a leaf from its
                    // surviving-call replay fence (`IrBuilder::
                    // is_self_copy_leaf_call`) on the strength of the copy
                    // admission, so a leaf in any other body would be exempt
                    // without that proof. With `IR_RECURSIVE_INLINE_MAX_COPIES`
                    // copies and copies admitting no foreign call this cannot
                    // happen today; refuse rather than rely on it.
                    if !self_copy {
                        return Err("ir-splice-self-leaf-outside-a-copy");
                    }
                    nested_pcs.insert(n.callee_pc);
                    tables.invoke_info.insert(base + n.callee_pc, row);
                    continue;
                }
            },
        };
        nested_pcs.insert(n.callee_pc);
        // Round 13 wave 5 (lane hashsplice): a nested body refused DURING its
        // append (the byte budget, a row check) may fall back to its call
        // instead of refusing this body, when that call can survive and the
        // fence judgement this body already passed counted a call at this pc
        // anyway (it left one in the nested subtree), or the builder owns the
        // fence (chain fences). Its rows go to scratch outputs, merged only on
        // success, exactly like a top-level site's. The static refusals were
        // pruned before the append (`ir_splice_prune_refused_nested`).
        let may_degrade = self_rec.is_none()
            && ir_splice_nested_call_survives(&site, n.callee_pc)
            && (chain_fences || ir_splice_nested_leaves_a_call(&n, None));
        // Round 13 wave 6 (lane chain2; item 2 of
        // `r13w5-hashsplice-nested-append-refusals-and-cold-budget`): a nested
        // body over the cold size that would leave less than the reserve for
        // the top-level sites still to be planned stays its call, under
        // `CRATONVM_JIT_IR_SPLICE_NESTED_COLD_RESERVE` (default off).
        if may_degrade
            && n.site.callee_code_len > MAX_INLINE_SIZE_COLD
            && (*budget).saturating_sub(n.site.callee_code_len) < IR_INLINE_NESTED_COLD_RESERVE
            && ir_splice_nested_cold_reserve_enabled()
        {
            nested_pcs.remove(&n.callee_pc);
            continue;
        }
        if !may_degrade {
            append_ir_inline_site(
                n.site,
                base + n.callee_pc,
                combined,
                tables,
                owned_strings,
                owned_invoke_infos,
                direct_callee_entries,
                budget,
                virtual_call_sites,
                compact_fields_out,
                ir_emit_long,
                ir_emit_fp,
                static_init_classes_out,
                direct_call_sites,
                spliced_checkcast_seen,
                volatile_pcs_out,
                cycle_cell_sites_out,
                cycle_cell_admitter,
                exact_nested_out,
                nested_self_rec.as_ref(),
                thin_helper_entry,
                fence_relax,
                chain_fences,
            )?;
            continue;
        }
        let mark = combined.len();
        let budget_mark = *budget;
        // The rolled-back body's keep-alive rows too: a pinned callee this
        // artifact never calls would only tie its invalidation to this one.
        let entries_mark = direct_callee_entries.len();
        let mut n_tables = ir::IrInlineTables::default();
        let mut n_vcalls: Vec<(usize, usize)> = Vec::new();
        let mut n_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let mut n_static_init: Vec<u32> = Vec::new();
        let mut n_direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
        let mut n_checkcast_seen = false;
        let mut n_volatile: Vec<usize> = Vec::new();
        let mut n_cycle_cells: Vec<(usize, Arc<JitCycleEdgeCell>)> = Vec::new();
        let mut n_exact: Vec<(usize, u32)> = Vec::new();
        let appended = append_ir_inline_site(
            n.site,
            base + n.callee_pc,
            combined,
            &mut n_tables,
            owned_strings,
            owned_invoke_infos,
            direct_callee_entries,
            budget,
            &mut n_vcalls,
            &mut n_compact,
            ir_emit_long,
            ir_emit_fp,
            &mut n_static_init,
            &mut n_direct_calls,
            &mut n_checkcast_seen,
            &mut n_volatile,
            &mut n_cycle_cells,
            cycle_cell_admitter,
            &mut n_exact,
            None,
            thin_helper_entry,
            fence_relax,
            chain_fences,
        );
        if appended.is_ok() {
            merge_ir_inline_tables(tables, n_tables);
            virtual_call_sites.extend(n_vcalls);
            compact_fields_out.extend(n_compact);
            static_init_classes_out.extend(n_static_init);
            direct_call_sites.extend(n_direct_calls);
            cycle_cell_sites_out.extend(n_cycle_cells);
            *spliced_checkcast_seen |= n_checkcast_seen;
            volatile_pcs_out.extend(n_volatile);
            exact_nested_out.extend(n_exact);
        } else {
            combined.truncate(mark);
            *budget = budget_mark;
            direct_callee_entries.truncate(entries_mark);
            nested_pcs.remove(&n.callee_pc);
        }
    }

    // The calls the body makes that stay calls. Interned into THIS compile's
    // arenas by the same routine the single-pass path uses, so the `&'static
    // str` names and the baked `*const JitInvokeInfo` have identical lifetime
    // rules on both paths.
    // A splice must not leave behind a call WORSE than the one it replaced.
    //
    // The single-pass resolver has always refused a splice on that rule; IR
    // mode was exempted from it on the stated grounds that "the IR tier's
    // spliced call is a call, not a resolution". That premise holds only while
    // every surviving call can be lowered to something better than
    // `jit_invoke_dispatch`, and there is one shape where it cannot:
    //
    //   * virtual / interface (kind 0, 2) get the MIC/PIC cascade, which needs
    //     no plan-time binding -- it caches on the runtime receiver;
    //   * statically bound (kind 1, 3) get a raw `CALL`, but ONLY if the
    //     resolver produced a `direct_entry`. With none there is no cache to
    //     fall back on, and the call lowers to a blind NAME RESOLUTION at
    //     ~175 ns against a direct `CALL`'s ~4.
    //
    // Measured on `bench/SpliceCastProbe.java` (2026-09-10), which is where
    // `c2-splice-checkcast-and-instanceof-20260909.md` recorded
    // `ir blind dispatches: own_code=0 in_splice=1` and attributed it to a
    // virtual `ArrayList.elementData` missing its cascade. It is not that. The
    // site is `jdk/internal/util/Preconditions.checkIndex`, `invokestatic`, and
    // it is there because `Objects.checkIndex(II)I` -- which IS compiled and
    // `bg-direct-call BOUND` -- was spliced into `ArrayList.get`, and the call
    // its body leaves behind is `native-shadow`, which the direct-bind path
    // DECLINES. So the splice traded a bound direct call for a resolution: the
    // exact trade this rule exists to refuse, arrived at through the exemption.
    //
    // Refuse before `intern_inline_invoke_targets`, so a site rejected here
    // also registers no keep-alive entry for a callee this artifact will not
    // call.
    // Round 14 wave 4 (lane sync4, SS-7): a synchronized body's
    // `holdsLock(this)` is folded by the builder, never lowered as a call
    // (`folds_every_call`, read above before the nested rows were taken).
    // JIT round 16 (lane callbind, wave b): the box-thin admission below only
    // for a body with no loop. Admitting it for a loop-bearing body moved
    // whole hot loops into their callers (`R16OsrdoorParts.run` took
    // `get(I)J`, `put(I)J`, `pair(I)J`: 28 bodies, its budget spent), where
    // the loop's own call sites lose the profile they are guarded by in the
    // callee's own compile (a nested site has no profile-guarded splice,
    // CB16-1) -- Linux w5a: `get-*` 25 -> 94 ns/op. A loop-free boxing helper
    // is the shape the switch exists for.
    let box_thin_body = ir_splice_box_thin_admits_body(&site.callee_code, site.callee_code_len);
    let live_mask = ir_splice_live_pc_mask(&site);
    if ir_splice_refuse_unbindable_call_enabled()
        && !folds_every_call
        && site.invoke_targets.iter().any(|(pc, t)| {
            !nested_pcs.contains(pc)
                && !ir_splice_row_is_prune_dead(&live_mask, *pc)
                && !matches!(t.invoke_kind, 0 | 2)
                && t.direct_entry.is_none()
                // JIT round 23 wave 4 (CF23-1): or one the bound-site cache
                // serves (`append_ir_inline_site` routes it to the pair).
                && !ir_splice_unbound_call_takes_ic(t)
                // JIT r25 wave 11 (W11-2): a `System.arraycopy` row is bound
                // after the build (`ir_arraycopy_entry`), like the caller's own.
                && !r25_w11_2_arraycopy_row::counts_bound(t)
                // JIT round 22 (lane memory): a `Reference.reachabilityFence`
                // row is built as an `Op::KeepAlive`, no call at all.
                && !r22_fence_row::counts_keepalive(t)
                // j27 wave 8 (lane W8SPL): a `Thread.onSpinWait()` row is
                // lowered as `PAUSE` (`ir_lower::on_spin_wait`), no call.
                && !ir_lower::on_spin_wait::counts_as_barrier_row(t)
                && !is_reflection_get_caller_class_row(t)
                && !ir_lower::ir_spliced_prefix_counts_bound(t)
                && !(unloaded_static_call_trap_admitted(chain_fences)
                    && site.unloaded_static_call_pcs.contains(pc))
                // JIT round 16 (lane callbind): a static call the interning
                // below binds to a thin helper (`Integer.valueOf`,
                // `Long.valueOf` under `CRATONVM_JIT_IR_SPLICE_BOX_THIN`) is
                // bound, in a loop-free body (`box_thin_body`). Only kind 3:
                // the thin closure's other arm, the `Unsafe` accessors, are
                // instance calls, and asking it for them here would count
                // their sites twice.
                && !(t.invoke_kind == 3
                    && box_thin_body
                    && thin_helper_entry(
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str(),
                    )
                    .is_some())
                // nio21 wave 1 L5 (L3W6-1): a row the post-plan pass lowers as
                // an `Op::ScalarIntrinsic` (`ir_plan_spliced_scalar_intrinsics`)
                // leaves no call behind.
                && ir_spliced_scalar_row(&t.class_name, &t.method_name, &t.descriptor, t.invoke_kind)
                    .is_none()
                && !is_ir_lowered_buffer_or_charset_intrinsic(t)
                && !(ir_splice_static_cell_candidate(t)
                    && cycle_cell_admitter.is_some_and(|admit| admit(t)))
        })
    {
        return Err("ir-splice-unbindable-surviving-call");
    }
    intern_inline_invoke_targets(
        &mut site,
        owned_strings,
        owned_invoke_infos,
        direct_callee_entries,
    );
    for r in &site.resolved_invoke_infos {
        if nested_pcs.contains(&r.callee_pc) {
            continue;
        }
        if unloaded_static_call_trap_admitted(chain_fences)
            && site.unloaded_static_call_pcs.contains(&r.callee_pc)
        {
            tables.unloaded_static_call_pcs.insert(base + r.callee_pc);
        }
        tables.invoke_info.insert(
            base + r.callee_pc,
            (r.info_addr, r.num_jit_args, r.return_type),
        );
        // A surviving call the caller's own-site ladder would have bound to a
        // thin VM-side helper gets that helper here too.
        //
        // The ladder in `try_compile_inner` walks only the COMPILING method's
        // invoke pcs, so a helper-eligible call that reached this graph by
        // being spliced in was lowered as the MIC/PIC cascade -- and the
        // cascade's miss path is `jit_invoke_virtual_mic` -> the generic native
        // funnel, every call. That was the whole of the AQS case: AQS
        // `compareAndSetState` is spliced into `tryReleaseShared`, so the hot
        // `Unsafe.compareAndSetInt` sat in a spliced body and the helper bound
        // in `compareAndSetState`'s own (never executed) body served 0 calls
        // (`CRATONVM_DBG=jit-method-stats`: `singlepass=2 optimizing=2
        // served=0 declined=0`). `ir_lower` consults `direct_calls` before
        // `ic_slots`, so the row below is all the lowering needs; the site
        // takes no inline cache, which it would never have used.
        let thin = site
            .invoke_targets
            .iter()
            .find(|(pc, t)| {
                *pc == r.callee_pc
                    && (matches!(t.invoke_kind, 0 | 1)
                        // JIT round 16 (lane callbind): a static box call.
                        || (t.invoke_kind == 3 && ir_splice_box_thin_enabled()))
            })
            // JIT round 21 (lane tl2): the `ThreadLocal` helpers decline by a
            // VIRTUAL re-dispatch, so they stand in for virtual sites only (the
            // own-site ladders' `invoke_kind == 0` / `is_virtual`), never for
            // an `invokespecial` (`super.get()` in an overriding subclass).
            .filter(|(_, t)| {
                t.invoke_kind == 0
                    || !thread_local_thin_row(
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str(),
                    )
            })
            // `Object.clone()` the other way round: `super.clone()` only, never
            // a virtual call of the triple (the receiver's override wins).
            .filter(|(_, t)| {
                t.invoke_kind == 1
                    || !object_clone_row(
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str(),
                    )
            })
            .and_then(|(_, t)| {
                thin_helper_entry(
                    t.class_name.as_str(),
                    t.method_name.as_str(),
                    t.descriptor.as_str(),
                )
            });
        if let Some(entry) = thin {
            // JIT round 21 (lane tlsub, TL321-4 / TL321-1): a `ThreadLocal`
            // row keeps the callee's receiver row too, for the builder's
            // expansion of the site (`ir::ThreadLocalSitePlan`); no inline
            // cache reads it (the site has none).
            if site.invoke_targets.iter().any(|(pc, t)| {
                *pc == r.callee_pc
                    && thread_local_thin_row(
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str(),
                    )
            }) {
                if let Some((_, counts)) = site
                    .callee_receiver_profile
                    .iter()
                    .find(|(pc, _)| *pc == r.callee_pc)
                {
                    tables
                        .spliced_receiver_seeds
                        .insert(base + r.callee_pc, counts.clone());
                }
            }
            direct_call_sites.push((base + r.callee_pc, (entry, true)));
            continue;
        }
        // `invoke_kind` lives on the resolver-side target, not on the interned
        // row; 0 = virtual, 2 = interface are the two the inline-cache cascade
        // serves.
        // JIT round 23 wave 4 (lane L1, CF23-1): an unbound statically bound
        // instance row takes the bound-site pair (`ir_splice_unbound_call_takes_ic`).
        if r.direct_entry == 0
            && site
                .invoke_targets
                .iter()
                .any(|(pc, t)| *pc == r.callee_pc && ir_splice_unbound_call_takes_ic(t))
        {
            virtual_call_sites.push((base + r.callee_pc, r.num_jit_args));
            continue;
        }
        if site
            .invoke_targets
            .iter()
            .any(|(pc, t)| *pc == r.callee_pc && matches!(t.invoke_kind, 0 | 2))
        {
            virtual_call_sites.push((base + r.callee_pc, r.num_jit_args));
            // JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2): the
            // callee's own receiver row for this bci, re-keyed at the call's
            // combined pc, for the planner's inline-cache seed. Rides in
            // `tables`, so a rolled-back body takes its rows with it.
            if let Some((_, counts)) = site
                .callee_receiver_profile
                .iter()
                .find(|(pc, _)| *pc == r.callee_pc)
            {
                tables
                    .spliced_receiver_seeds
                    .insert(base + r.callee_pc, counts.clone());
            }
        }
        // A STATICALLY BOUND surviving call gets the raw `CALL` the caller's
        // own sites get.
        //
        // This row is what was missing, and its absence was not neutral. The
        // resolver binds `direct_entry` for every `invokestatic` /
        // `invokespecial` in a spliced body, and `intern_inline_invoke_targets`
        // registers it in `direct_callee_entries` -- the KEEP-ALIVE list, which
        // pins the callee artifact so a baked address cannot dangle. So the
        // compile paid to pin a target for a direct call it then never emitted:
        // `ir_direct_calls` was filled only from the CALLER's own scan loop, the
        // lowerer's `self.direct_calls.get(&pc)` missed at every spliced pc, and
        // the call fell through to `jit_invoke_dispatch` -- a blind NAME
        // RESOLUTION, not a call.
        //
        // `resolve_inline_site_from` exempts IR mode from the single-pass side's
        // "a spliced call must not be worse than the call it replaced" refusal,
        // on the stated grounds that the IR tier's spliced call "is a call, not
        // a resolution". That was the intent; it was not what the code did.
        // Measured on `SpliceCallProbe`, whose spliced `mid` leaves behind a
        // call to a `tableswitch`-bearing `pick` no resolver will splice:
        // 53 ms running the single-pass body against ~455 ms running the
        // optimizing one, checksums matching Temurin JDK 25 throughout. With
        // the rows produced the optimizing body is at parity (~57 ms).
        // Splicing a frame away while downgrading the calls inside it to
        // resolutions is a net loss -- the same conclusion, and the same
        // ~175 ns per resolution, the single-pass side measured in 2026-08.
        //
        // Virtual and interface kinds are NOT here: they select on the runtime
        // receiver, the resolver offers them no direct bind, and the row above
        // already routes them to the inline-cache cascade.
        if r.direct_entry == 0 {
            if let Some((_, target)) = site.invoke_targets.iter().find(|(pc, target)| {
                *pc == r.callee_pc
                    && ir_splice_static_cell_candidate(target)
                    && cycle_cell_admitter.is_some_and(|admit| admit(target))
            }) {
                let cell = JitCycleEdgeCell::new_any_abi(
                    &target.class_name,
                    &target.method_name,
                    &target.descriptor,
                    cratonvm_types::ClassId::new(target.target_class_id),
                );
                cycle_cell_sites_out.push((base + r.callee_pc, cell));
            }
        }
        if ir_splice_direct_call_enabled() && ir_direct_calls_enabled() && r.direct_entry != 0 {
            // The GPU gate the top-level planner applies, applied here for the
            // same reason: a raw `CALL` to a kernel's entry bypasses
            // `jit_invoke_dispatch`, and that helper is where the offload hook
            // lives. Binding one directly would compile the caller and silently
            // end offload. Unarmed, `keeps_dispatch_helper` is one relaxed bool.
            let keeps_helper = site.invoke_targets.iter().any(|(pc, t)| {
                *pc == r.callee_pc
                    && crate::offload_hook::keeps_dispatch_helper(
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str(),
                    )
            });
            if !keeps_helper {
                direct_call_sites
                    .push((base + r.callee_pc, (r.direct_entry, r.direct_needs_context)));
            }
        }
    }
    Ok(())
}

/// May a surviving statically-bound call inside a SPLICED body be bound to the
/// callee's entry with a raw `CALL`? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_DIRECT_CALL=0` restores the dispatch-helper
/// fallback, which is what every such site got before 2026-09-09.
///
/// The off arm is not so much a safety net as the measurement's A arm: what it
/// restores is `jit_invoke_dispatch`, which resolves the callee by NAME on
/// every call. Keep it, because "did the splice make this slower?" has to stay
/// a question one binary can answer.
/// `CRATONVM_JIT_IR_SPLICE_REFUSE_UNBINDABLE=0` — splice a callee even when a
/// statically-bound call in its body has no `direct_entry`, i.e. re-admit the
/// trade where a splice replaces a bound `CALL` with a blind name resolution.
///
/// On by default: see the refusal in `append_ir_inline_site` for the measured
/// case (`Objects.checkIndex` spliced into `ArrayList.get`, leaving the
/// native-shadowed `Preconditions.checkIndex` unbindable). The off switch
/// exists so the cost of the trade can be re-measured rather than argued.
pub fn ir_splice_refuse_unbindable_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_BOX_THIN` (JIT round 16, lane callbind), **default
/// OFF** (not measured). A spliced body's surviving `Integer.valueOf(I)` /
/// `Long.valueOf(J)` call is bound to the thin helper the compiling method's own
/// sites get (`jit_integer_value_of_direct` / `jit_long_value_of_direct`),
/// instead of refusing the splice as `ir-splice-unbindable-surviving-call`:
/// both are native-shadowed, so the resolver's binder declines them, and
/// before this a body that boxes once could not be spliced at all
/// (`R16OsrdoorParts.run` refuses `get(I)J`, `put(I)J`, `pair(I)J`). The
/// valueOf inline prefixes in `ir_lower` then apply at the spliced site too.
///
/// Only for a LOOP-FREE body (wave b): admitting loop-bearing bodies moved
/// `R16OsrdoorParts`' hot loops into the `run` dispatcher and made `get-*`
/// 3.8x slower on Linux w5a (see `append_ir_inline_site`).
///
/// Only the append's refusal knows this: the planner's prune
/// (`ir_splice_nested_entry_refused`) still treats a NESTED body with such a
/// call as refused and keeps that body its call, the conservative direction.
/// Read per row, so it adds no process global.
pub(crate) fn ir_splice_box_thin_enabled() -> bool {
    true
}

/// May [`ir_splice_box_thin_enabled`] admit a spliced body whose surviving box
/// call would otherwise refuse it? Only under the switch, and only a body with
/// no loop (wave b; see the admission in `append_ir_inline_site`).
pub(crate) fn ir_splice_box_thin_admits_body(code: &[u8], code_len: usize) -> bool {
    ir_splice_box_thin_enabled() && bytecode_loop_ranges(code, code_len).is_empty()
}

/// The [`DirectHelperTable`] cell of the thin box helper for a spliced call,
/// or `None`: the switch off, or any other triple. The caller asks
/// `direct_native_helper` the policy question, exactly as the own-site ladder
/// does.
///
/// * the static `Integer.valueOf(I)` / `Long.valueOf(J)` (the latter under the
///   own-site ladder's `long_box_direct_helpers_enabled` too), only when
///   `box_calls` -- the planner's answer to "may this site's tree take a box
///   call at all" ([`ir_splice_tree_has_loop`]), because binding one is what
///   lets a body that boxes be spliced;
/// * the virtual `Integer.intValue()I` (wave c, under `int_value_direct_enabled`
///   as at the own-site door), whatever `box_calls` says: binding it decides
///   nothing about what is spliced (a virtual row is never refused), it only
///   replaces the inline-cache call whose target is the registered native, the
///   generic funnel per unbox. A spliced `intValue` has no unbox intrinsic
///   (that table is filled from the compiling method's own sites).
pub(crate) fn ir_splice_box_thin_helper_cell(
    helpers: &DirectHelperTable,
    class: &str,
    method: &str,
    descriptor: &str,
    box_calls: bool,
) -> Option<usize> {
    if !ir_splice_box_thin_enabled() {
        return None;
    }
    match (class, method, descriptor) {
        ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") if box_calls => {
            Some(helpers.integer_value_of)
        }
        ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;")
            if box_calls && long_box_direct_helpers_enabled() =>
        {
            Some(helpers.long_value_of)
        }
        ("java/lang/Integer", "intValue", "()I") if int_value_direct_enabled() => {
            Some(helpers.integer_int_value)
        }
        ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;") => {
            Some(helpers.thread_current_thread)
        }
        ("java/lang/Thread", "currentCarrierThread", "()Ljava/lang/Thread;")
            if direct_helpers::carrier_thread_direct_enabled() =>
        {
            Some(helpers.thread_current_carrier_thread)
        }
        ("java/lang/System", "nanoTime", "()J") if helpers.system_nano_time != 0 => {
            Some(helpers.system_nano_time)
        }
        ("java/lang/System", "currentTimeMillis", "()J")
            if helpers.system_current_time_millis != 0 =>
        {
            Some(helpers.system_current_time_millis)
        }
        _ => None,
    }
}

/// Does any body of this site's splice tree contain a loop (JIT round 16,
/// lane callbind, wave c)? `CRATONVM_JIT_IR_SPLICE_BOX_THIN` binds a spliced
/// box call only in a tree with none. The wave-b gate looked only at the body
/// holding the box call: on `R16CallbindBox` the loop-free `box(I)` became
/// spliceable, which made its caller `boxHelper(I)J` -- a LOOP -- spliceable
/// into the `run(II)J` dispatcher, where the loop's `intValue()` lost the
/// unbox intrinsic of `boxHelper`'s own compile (Linux w6a: `boxhelper-full`
/// 16 -> 195 ns/op).
pub(crate) fn ir_splice_tree_has_loop(site: &InlineSite) -> bool {
    !bytecode_loop_ranges(&site.callee_code, site.callee_code_len).is_empty()
        || site
            .nested_sites
            .iter()
            .any(|n| ir_splice_tree_has_loop(&n.site))
}

/// `CRATONVM_JIT_IR_VH_FIELD_FOLD`, default ON: fold `VarHandle.get*` over a
/// `static final` handle on an instance field into a guarded field load in the
/// optimizing tier (`UnboxOp::FieldInt` / `FieldLong`). `=0` restores the thin
/// native-helper route. Read once per compile, no `static` (j27 LATCHES); it
/// only selects between two correct lowerings.
pub(crate) fn ir_vh_field_fold_enabled() -> bool {
    true
}

/// Is `class.method(descriptor)` a `VarHandle` plain/volatile/acquire/opaque
/// READ of one `int` or `long` through a single typed receiver argument?
/// `Some(true)` for a `long` result.
///
/// The receiver argument must be named by a class that no array is assignable
/// to (`Object`, `Cloneable` and `Serializable` are not): the folded load
/// guards the receiver's header class id, and an array's header carries its
/// COMPONENT's id, so the verifier's typing of the argument is what keeps an
/// array from reaching the guard.
pub(crate) fn ir_vh_field_read_candidate(
    class: &str,
    method: &str,
    descriptor: &str,
) -> Option<bool> {
    if class != "java/lang/invoke/VarHandle"
        || !matches!(method, "get" | "getVolatile" | "getAcquire" | "getOpaque")
    {
        return None;
    }
    let rest = descriptor.strip_prefix("(L")?;
    let (name, tail) = rest.split_once(';')?;
    if name.is_empty()
        || name.contains('[')
        || matches!(
            name,
            "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
        )
    {
        return None;
    }
    match tail {
        ")I" => Some(false),
        ")J" => Some(true),
        _ => None,
    }
}

/// Mark the `VarHandle` read sites a spliced body left behind as fold
/// candidates (see [`ir_vh_field_read_candidate`]) and resolve the constant
/// handles its `getstatic` rows read. Returns how many candidate rows it added.
///
/// Unlike an own-site candidate the spliced pc KEEPS its `invoke_info` row:
/// `IrBuilder::try_emit_vh_field_read` declines inside a splice wherever the
/// guard could not resume, and the call is the fallback.
fn collect_static_final_prims<'a>(
    entries: impl Iterator<Item = &'a (u32, usize, u8, bool)>,
    resolve_prim: &dyn Fn(u32, u32, u8) -> Option<i64>,
) -> Vec<((u32, u32), (u8, i64))> {
    entries
        .filter(|&&(_, _, type_tag, is_volatile)| !is_volatile && !matches!(type_tag, b'L' | b'['))
        .filter_map(|&(class_id, field_index, type_tag, _)| {
            let fi = u32::try_from(field_index).ok()?;
            resolve_prim(class_id, fi, type_tag).map(|bits| ((class_id, fi), (type_tag, bits)))
        })
        .collect()
}

pub(crate) fn ir_plan_root_static_final_prims(
    req: &CompileRequest<'_>,
    sm: &HashMap<usize, (u32, usize, u8, bool)>,
    builder: &mut ir::IrBuilder,
) {
    let Some(resolve_prim) = req.static_final_prim_resolver else {
        return;
    };
    let prims = collect_static_final_prims(sm.values(), resolve_prim);
    if !prims.is_empty() {
        builder.add_static_final_prims_from_sites(prims);
    }
}

pub(crate) fn ir_plan_spliced_vh_field_reads(
    req: &CompileRequest<'_>,
    tables: &mut ir::IrInlineTables,
    code_len: usize,
    builder: &mut ir::IrBuilder,
) -> usize {
    if let Some(resolve_prim) = req.static_final_prim_resolver {
        let prims = collect_static_final_prims(
            tables
                .static_field_info
                .iter()
                .filter(|&(&pc, _)| pc >= code_len)
                .map(|(_, entry)| entry),
            resolve_prim,
        );
        if !prims.is_empty() {
            builder.add_static_final_prims_from_sites(prims);
        }
    }
    if !ir_vh_field_fold_enabled() {
        return 0;
    }
    let Some(resolve_vh) = req.static_varhandle_resolver else {
        return 0;
    };
    let plans: Vec<((u32, u32), StaticVarHandleField)> = tables
        .static_field_info
        .iter()
        .filter(|&(&pc, &(_, _, _, _))| pc >= code_len)
        .filter(|&(_, &(_, _, type_tag, _))| type_tag == b'L')
        .filter_map(|(_, &(class_id, field_index, _, _))| {
            let field_index = u32::try_from(field_index).ok()?;
            resolve_vh(class_id, field_index).map(|plan| ((class_id, field_index), plan))
        })
        .collect();
    if plans.is_empty() {
        return 0;
    }
    builder.add_vh_field_plans(plans);
    let mut rows: Vec<(usize, (ir::UnboxOp, u32))> = Vec::new();
    for (&pc, &(info_addr, _, _)) in &tables.invoke_info {
        if pc < code_len || info_addr == 0 {
            continue;
        }
        // SAFETY: every `invoke_info` row of the plan names a live
        // `JitInvokeInfo` (see `ir_plan_spliced_unbox_intrinsics`).
        let info: &JitInvokeInfo = unsafe { &*(info_addr as *const JitInvokeInfo) };
        if !matches!(info.invoke_kind, 0 | 1) {
            continue;
        }
        if let Some(wide) =
            ir_vh_field_read_candidate(info.class_name, info.method_name, info.descriptor)
        {
            let candidate = if wide {
                ir::UnboxOp::FieldLong(0)
            } else {
                ir::UnboxOp::FieldInt(0)
            };
            rows.push((pc, (candidate, 0)));
        }
    }
    let added = rows.len();
    tables.unbox_intrinsics.extend(rows);
    added
}

/// Mark the `ScopedMemoryAccess` accessor calls a spliced body left behind as
/// raw-access candidates ([`ir::try_ir_scoped_unbox`]). The pc keeps its
/// `invoke_info` row, the helper-bound call, as the fallback
/// ([`ir::IrBuilder::try_emit_scoped_raw`] declines where its guards could not
/// resume). Returns how many rows it added.
pub(crate) fn ir_plan_spliced_scoped_raw(
    tables: &mut ir::IrInlineTables,
    code_len: usize,
) -> usize {
    if !ir::ir_scoped_fold_enabled() {
        return 0;
    }
    let mut rows: Vec<(usize, (ir::UnboxOp, u32))> = Vec::new();
    for (&pc, &(info_addr, _, _)) in &tables.invoke_info {
        if pc < code_len || info_addr == 0 {
            continue;
        }
        // SAFETY: every `invoke_info` row of the plan names a live
        // `JitInvokeInfo` (see `ir_plan_spliced_unbox_intrinsics`).
        let info: &JitInvokeInfo = unsafe { &*(info_addr as *const JitInvokeInfo) };
        if !matches!(info.invoke_kind, 0 | 1) {
            continue;
        }
        if let Some(uop) =
            ir::try_ir_scoped_unbox(info.class_name, info.method_name, info.descriptor)
        {
            rows.push((pc, (uop, 0)));
        }
    }
    let added = rows.len();
    tables.unbox_intrinsics.extend(rows);
    added
}

/// Round nio20 wave 6 (lane L1): the `(class_id, field_index)` rows of a
/// `getstatic` table (`pc -> (class, field, tag, volatile)`, the compiling
/// method's own or a spliced body's) whose read the lowering serves as the
/// DIRECT load, for [`ir::IrBuilder::add_plain_static_reads`] /
/// `ir::Graph::plain_static_reads`. The lowering's own test, asked the same
/// way (`ir_lower::Lowerer::emit_inline_getstatic`): the inline route on
/// (`x64::inline_getstatic_enabled`), a tag it emits, and
/// `DirectHelperTable::resolve_static_base` answering (the declaring class
/// initialised now -- monotone -- and not `java/lang/System`). Volatile reads
/// are never listed. Empty under `CRATONVM_JIT_IR_PLAIN_STATIC_READ=0`.
pub(crate) fn ir_plain_static_read_rows<'a>(
    direct_helpers: &DirectHelperTable,
    rows: impl IntoIterator<Item = &'a (u32, usize, u8, bool)>,
) -> Vec<(u32, u32)> {
    if !ir::ir_plain_static_read_enabled() || !x64::inline_getstatic_enabled() {
        return Vec::new();
    }
    let mut out: Vec<(u32, u32)> = rows
        .into_iter()
        .filter(|&&(_, _, tag, is_volatile)| {
            let emitted = b"IZBCSL[JDF".contains(&tag);
            !is_volatile && emitted
        })
        .filter_map(|&(class_id, field_index, _, _)| {
            // The lowering's cell arithmetic (`static_cell_offsets`) refuses
            // only an index past any real class; stay well inside it.
            if class_id == 0 || field_index >= (1 << 20) {
                return None;
            }
            direct_helpers.resolve_static_base(class_id, field_index)?;
            Some((class_id, u32::try_from(field_index).ok()?))
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// t1misc (page `t1-staticfwd-proposal-spliced-static-classes-initialized`): the
/// declaring classes of a spliced body's static rows (`pc -> (class, field,
/// tag, volatile)`) for which `DirectHelperTable::resolve_static_base` answers,
/// for [`ir::IrBuilder::add_spliced_static_initialized_classes`]. The caller's
/// own rows pass the same test (the `set_static_initialized_classes` feed
/// above): the class is initialised now -- monotone -- so a `getstatic` /
/// `putstatic` of it runs no `<clinit>` and need not clear the static memo.
/// Empty unless a memo is on (`ir_static_load_forward_enabled` /
/// `ir_static_store_forward_enabled`) AND `CRATONVM_JIT_IR_STATIC_MEMO_IN_SPLICES`
/// is (the shape the page describes: the first read of another class's static
/// inside a splice wiped every entry). Volatile rows are skipped (the scan
/// clears on a volatile access whatever its class).
pub(crate) fn ir_spliced_initialized_static_classes<'a>(
    direct_helpers: &DirectHelperTable,
    rows: impl IntoIterator<Item = &'a (u32, usize, u8, bool)>,
) -> std::collections::HashSet<u32> {
    if !(ir::ir_static_load_forward_enabled() || ir::ir_static_store_forward_enabled())
        || !ir::ir_static_memo_in_splices_enabled()
    {
        return std::collections::HashSet::new();
    }
    rows.into_iter()
        .filter(|&&(class_id, field_index, _, is_volatile)| {
            !is_volatile
                && class_id != 0
                && field_index < (1 << 20)
                && direct_helpers
                    .resolve_static_base(class_id, field_index)
                    .is_some()
        })
        .map(|&(class_id, _, _, _)| class_id)
        .collect()
}

/// `CRATONVM_JIT_IR_BUFFER_TRAP_MISS` (round nio20 wave 6, lane L1;
/// `r20-buffer-loop-barriers-left-after-the-raw-class-CLOSED-20261012.md` item 1) --
/// **default ON**, `=0` is the kill switch: a profile-guarded splice of a
/// `ByteBuffer` access (`b.putInt(x)` on a `ByteBuffer` whose profile names
/// `HeapByteBuffer` alone) takes a DEOPT as its miss edge instead of the
/// virtual call, as `CRATONVM_JIT_IR_FACTORY_TRAP_MISS` does for factory
/// sites. The general `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS` (default OFF)
/// restricted to the `java.nio` byte-buffer family, under the general rule's
/// evidence bar (`ir::profile_admits_trap_miss`: one class over
/// `MIN_GUARDED_RECEIVER_EVIDENCE` exact samples) and its de-spec round trip
/// (`(bci, UncommonTrap)`, asked before the site is registered: a trap that
/// keeps missing is withdrawn and the recompile takes the call edge).
///
/// Why: with the call as the miss edge every `put*` / `get*` of a buffer loop
/// carries an `Op::Call` (`MemEffect::OPAQUE`) -- the builder's field memo
/// clears at it, `loop_field_forward` refuses the loop, LICM sees a hard
/// barrier -- and the result is a φ of the body's and the call's. With the
/// trap the receiver is EXACT past the test (later sites on it splice with no
/// test until the next join), and `ir_optimize::plant_length_predicates` moves
/// a loop-invariant receiver's test to the pre-header. Read once per compile.
pub(crate) fn ir_buffer_trap_miss_enabled() -> bool {
    ir::ir_guarded_receiver_splice_enabled()
}

/// Is a guarded splice at a call naming `invoked_class` whose spliced body is
/// `body_class`'s a `java.nio` byte-buffer access ([`ir_buffer_trap_miss_enabled`])?
/// The invoked class is the verifier type the site was compiled against
/// (`ByteBuffer` / `MappedByteBuffer`), the body a `java.nio` byte-buffer
/// implementation (`HeapByteBuffer`, `DirectByteBuffer`, their read-only
/// twins). Both live in `java.base`, which only the bootstrap loader defines.
/// `CharBuffer` and the typed views are left to their own rows.
pub(crate) fn ir_buffer_trap_site(invoked_class: &str, body_class: &str) -> bool {
    matches!(
        invoked_class,
        "java/nio/ByteBuffer" | "java/nio/MappedByteBuffer"
    ) && matches!(
        body_class,
        "java/nio/HeapByteBuffer"
            | "java/nio/HeapByteBufferR"
            | "java/nio/DirectByteBuffer"
            | "java/nio/DirectByteBufferR"
    )
}

/// `CRATONVM_JIT_IR_BUFFER_TRAP_LONE_PUT_CALL` (nio21 wave 3, lane L3; page
/// `docs/feature-designs/jit-ir-optimizations.md`) -- **default
/// ON**, `=0` restores the nio20 wave-6 rule exactly: a compile whose ONLY
/// planned byte-buffer site ([`ir_buffer_trap_site`]) is a `put*` keeps the
/// call as that site's miss edge instead of the trap.
///
/// Why: the trap form buys two things, and such a site gets neither. (1) The
/// receiver is exact past the test for LATER sites on it -- there are none.
/// (2) The miss call's `Op::Call` barrier leaves the loop -- but a put's own
/// scoped fold (`UnboxOp::ScopedPut`) is an op-level `Opaque` write, so the
/// loop keeps a hard barrier anyway and LICM / `loop_field_forward` gain
/// nothing. What remains is whatever the trap form costs on its own, which
/// the host measured on `Nio20BufferTyped heapput` (one `putInt(i, x)` per
/// iteration, nothing else): 204 ns/op with the trap, 161 with
/// `CRATONVM_JIT_IR_BUFFER_TRAP_MISS=0`, 160 before nio20 wave 6. A lone GET
/// keeps the trap (its loop does lose the barrier; `heapget` measured equal
/// in both arms). Read once per compile.
pub(crate) fn ir_buffer_trap_lone_put_call_enabled() -> bool {
    true
}

/// May the planned byte-buffer site at `pc` take the trap form, given every
/// planned byte-buffer site of the compile (`sites`) and the `put*` ones among
/// them (`puts`)? Everything but a lone put does, and a lone put does too with
/// [`ir_buffer_trap_lone_put_call_enabled`] off. A `pc` not in `sites` never
/// does.
pub(crate) fn ir_buffer_trap_admits_site(
    sites: &std::collections::HashSet<usize>,
    puts: &std::collections::HashSet<usize>,
    pc: usize,
    lone_put_call: bool,
) -> bool {
    sites.contains(&pc) && !(lone_put_call && sites.len() == 1 && puts.contains(&pc))
}

/// `CRATONVM_JIT_IR_SPLICE_UNBOX_INTRINSIC` (JIT round 16 wave 7, lane
/// callbind), **default ON (flipped; `=0` kills)** (since the orchestrator's A/B): a
/// surviving `Integer.intValue()I` / `Long.longValue()J` call inside a spliced
/// body is lowered as the guarded field load the compiling method's OWN sites
/// get (`ir_unbox_intrinsic_sites`, `IrBuilder::try_emit_unbox_intrinsic`),
/// instead of an inline-cache call whose target is the registered native --
/// the generic native funnel, once per unbox
/// (`r16-callbind-a-spliced-intvalue-is-a-native-funnel-call-FIXED-20261001.md`).
/// The site keeps its call row as the fallback: the builder takes the load
/// only where a chain snapshot resumes its guard. Read per compile, so it
/// adds no process global.
pub(crate) fn ir_splice_unbox_intrinsic_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_SCALAR_INTRINSIC` (nio21 wave 1, lane L5, proposal
/// L3W6-1; **default ON**, `=0` off): a statically bound call a SPLICED body
/// leaves behind that [`ir::try_ir_scalar_intrinsic`] names (the surrogate
/// range tests, `Integer.min`/`max`, `numberOfLeadingZeros`, ...) is lowered
/// as the same `Op::ScalarIntrinsic` the compiling method's own sites get,
/// instead of refusing the whole body (`ir-splice-unbindable-surviving-call`:
/// several of those families are registered natives with no direct entry).
/// `Surrogate$Parser.parse` inside `UTF_8$Encoder.encodeBufferLoop` is the
/// measured shape. Read per compile, so it adds no process global.
pub fn ir_splice_scalar_intrinsic_enabled() -> bool {
    true
}

/// nio21 wave 1, lane L5: the scalar op a surviving STATIC call row of a
/// spliced body lowers as, or `None`. Static (`invoke_kind == 3`) only -- every
/// [`ir::ScalarOp`] family is declared `static` -- and only for an
/// `int`-shaped descriptor (no `F` / `D`; `J` only for `Long.compress`/`expand` while
/// `CRATONVM_JIT_IR_LONG` is on), so the row can never put a
/// long or floating-point node into a graph whose value tier is off (the own
/// sites are gated by the method's admission; a splice is not). One predicate
/// for the planner's two unbindable-call refusals and the post-plan pass
/// ([`ir_plan_spliced_scalar_intrinsics`]), so a row exempted from the refusal
/// is always given its scalar lowering.
pub(crate) fn ir_spliced_scalar_row(
    class: &str,
    method: &str,
    descriptor: &str,
    invoke_kind: u8,
) -> Option<ir::ScalarOp> {
    if invoke_kind != 3
        || !ir_splice_scalar_intrinsic_enabled()
        || descriptor
            .bytes()
            .any(|b| matches!(b, b'F' | b'D' | b'L' | b'['))
    {
        return None;
    }
    let sop = ir::try_ir_scalar_intrinsic(class, method, descriptor)?;
    // A long-shaped row is only the PEXT/PDEP pair, and only while the long tier is
    // on (`CRATONVM_JIT_IR_LONG`, the gate every own site is admitted by).
    let wide = descriptor.bytes().any(|b| b == b'J');
    (!wide
        || (matches!(
            sop,
            ir::ScalarOp::CompressL | ir::ScalarOp::ExpandL | ir::ScalarOp::ReverseBytesL
        ) && true))
        .then_some(sop)
}

/// `CRATONVM_JIT_IR_SPLICE_SCALAR_ROW_BODY` (nio21 wave 7, lane L4;
/// `r21-compare-to-ci-stays-a-call-on-the-character-case-natives-CLOSED-20261012.md`)
/// -- **default ON**, `=0` restores the VM resolver's
/// `static-intrinsic-keeps-its-own-fast-path` refusal for every body that
/// calls a static intrinsic. Read per question, so it adds no process global.
pub fn ir_splice_scalar_row_body_enabled() -> bool {
    true
}

/// Does an optimizing-tier splice lower the static call `(class, method,
/// descriptor)` inside a spliced body as arithmetic (an `Op::ScalarIntrinsic`,
/// [`ir_plan_spliced_scalar_intrinsics`]) rather than as a call? The VM's
/// inline resolver asks this before refusing a body for a static intrinsic
/// call (`static-intrinsic-keeps-its-own-fast-path`): that refusal exists
/// because a spliced call used to be bound to the callee's compiled JDK body
/// and so lost the intrinsic, which a scalar row does not. The same predicate
/// as the planner's unbindable-call exemption and the post-plan pass, so a
/// body admitted for it is always given the scalar lowering.
///
/// `StringLatin1.compareToCI([B[B)I`'s `Math.min(II)I` is the measured case:
/// it kept `compareToCI` out of `CaseInsensitiveComparator.compare` and of
/// every `TreeMap` comparison loop.
pub fn ir_splice_lowers_static_scalar_row(class: &str, method: &str, descriptor: &str) -> bool {
    ir_splice_scalar_row_body_enabled()
        && ir_spliced_scalar_row(class, method, descriptor, 3).is_some()
}

/// `CRATONVM_JIT_IR_SPLICE_PAST_ARRAYCOPY` (JIT r25, lane regex rows; perf,
/// **default ON** since R25-FLIP, `=0` kills): may an optimizing-tier splice keep a static
/// `System.arraycopy` call in its body? The VM's inline resolver refuses a
/// body that calls a static intrinsic (`static-intrinsic-keeps-its-own-fast-path`)
/// because a spliced call used to be bound to the callee's compiled JDK body
/// and so lost the intrinsic. `System.arraycopy` does not lose anything in
/// this tier: after the build, every `System.arraycopy` call node -- the
/// caller's own and every one a splice relocated -- binds to the same direct
/// helper (`ir_arraycopy_entry`, in `ir_tier_attempt`) and takes the same
/// inline prefixes (`try_emit_ir_ref_arraycopy_prefix`, the reference copy and,
/// under `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY`, the byte copy). So the
/// refusal only kept `Arrays.copyOfRange([BII)` out of `StringLatin1.newString`
/// (and with it out of every `substring`): the r23-w5 page's wave 9b
/// (`inline-resolve REFUSED java/util/Arrays.copyOfRange([BII)[B depth=1:
/// static-intrinsic-keeps-its-own-fast-path`). Answers `true` exactly when
/// that bind applies: the switch, the descriptor, `CRATONVM_JIT_IR_DIRECT_CALL`
/// and `CRATONVM_JIT_IR_ARRAYCOPY` (`ir_arraycopy_entry`'s own gates, less
/// the per-VM table, which every VM wires). Read per question.
pub fn ir_splice_keeps_arraycopy_bind(class: &str, method: &str, descriptor: &str) -> bool {
    class == "java/lang/System"
        && method == "arraycopy"
        && descriptor == "(Ljava/lang/Object;ILjava/lang/Object;II)V"
        // R25-FLIP: default ON; `=0` is the kill switch.
        && ir_direct_calls_enabled()
        && true
}

/// nio21 wave 1, lane L5: `(combined pc, op)` for every surviving spliced call
/// row [`ir_spliced_scalar_row`] admits, walked over the plan's `invoke_info`
/// rows exactly as [`ir_plan_spliced_unbox_intrinsics`] walks them. The caller
/// hands them to `IrBuilder::add_scalar_intrinsics`; the builder consults the
/// scalar map BEFORE `invoke_info` at an invoke, so the row's interned call is
/// dead whenever this answers.
pub(crate) fn ir_plan_spliced_scalar_intrinsics(
    tables: &ir::IrInlineTables,
    code_len: usize,
) -> Vec<(usize, ir::ScalarOp)> {
    let mut rows: Vec<(usize, ir::ScalarOp)> = Vec::new();
    if !ir_splice_scalar_intrinsic_enabled() {
        return rows;
    }
    for (&pc, &(info_addr, _, _)) in &tables.invoke_info {
        if pc < code_len || info_addr == 0 {
            continue;
        }
        // SAFETY: as in `ir_plan_spliced_unbox_intrinsics`: every
        // `invoke_info` row of the plan names a live, compile-owned
        // `JitInvokeInfo`.
        let info: &JitInvokeInfo = unsafe { &*(info_addr as *const JitInvokeInfo) };
        if let Some(sop) = ir_spliced_scalar_row(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        ) {
            rows.push((pc, sop));
        }
    }
    rows
}

/// The unbox row ([`ir::IrInlineTables::unbox_intrinsics`]) for one surviving
/// call of a spliced body, or `None`. Only the two FINAL box classes: the
/// verifier types the receiver as the constant-pool class, and a final class
/// has no subclass, so the receiver guard is a layout check and nothing else
/// (`ir_lower`'s `value_equivalent`). The guard class is the VM's unique
/// `java/lang/Integer` / `java/lang/Long` (`integer_id` / `long_id`, from
/// `DirectHelperTable::integer_class_id` and `ir_lower::ir_long_class_id`):
/// both live in `java.lang`, which only the bootstrap loader may define. Never
/// class id 0 (`try_ir_unbox_intrinsic` refuses it). Kind 0 (the row stays
/// virtual: `final-devirt` is refused because a registered native shadows the
/// method) or kind 1 (statically bound); never static or interface.
pub(crate) fn ir_spliced_unbox_row(
    class: &str,
    method: &str,
    descriptor: &str,
    invoke_kind: u8,
    integer_id: Option<u32>,
    long_id: Option<u32>,
) -> Option<(ir::UnboxOp, u32)> {
    if !matches!(invoke_kind, 0 | 1) {
        return None;
    }
    let cid = match (class, method, descriptor) {
        ("java/lang/Integer", "intValue", "()I") => integer_id?,
        ("java/lang/Long", "longValue", "()J") => long_id?,
        _ => return None,
    };
    ir::try_ir_unbox_intrinsic(class, method, descriptor, cid).map(|op| (op, cid))
}

/// Fill `tables.unbox_intrinsics` for every surviving spliced call that
/// [`ir_spliced_unbox_row`] admits (JIT round 16 wave 7, lane callbind), under
/// [`ir_splice_unbox_intrinsic_enabled`]. Returns how many rows it added.
///
/// Walks the plan's own `invoke_info` rows -- every call the spliced bodies
/// leave behind, already interned, keyed by combined pc -- so it sees exactly
/// the calls `append_ir_inline_site` kept, nested bodies' included, and none
/// it spliced away. A caller pc (`< code_len`) is never one of them; it is
/// skipped all the same, because the own-site table already owns those.
pub(crate) fn ir_plan_spliced_unbox_intrinsics(
    tables: &mut ir::IrInlineTables,
    code_len: usize,
    integer_id: Option<u32>,
    long_id: Option<u32>,
) -> usize {
    if !ir_splice_unbox_intrinsic_enabled() {
        return 0;
    }
    let mut rows: Vec<(usize, (ir::UnboxOp, u32))> = Vec::new();
    for (&pc, &(info_addr, _, _)) in &tables.invoke_info {
        if pc < code_len || info_addr == 0 {
            continue;
        }
        // SAFETY: every `invoke_info` row of the plan names a live
        // `JitInvokeInfo`: one `intern_inline_invoke_targets` boxed into this
        // compile's `owned_invoke_infos`, or the caller's own row a bounded
        // self copy reuses. Both outlive the compile.
        let info: &JitInvokeInfo = unsafe { &*(info_addr as *const JitInvokeInfo) };
        if let Some(row) = ir_spliced_unbox_row(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            integer_id,
            long_id,
        ) {
            rows.push((pc, row));
        }
    }
    let added = rows.len();
    tables.unbox_intrinsics.extend(rows);
    added
}

pub fn ir_splice_direct_call_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Merge one top-level site's rows into the compile's plan. Split out so the
/// roll-back path has something to NOT call.
pub(crate) fn merge_ir_inline_tables(into: &mut ir::IrInlineTables, from: ir::IrInlineTables) {
    // DESTRUCTURED, not field-by-field on `from`. A row added to
    // `IrInlineTables` and forgotten here does not fail to compile — it
    // silently reaches the builder empty, and the builder then bails the whole
    // METHOD at the first spliced site that needed it, which reads from the
    // outside exactly like a workload with no such site. That is not
    // hypothetical: it is how the `ldc` rows behaved for their first hour of
    // existence, with the scanner admitting the callee and the builder refusing
    // it 65 bytes later. The pattern below makes the next such row a
    // compile error.
    let ir::IrInlineTables {
        original_body_codes,
        original_body_handler_rows,
        unloaded_static_call_pcs,
        sites,
        field_info,
        invoke_info,
        new_info,
        anewarray_info,
        object_init_pcs,
        ldc_info,
        ldc2w_info,
        static_field_info,
        checkcast_info,
        instanceof_info,
        sync_held_sites,
        ldc_self_class_holds_lock,
        field_decl_class,
        final_field_pcs,
        byte_array_field_pcs,
        referent_load_pcs,
        unbox_intrinsics,
        splice_handler_rows,
        indy_trap_sites,
        indy_bridge_sites,
        indy_capture_news,
        lambda_impl_sites,
        spliced_receiver_seeds,
        finalizable_new_classes,
        ldc_ref_string,
        ldc_ref_class,
        ldc_ref_slots,
        splice_getclass_compares,
        spliced_never_taken,
        spliced_never_taken_dead,
        spliced_always_taken,
        spliced_always_taken_dead,
        spliced_pruned_switch_edges,
        spliced_pruned_switch_dead,
        spliced_cold_ranges,
        splice_caller_frames,
    } = from;
    into.splice_caller_frames.extend(splice_caller_frames);
    // JIT r23 wave 4 (lane L1, CF23-2): rows already rebased to combined pcs.
    into.spliced_never_taken.extend(spliced_never_taken);
    into.spliced_never_taken_dead
        .extend(spliced_never_taken_dead);
    into.spliced_always_taken.extend(spliced_always_taken);
    into.spliced_always_taken_dead
        .extend(spliced_always_taken_dead);
    into.spliced_pruned_switch_edges
        .extend(spliced_pruned_switch_edges);
    into.spliced_pruned_switch_dead
        .extend(spliced_pruned_switch_dead);
    // JIT r23 wave 8 (lane W8-L2, CF-W8-3): combined pcs too.
    into.spliced_cold_ranges.extend(spliced_cold_ranges);
    // JIT round 20 wave 19 (lane ySplice, SPLICE20-1): spliced capturing
    // lambda sites, combined pcs.
    into.indy_capture_news.extend(indy_capture_news);
    // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): combined pcs.
    into.lambda_impl_sites.extend(lambda_impl_sites);
    // JIT r20 wave 19 (lane zLdc): combined pcs (and site-keyed slots).
    into.ldc_ref_string.extend(ldc_ref_string);
    into.ldc_ref_class.extend(ldc_ref_class);
    into.ldc_ref_slots.extend(ldc_ref_slots);
    into.splice_getclass_compares
        .extend(splice_getclass_compares);
    // JIT round 20 wave 12 (lane hFin): class ids, not pcs; a union.
    into.finalizable_new_classes.extend(finalizable_new_classes);
    // JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2): combined
    // pcs, so a nested body's rows never collide with its enclosing body's.
    into.spliced_receiver_seeds.extend(spliced_receiver_seeds);
    // nio21 wave 11 (lane L3): spliced constant-word lambda sites.
    into.indy_trap_sites.extend(indy_trap_sites);
    into.indy_bridge_sites.extend(indy_bridge_sites);
    // nio21 wave 4 (lane L2): keyed by body base.
    into.splice_handler_rows.extend(splice_handler_rows);
    // nio21 wave 2 (lane L3).
    into.final_field_pcs.extend(final_field_pcs);
    // nio21 wave 7 (lane L4).
    into.byte_array_field_pcs.extend(byte_array_field_pcs);
    // JIT r20 wave 18 (lane xLib).
    into.referent_load_pcs.extend(referent_load_pcs);
    // JIT round 16 wave 7 (lane callbind): filled after the whole plan
    // (`ir_plan_spliced_unbox_intrinsics`), so empty here today; merged all
    // the same, for the reason the pattern above is a destructuring.
    into.unbox_intrinsics.extend(unbox_intrinsics);
    into.sites.extend(sites);
    into.original_body_codes.extend(original_body_codes);
    into.original_body_handler_rows
        .extend(original_body_handler_rows);
    into.sync_held_sites.extend(sync_held_sites);
    into.ldc_self_class_holds_lock
        .extend(ldc_self_class_holds_lock);
    into.field_info.extend(field_info);
    // Round 15 wave 3 (lane chain3, CHN15-2 producer).
    into.field_decl_class.extend(field_decl_class);
    into.invoke_info.extend(invoke_info);
    into.unloaded_static_call_pcs
        .extend(unloaded_static_call_pcs);
    into.new_info.extend(new_info);
    // W10-6: combined pcs.
    into.anewarray_info.extend(anewarray_info);
    into.object_init_pcs.extend(object_init_pcs);
    into.ldc_info.extend(ldc_info);
    into.ldc2w_info.extend(ldc2w_info);
    into.static_field_info.extend(static_field_info);
    into.checkcast_info.extend(checkcast_info);
    into.instanceof_info.extend(instanceof_info);
}

/// Turn a spliced body's resolver-side [`InlineInvokeTarget`]s into the
/// `*const JitInvokeInfo`s its emitter can bake, interning the names into the
/// arenas `owned_strings` / `owned_invoke_infos` — which `try_compile_inner`
/// moves into the `CompiledMethod`, so the pointees outlive the code that
/// references them and are freed with it.
///
/// Module-scope rather than a closure inside `try_compile_inner` so a test can
/// assert the one property that has no visible symptom: a nested body whose
/// calls are left uninterned degrades silently to a dispatch.
pub(crate) fn intern_inline_invoke_targets(
    site: &mut InlineSite,
    owned_strings: &mut Vec<Box<str>>,
    owned_invoke_infos: &mut Vec<Box<JitInvokeInfo>>,
    direct_callee_entries: &mut Vec<(usize, u64)>,
) {
    // Wholesale, never additive: an `InlineSite` may have been CLONED from
    // a cached plan that already carries pointers from an earlier compile,
    // and those point into an arena this compile does not own.
    site.resolved_invoke_infos.clear();
    for (callee_pc, target) in site.invoke_targets.iter() {
        let class_ref = intern_jit_invoke_str(&target.class_name);
        let method_ref = intern_jit_invoke_str(&target.method_name);
        let desc_ref = intern_jit_invoke_str(&target.descriptor);
        // SAFETY: the three boxed strs were just moved into `owned_strings`,
        // which `try_compile_inner` moves into `compiled._jit_strings`; a
        // `Box<str>`'s payload does not move when the Box does, so these
        // pointers stay valid for as long as the `CompiledMethod` lives —
        // the identical argument the top-level `invoke_info` construction
        // makes a few hundred lines above.
        let info = Box::new(JitInvokeInfo {
            class_name: class_ref,
            method_name: method_ref,
            descriptor: desc_ref,
            num_jit_args: target.num_jit_args,
            return_type: target.return_type,
            invoke_kind: target.invoke_kind,
            declaring_class_id: target.declaring_class_id,
            // The substituted owner's id, when the resolver pinned the site to
            // a declaring class; the helper reads it for kind 1 only.
            owner_class_id: target.owner_class_id,
        });
        let info_ptr: *const JitInvokeInfo = &*info;
        owned_invoke_infos.push(info);
        let (direct_entry, direct_needs_context) = target.direct_entry.unwrap_or((0, false));
        // THE KEEP-ALIVE STEP, and the one whose omission has no symptom until
        // a tier-up. `_direct_callee_entries` is what `prepare_for_publication`
        // pins the callee artifact through, and what the invalidation reverse
        // closure walks to evict this caller when that callee is withdrawn. A
        // spliced direct call bakes the same kind of address a top-level one
        // does and needs the same registration; skipping it leaves a raw `CALL`
        // into freed code.
        if direct_entry != 0 {
            // The owner AT BAKE TIME -- see `_direct_callee_expected`.
            direct_callee_entries.push((direct_entry, jit_entry_artifact_id(direct_entry)));
        }
        site.resolved_invoke_infos.push(ResolvedInlineInvoke {
            callee_pc: *callee_pc,
            // Cast: pointer parked in a Send-able plan; deref only inside this compile
            info_addr: info_ptr as usize,
            direct_entry,
            direct_needs_context,
            num_jit_args: target.num_jit_args,
            return_type: target.return_type,
        });
    }
    // A nested body's calls need the same treatment; the resolver bounds
    // the depth (`MAX_INLINE_NEST_DEPTH`), so this terminates.
    for nested in site.nested_sites.iter_mut() {
        intern_inline_invoke_targets(
            &mut nested.site,
            owned_strings,
            owned_invoke_infos,
            direct_callee_entries,
        );
    }
}

/// Stage the one [`JitInvokeInfo`] that describes THIS method as the callee of
/// its own raw self-recursive `CALL` sites, returning its address (`0` is never
/// returned; the caller decides whether to ask).
///
/// # Why a self-call needs an info at all
///
/// Both self-call planner arms deliberately `continue` past the generic
/// `invoke_info` registration: a row there would route the site to
/// `jit_invoke_dispatch` instead of the raw `CALL` that is the whole point of
/// binding it directly. But `JitInvokeInfo` is also what
/// `emit_inline_callee_deopt_check` hands `jit_service_callee_deopt`, and that
/// service is the ONLY thing at a raw JIT-to-JIT call site that gives the
/// CALLEE's own exception table a look before the callee's `i64::MIN` return is
/// treated as the caller's own unwind.
///
/// Without it a self-recursive method whose body catches what its own recursive
/// call throws never ran that catch in any activation the compiler owned: each
/// one stamped its own throw bci over the callee's and returned the sentinel
/// again, so the exception walked the entire compiled region and was first
/// resolved at the topmost compiled frame — the outermost activation's handler,
/// or wherever the compiled/interpreted boundary happened to fall that run.
/// `regression-suite/probes/R10SelfRecCatch.java` is the witness.
///
/// So the info is staged on its own channel (`BackendRequest::self_call_invoke_info`)
/// rather than in `invoke_info`, which leaves the routing decision untouched.
/// The IR tier reached the same place from the other side: it already had an
/// info in scope at its self-call and simply never passed it to the service
/// (`ir_lower::emit_self_recursive_call`).
///
/// ONE per compile: every self-recursive site in a method calls that same
/// method, so one info describes them all.
pub(crate) fn stage_self_call_invoke_info(
    cached: &CachedBytecodeMethod,
    invoke_kind: u8,
    num_jit_args: usize,
    return_type: u8,
    owned_strings: &mut Vec<Box<str>>,
    owned_invoke_infos: &mut Vec<Box<JitInvokeInfo>>,
) -> usize {
    let class_ref = intern_jit_invoke_str(&cached.class_name.to_string());
    let method_ref = intern_jit_invoke_str(&cached.method_name.to_string());
    let desc_ref = intern_jit_invoke_str(&cached.method_descriptor.to_string());
    let info = Box::new(JitInvokeInfo {
        class_name: class_ref,
        method_name: method_ref,
        descriptor: desc_ref,
        num_jit_args,
        return_type,
        // The REAL dispatch kind (3 static / 1 special), never the IR-only
        // marker `4`: `jit_service_callee_deopt` branches on this to decide
        // whether arg 0 is a receiver, and reading a static call's arg 0 as one
        // resolves the callee from whatever that argument points at.
        invoke_kind,
        declaring_class_id: cached.declaring_class_id.as_u32(),
        owner_class_id: 0,
    });
    let info_ptr: *const JitInvokeInfo = &*info;
    owned_invoke_infos.push(info);
    // Cast: pointer parked as an address so `BackendRequest` keeps its auto
    // traits; dereferenced only inside this compile, by the codegen below.
    info_ptr as usize
}

/// Estimate the native-code expansion charged to the compilation's inline
/// budget. The estimate deliberately stays cheap and deterministic: planning
/// happens before backend emission and must not resolve or compile anything
/// speculatively.
pub fn inline_site_expansion_cost(site: &InlineSite) -> Option<usize> {
    inline_site_expansion_cost_tiered(site, /* site_is_hot */ false)
}

/// Site-hotness-aware form of [`inline_site_expansion_cost`]
/// (jit-inlining-and-ir-calls).
///
/// The cost MODEL is identical — bytecode length plus a backend-oriented charge
/// per field / static-field access and for a context-needing body. What the
/// tier changes is the two ceilings the estimate is judged against:
///
/// | | callee bytecode | expansion cost |
/// |---|---|---|
/// | cold | [`MAX_INLINE_SIZE_COLD`] (35, HotSpot `MaxInlineSize`) | [`MAX_INLINE_EXPANSION_COST`] (64) |
/// | hot | [`MAX_INLINE_BYTECODE_SIZE`] (325, HotSpot `FreqInlineSize`) | [`MAX_INLINE_EXPANSION_COST_HOT`] (512) |
///
/// A callee at or below [`MAX_TRIVIAL_INLINE_SIZE`] (HotSpot `MaxTrivialSize`)
/// bypasses the *size* cap in either tier — it is smaller than the call
/// sequence it replaces — but still pays into the per-method budget and still
/// has to clear its tier's expansion ceiling, so a 6-byte body doing five field
/// loads is not waved through.
///
/// `site_is_hot == false` reproduces the pre-2026-07-26 decision exactly, which
/// is why [`inline_site_expansion_cost`] simply delegates with `false`: an
/// unprofiled compile is bit-for-bit unchanged.
/// Does this body (or anything it splices) contain an FFM ELEMENT accessor?
///
/// A spliced body is emitted by `emit_inline_invoke`, which lowers every invoke
/// it contains to an ordinary dispatch — the single-pass intrinsic ladder does
/// NOT run inside an inlined body. So inlining a method that contains a
/// `MemorySegment.getAtIndex`/`setAtIndex` site silently converts that site from
/// the fast path back into the ~1158 ns/element native dispatch.
///
/// That is exactly the shape the accessors appear in: `ShortArray.get` and
/// `TornadoMemorySegment.getShortAtIndex` are a few bytecodes each, so the
/// inliner takes them every time and the intrinsic would never fire on the path
/// that matters.
///
/// Refusing the splice keeps ONE compiled-to-compiled call per element — a few
/// nanoseconds — in exchange for the accessor keeping its fast path. Checked
/// transitively, because a nested splice is emitted into the same body.
pub fn inline_site_contains_ffm_accessor(site: &InlineSite) -> bool {
    site.invoke_targets.iter().any(|(_, t)| {
        t.class_name == "java/lang/foreign/MemorySegment"
            && ffm_element_accessor_name(&t.method_name)
            && ffm_kind_for_descriptor(&t.descriptor).is_some()
    }) || site
        .nested_sites
        .iter()
        .any(|n| inline_site_contains_ffm_accessor(&n.site))
}

pub fn inline_site_expansion_cost_tiered(site: &InlineSite, site_is_hot: bool) -> Option<usize> {
    if site.callee_code_len == 0 {
        return None;
    }
    // An FFM element accessor loses its fast path the moment it is spliced —
    // see `inline_site_contains_ffm_accessor`. Refuse the body rather than
    // trade ~1158 ns/element for the few nanoseconds the call would have cost.
    if inline_site_contains_ffm_accessor(site) {
        return None;
    }
    if site.class_name == "java/lang/reflect/Method"
        && site.method_name == "invoke"
        && site.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
    {
        return None;
    }
    let size_cap = if site_is_hot {
        MAX_INLINE_BYTECODE_SIZE
    } else {
        MAX_INLINE_SIZE_COLD
    };
    if site.callee_code_len > size_cap && site.callee_code_len > MAX_TRIVIAL_INLINE_SIZE {
        return None;
    }

    let field_cost = site.field_info.len().saturating_mul(6);
    let static_field_cost = site.static_field_info.len().saturating_mul(8);
    let context_cost = usize::from(site.needs_heap).saturating_mul(4);
    // A call the spliced body makes is the single most expensive thing it can
    // contain: `emit_inline_invoke` builds a contiguous argument buffer (a
    // load+store pair per argument), loads four ABI registers, spills before
    // the safepoint, calls, and emits the post-invoke exception check. 12 is
    // the same order as the 8 charged for a static-field access, which is the
    // next-largest single item, and deliberately does NOT try to be exact —
    // the model's job is to keep a budget honest, not to predict bytes.
    //
    // Charged per NON-nested call only. A nested call's cost arrives through
    // `nested_expansion` below, which is the nested body's own estimate; adding
    // both would double-charge the same call site.
    let nested_pcs: Vec<usize> = site.nested_sites.iter().map(|n| n.callee_pc).collect();
    let live_mask = ir_splice_live_pc_mask(site);
    let dispatch_cost = site
        .invoke_targets
        .iter()
        .filter(|(pc, _)| !nested_pcs.contains(pc) && !ir_splice_row_is_prune_dead(&live_mask, *pc))
        .count()
        .saturating_mul(12);
    // A nested splice expands into THIS site's code, so its expansion is this
    // site's expansion. Charged at the nested body's own hot-tier estimate;
    // a nested body the model cannot cost at all (`None` — too large for even
    // the hot cap) is charged the flat cost cap, which reliably pushes the
    // outer site over its own ceiling rather than letting an uncostable body
    // in for free.
    let nested_expansion = site
        .nested_sites
        .iter()
        .map(|nested| {
            // A guarded nested splice also emits the compare, the null check
            // and the miss-edge jump; 8 is the same order the guarded-virtual
            // planner charges one level up.
            let guard_cost = if nested.guard_class_id != 0 { 8 } else { 0 };
            inline_site_expansion_cost_tiered(&nested.site, site_is_hot)
                .unwrap_or(MAX_INLINE_EXPANSION_COST_HOT)
                .saturating_add(guard_cost)
        })
        .fold(0usize, |a, b| a.saturating_add(b));
    let cost = site
        .callee_code_len
        .saturating_add(field_cost)
        .saturating_add(static_field_cost)
        .saturating_add(context_cost)
        .saturating_add(dispatch_cost)
        .saturating_add(nested_expansion);
    let cost_cap = if site_is_hot {
        MAX_INLINE_EXPANSION_COST_HOT
    } else {
        MAX_INLINE_EXPANSION_COST
    };
    (cost <= cost_cap).then_some(cost)
}

// ---------------------------------------------------------------------------
// Profile-guided inlining policy (C2-review P1)
// ---------------------------------------------------------------------------
//
// Everything above this line decides ONE question — "is this callee small
// enough?" — and the caller (`try_compile_inner`'s invoke loop) answered every
// other question implicitly, by never asking it: no depth accounting, no
// recursion accounting, no receiver-type profile, no record of WHY a candidate
// was refused, and no dependency beyond the callee's own class name. This
// section is the policy those decisions were missing. It does not replace the
// size model; it wraps it.
//
// # What is admitted, and what is deliberately refused
//
// `plan_inline` is a pure function: it takes a resolved [`InlineSite`], the
// site's profile evidence, the compilation's remaining budget and a
// description of what the BACKEND can actually emit, and returns an
// [`InlinePlan`]. It never resolves, allocates code, or mutates anything.
//
//   * A statically bound site (`invokestatic` / `invokespecial`) has exactly
//     one possible target, so it is admitted on the pre-existing size/budget
//     terms with no guard and no speculation. Every refusal added here for
//     such a site is one that could not previously fire (see the per-variant
//     notes on [`InlineRefusal`]), so an unprofiled compile inlines exactly as
//     it did before.
//   * A virtual/interface site is SPECULATIVE: the target is chosen from the
//     receiver-type profile, so it is admitted only behind an exact receiver
//     class-id guard AND only with a recorded invalidation dependency. Absent
//     either, the plan refuses. That is the fail-closed rule for this task, and
//     it is enforced in one place ([`plan_inline`]'s speculative arm) rather
//     than at each call site.
//
// # Deopt safety with no caller scopes
//
// `deopt::FrameState::caller` exists but NO production site ever populates it
// — every construction in `ir_lower.rs` and `x64.rs` passes `caller: None`, so
// an inlined scope is not representable in deopt metadata. An inline that
// needed to describe "we are inside callee C, called from caller M at bci N"
// would therefore publish a frame naming M with C's bci: a silent wrong answer
// of exactly the class this branch just closed.
//
// This policy does not rely on caller scopes, and does not need them, because
// the single-pass inline emitter publishes NO deopt point inside a callee
// body. `x64::try_emit_inline_body` now lives in `x64/inlining.rs`, not at the
// `x64.rs:9772-11056` this comment used to cite.
//
// It is NOT true that the splice walk contains no publisher — but the count
// this comment carried was wrong in both directions, and the correction is
// recorded here rather than quietly applied because the wrong list is the kind
// that survives review: every name in it is a real publisher, just not one the
// splice walk can reach.
//
// **TWO publishers are reachable from `x64/inlining.rs`, not four:**
// `emit_post_invoke_exception_check` (its nested-invoke arm) and
// `emit_precise_null_check_field_store`. Wave 7 listed four —
// `emit_post_invoke_exception_check`, `emit_post_alloc_oom_check`,
// `emit_precise_array_npe_check` and the reason-11 bounds arm — and three of
// those four appear in `inlining.rs` only inside COMMENTS; none is called from
// it. The one that was actually missing from the list is
// `emit_precise_null_check_field_store` (`x64/arrays.rs`, reason 10 via
// `build_and_record_deopt_point`), which `inlining.rs` calls twice. So the old
// list over-counted by two and omitted the second real hazard.
// `docs/feature-designs/deopt-osr.md`
// has lane `bybci`'s enumeration; it was re-derived here by grepping
// `inlining.rs` for CALLS rather than mentions, and it holds.
//
// What keeps the published set empty is a compilation-wide INTERLOCK, not an
// absence: both publishers are armed only when `precise_exception_frames` is
// set, and such a compile inlines nothing. The interlock is **four statements,
// not two**, and each is load-bearing:
//
//   1. `plan_inline` refuses every candidate with
//      [`InlineRefusal::PreciseExceptionFrames`];
//   2. `build_single_pass_tables` clears `inline_sites`;
//   3. it clears `inline_guard_variants` too — the guard-variant splice path
//      has been on by default since N8, so clearing only `inline_sites` would
//      have left the interlock resting on (1), a policy decision in another
//      function;
//   4. `x64::Compiler::try_emit_inline_site` refuses outright on
//      `self.precise_exception_frames`, which is the belt to (1)-(3)'s braces
//      and the only one of the four that would still hold if a future edit
//      reached the emitter with a non-empty plan.
// A point that IS published inside a splice is now built to the callee's own
// geometry with every slot `Unsupported`, so it is unresumable rather than
// wrong; see
// `docs/feature-designs/deopt-osr.md`.
// [`InlineRefusal::PreciseExceptionFrames`] states that interlock in the
// policy as well, so the two cannot drift apart silently.
//
// Round nio20 wave 3 (lane L3, W2L2-3): statement 4 is now a per-site
// question, not a blanket refusal, and only the single-pass OSR door
// (`plan_osr_door_splices`) asks it: a site outside every protected range (both
// publishers arm only at a protected pc) or a static body that cannot throw
// ([`inline_site_body_cannot_throw`]) is admitted. Statements 1-3 still hold
// for every method-entry compile.
//
// Round nio20 wave 4 (lane L3, W3L3-1/2): statements 1-3 are per-site too,
// under [`sp_splice_precise_per_site_enabled`] (default ON): the method-entry
// door asks [`sp_entry_precise_interlock_holds`] per site before `plan_inline`
// and replaces the two clears by a per-site `retain`, and the OSR door's
// guarded pass ([`plan_osr_door_guarded_splices`]) plans its unprotected
// sites. What keeps the published set empty is now `may_file_by_bci` plus the
// protected-pc arming of both publishers, not the absence of splices.
//
// The consequence, stated plainly: **an inlined body is only ever entered and
// left within one frame that is described as the caller's own**. No deopt
// point inside it is described at all, so there is no metadata that a missing
// caller scope could make wrong. If a future emitter learns to publish deopt
// points inside inlined callees, this policy must refuse until
// `FrameState::caller` is populated — that is a hard precondition, not a
// preference.

/// Maximum nesting depth of inlined scopes. HotSpot's `MaxInlineLevel`.
///
/// **This comment used to say the single-pass emitter cannot nest.** It can:
/// `try_emit_nested_inline` / `emit_guarded_nested_inline` splice inside a
/// splice, `InlineSite::nested_sites` carries the plan, and
/// [`MAX_INLINE_NEST_DEPTH`] bounds it at 3 — the depth the JUnit assert chain
/// needs to collapse. The sentence about the emitter bailing on any callee
/// invoke that is not a resolver-proven elidable super-`<init>`, and the
/// `depth = 1` that followed from it, describe the tree before nesting landed.
///
/// What is still true is that this constant does not bind: 3 is the live limit
/// and it is the smaller of the two. Kept as HotSpot's `MaxInlineLevel` so a
/// resolver that ever plans deeper inherits a ceiling rather than needing one
/// added.
pub const INLINE_MAX_DEPTH: usize = 9;

/// Maximum number of copies of the SAME method allowed on one inline stack —
/// HotSpot's `MaxRecursiveInlineLevel`. Counts ancestors, not siblings: a leaf
/// called five times from one caller is five independent sites, each paying the
/// per-method budget, and is not recursion.
pub const INLINE_MAX_RECURSIVE_DEPTH: usize = 1;

/// Receiver observations a virtual/interface site needs before its type
/// profile may be speculated on at all.
///
/// Half of [`INLINE_HOT_SITE_OBSERVATIONS`]: a site can be worth SPECULATING on
/// (the shape of its receiver distribution has stabilised) before it is hot
/// enough to earn the `FreqInlineSize` allowance. Under this count the shape is
/// reported as [`ReceiverShape::Cold`] and refused, so a site executed twice
/// with one receiver is never mistaken for a monomorphic one.
pub const INLINE_MIN_SPECULATION_OBSERVATIONS: u32 = 250;

/// Share of observations the dominant receiver must hold for a site to count as
/// monomorphic. Matches the `min_fraction_pct` convention of
/// [`profile::dominant_receiver`], which the interpreter-side consumers already
/// call with 80-90.
pub const INLINE_MONOMORPHIC_SHARE_PCT: u32 = 90;

/// Combined share the top TWO receivers must hold for a bimorphic split. Higher
/// than the monomorphic threshold on purpose: a bimorphic site pays two guards,
/// so the tail it may leave to the dispatch fallback has to be smaller for the
/// second guard to earn its space.
pub const INLINE_BIMORPHIC_SHARE_PCT: u32 = 92;

/// Distinct receiver types above which a site is megamorphic regardless of how
/// dominant its top type looks.
///
/// Twice the PIC cascade's four ways. A site with a 91%-dominant receiver and
/// twenty tail types is not a devirtualisation opportunity: the guard hits, but
/// the profile is describing a shape (a dispatch hub) that the next thousand
/// calls are free to change, and every reshuffle costs a retire/recompile.
pub const INLINE_MEGAMORPHIC_TYPE_CEILING: usize = 8;

/// What a call site's receiver-type profile says about it.
///
/// Derived only from [`profile::MethodProfile::receivers`], which the
/// interpreter really does feed (`record_receiver_borrowed`) for
/// `invokevirtual` / `invokeinterface`. Statically bound sites have no
/// receivers recorded and classify as [`ReceiverShape::Unprofiled`], which is
/// correct and is why the statically bound arm of [`plan_inline`] never
/// consults the shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReceiverShape {
    /// No receiver observation at this bci: profiling was off, the site never
    /// ran, or its invoke kind has no receiver. **Not** the same as "many
    /// types" — see [`profile::CallSiteEvidence`] for the same distinction on
    /// the count side.
    Unprofiled,
    /// Observed, but below [`INLINE_MIN_SPECULATION_OBSERVATIONS`].
    Cold { observations: u32, types: usize },
    /// One receiver class holds at least [`INLINE_MONOMORPHIC_SHARE_PCT`] of
    /// the observations. A tail is allowed: the guard routes it to dispatch.
    Monomorphic { class_id: u32, observations: u32 },
    /// The top two classes together hold at least
    /// [`INLINE_BIMORPHIC_SHARE_PCT`]. `class_ids` is ordered by descending
    /// observation count, ties broken by ascending class id, so the same
    /// profile always yields the same plan.
    Bimorphic {
        class_ids: [u32; 2],
        observations: u32,
    },
    /// Too many types, or no sufficiently dominant one.
    Megamorphic { types: usize, observations: u32 },
    /// At least one counter (or the total) has pinned at [`u32::MAX`], so the
    /// recorded proportions are no longer the observed proportions — see
    /// [`profile::ProfileFidelity`]. A site whose majority class simply
    /// *stopped counting* reads as monomorphic for a reason that has nothing
    /// to do with the program settling, which is exactly the "a truncated or
    /// saturated profile must not read as its dominant type" rule the pgo-02
    /// brief made a verification requirement. Reported apart from
    /// [`Self::Megamorphic`] because the two call for different responses (a
    /// saturated profile is not polymorphic, it is unreadable), but it refuses
    /// just as hard.
    Saturated { types: usize, observations: u32 },
}

/// Why a candidate was not inlined. Every refusal is reported rather than
/// silently dropped, because "the inliner did nothing" and "the inliner
/// declined for a stated reason" are indistinguishable from the outside, and
/// that is precisely the failure mode `flag-census.md` tracks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InlineRefusal {
    /// The compilation requested precise exception frames. Mirrors the
    /// `inline_sites` clear in `build_single_pass_tables`; stated here so the
    /// policy and the interlock cannot drift.
    ///
    /// Since round nio20 wave 4 (lane L3, W3L3-1) the single-pass doors pass
    /// `precise_exception_frames: true` only for a site the per-site interlock
    /// refuses ([`sp_entry_precise_interlock_holds`]: a protected site that is
    /// not an `invokestatic` of a body that cannot throw), and the clear is a
    /// per-site `retain` of the same question; with
    /// [`sp_splice_precise_per_site_enabled`] off this is the compile-wide
    /// refusal it always was.
    ///
    /// This is statement 1 of a FOUR-statement interlock, not one of two — the
    /// other three being that same function's `inline_guard_variants.clear()`
    /// and `x64::Compiler::try_emit_inline_site`'s own refusal on
    /// `self.precise_exception_frames`. What the interlock keeps quiet is TWO
    /// deopt publishers reachable from the splice walk
    /// (`emit_post_invoke_exception_check`'s nested-invoke arm and
    /// `emit_precise_null_check_field_store`), not the four an earlier version of
    /// the comment above `INLINE_MAX_DEPTH` named; see that comment for the
    /// enumeration and for which name was missing from the old list.
    PreciseExceptionFrames,
    /// Inline nesting would exceed [`INLINE_MAX_DEPTH`].
    DepthLimit { depth: usize },
    /// The callee already appears [`INLINE_MAX_RECURSIVE_DEPTH`] times on this
    /// inline stack.
    RecursionLimit { copies: usize },
    /// An invoke kind that has no inline lowering at all (`invokedynamic`).
    UnsupportedInvokeKind { kind: u8 },
    /// A virtual/interface site with no receiver observations.
    NoProfileEvidence,
    /// A virtual/interface site whose receiver profile is too thin to speculate
    /// on.
    ColdSite { observations: u32 },
    /// A virtual/interface site with too many receiver types.
    Megamorphic { types: usize },
    /// A virtual/interface site whose top one or two receivers do not clear
    /// their share thresholds.
    ReceiverNotDominant { types: usize },
    /// A SPECULATIVE site inside a protected range. The guard's miss edge is a
    /// new control-flow edge in the middle of a `try` block, and the inlined
    /// body publishes no exceptional frame of its own; statically bound sites
    /// are unaffected and keep their existing behaviour.
    SpeculationInsideProtectedRange,
    /// The callee failed [`inline_site_expansion_cost_tiered`] — over its
    /// tier's bytecode cap or its expansion cap.
    CalleeTooLarge,
    /// The per-method budget cannot pay for this site.
    BudgetExhausted { cost: usize, remaining: usize },
    /// The backend cannot emit the lowering this verdict requires. For a
    /// speculative site this is today's production answer: the single-pass
    /// backend consults `inline_sites` only at `invokestatic` (x64.rs:17274)
    /// and `invokespecial` (x64.rs:19463), and its plain direct-call arm
    /// (x64.rs:20716) emits no receiver guard at all. Fail closed.
    GuardNotEmittable,
    /// A speculative inline whose receiver class could not be named, so no
    /// invalidation dependency could be recorded for it. Fail closed: a
    /// speculation nothing can retire is worse than no speculation.
    NoInvalidationDependency,
    /// No inlineable body could be resolved for this site.
    ///
    /// `speculated_class_id` is `Some` when the failure is specific to a
    /// SPECULATED receiver class: the guard would admit exactly that class, so
    /// the body behind it must be the one that class dispatches to, and if the
    /// VM cannot hand back that body — or cannot prove the body it found is
    /// the one real dispatch would select — there is nothing safe to splice.
    /// `None` is the statically bound case (the resolver declined the
    /// constant-pool callee: native, oversized, or an ineligible construct).
    CalleeUnresolved { speculated_class_id: Option<u32> },
    /// The per-method expansion budget was already spent before this site was
    /// priced, so it was never resolved. Distinct from
    /// [`Self::BudgetExhausted`], which means "priced, and it did not fit":
    /// this one carries no cost because none was computed.
    BudgetAlreadySpent,
    /// The receiver profile has saturated ([`ReceiverShape::Saturated`]).
    SaturatedProfile { observations: u32 },
    /// The callee is force-interpreted by `CRATONVM_JIT_DENY` /
    /// `CRATONVM_JIT_BISECT_ONLY`.
    ///
    /// Those levers are documented as "a matching method is force-interpreted
    /// (never JIT-compiled)", and until 2026-08-23 they were consulted ONLY by
    /// `compile_gate::admit` — i.e. by the three COMPILE doors. The inline
    /// planner never asked, so a denied callee was still `inline-planned` and
    /// spliced into a compiled caller's body. MEASURED on
    /// `probes/XferProbe2.java`: with
    /// `CRATONVM_JIT_DENY=XferProbe2$Base.calleeSpecial` set,
    /// `CRATONVM_DBG=jitc` printed `inline-planned
    /// XferProbe2$Base.calleeSpecial(I)I @pc=2` and the arm read 24.2 ns/op
    /// against 25.8 undenied — the lever moved nothing because the callee was
    /// still running as compiled code, just not as its own artifact.
    ///
    /// That is not untidiness: it silently invalidates any bisect built on the
    /// lever. A conclusion of the form "the arm stalled with X force-
    /// interpreted, therefore X's compiled code is not the cause" is only
    /// sound if X really was interpreted.
    ForceInterpreted,
}

impl InlineRefusal {
    /// Short stable category name, for metrics keys and log greps. Same
    /// contract as [`bailout::BailoutReason::category`]: these strings are
    /// consumed by dashboards and test assertions and must not be renamed with
    /// the variants.
    pub fn category(&self) -> &'static str {
        match self {
            InlineRefusal::PreciseExceptionFrames => "precise-exception-frames",
            InlineRefusal::DepthLimit { .. } => "depth-limit",
            InlineRefusal::RecursionLimit { .. } => "recursion-limit",
            InlineRefusal::UnsupportedInvokeKind { .. } => "unsupported-invoke-kind",
            InlineRefusal::NoProfileEvidence => "no-profile-evidence",
            InlineRefusal::ColdSite { .. } => "cold-site",
            InlineRefusal::Megamorphic { .. } => "megamorphic",
            InlineRefusal::ReceiverNotDominant { .. } => "receiver-not-dominant",
            InlineRefusal::SpeculationInsideProtectedRange => "speculation-in-protected-range",
            InlineRefusal::CalleeTooLarge => "callee-too-large",
            InlineRefusal::BudgetExhausted { .. } => "budget-exhausted",
            InlineRefusal::GuardNotEmittable => "guard-not-emittable",
            InlineRefusal::NoInvalidationDependency => "no-invalidation-dependency",
            InlineRefusal::CalleeUnresolved { .. } => "callee-unresolved",
            InlineRefusal::BudgetAlreadySpent => "budget-already-spent",
            InlineRefusal::SaturatedProfile { .. } => "saturated-profile",
            InlineRefusal::ForceInterpreted => "force-interpreted",
        }
    }
}

/// The lowering an admitted plan asks the backend for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InlineVerdict {
    /// Statically bound callee — one possible target, no guard, no
    /// speculation. This is the only verdict the single-pass backend can emit
    /// today.
    DirectBind,
    /// Class-hierarchy analysis found exactly one loaded concrete class below
    /// the site's static type that provides the method, and the site is bound
    /// to that class's body behind the SAME exact class-id guard
    /// [`Self::Monomorphic`] uses.
    ///
    /// Kept apart from [`Self::Monomorphic`] although the lowering is
    /// identical, because the EVIDENCE is of a different kind and a metrics
    /// consumer must be able to tell them apart: `Monomorphic` rests on what
    /// the receiver profile observed, this rests on what the loaded hierarchy
    /// permits. That difference is the point of the verdict — it is answerable
    /// on a first, UNPROFILED compile, where the receiver profile is empty and
    /// the whole speculative ladder refuses with
    /// [`InlineRefusal::NoProfileEvidence`].
    ///
    /// **Guarded, not checkcast-free.** `NOTES-deopt2.md` M5 specifies this
    /// verdict as an unguarded `CheckcastFreeDirectBind`, whose soundness would
    /// rest entirely on the compiled body being retired before a conflicting
    /// class can reach the site. It is not named that here because this tree
    /// cannot discharge that obligation — see
    /// [`InlineDependency::UniqueConcreteMethod`] for the retirement argument
    /// and exactly where it fails. A guard costs one compare; a missing
    /// retirement runs the wrong method.
    UniqueConcreteMethod { guard_class_id: u32 },
    /// Speculative: splice the callee body behind `CMP DWORD [recv+0],
    /// guard_class_id` with the miss edge falling through to normal dispatch.
    Monomorphic { guard_class_id: u32 },
    /// Speculative: two guarded bodies, tried in descending profile order, with
    /// the second miss edge falling through to normal dispatch.
    Bimorphic { guard_class_ids: [u32; 2] },
    /// Not inlined.
    Refuse(InlineRefusal),
}

/// An assumption an inlined site makes about the world, and therefore a reason
/// to retire the caller's code when the world changes.
///
/// Both variants flatten to the `(class, method, descriptor)` triple that
/// [`CompiledMethod::inlined_methods`] already carries and that
/// `JitCache::invalidate_for_class_change` / `invalidate_for_class` /
/// `invalidate_unloaded_class` already scan — the dependency channel is the
/// existing one, not a new one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InlineDependency {
    /// The callee body was spliced in. If its class is redefined, replaced or
    /// unloaded, the caller's copy is stale.
    InlinedCallee {
        class_name: String,
        method_name: String,
        descriptor: String,
    },
    /// A receiver type was speculated on at a guarded site. Retiring on a
    /// conflicting class load is a PERFORMANCE obligation, not a correctness
    /// one — the exact class-id guard already routes every unexpected receiver
    /// to dispatch — but without it a hierarchy change leaves the caller
    /// paying a permanently-missing guard with no path back to a recompile.
    SpeculatedReceiver {
        class_name: String,
        class_id: u32,
        method_name: String,
        descriptor: String,
    },
    /// Class-hierarchy analysis found exactly ONE loaded concrete class below
    /// `class_name` that provides `method_name`/`descriptor`, and the site was
    /// bound to that class's body. `class_name` is the site's STATIC type — the
    /// class the constant pool names — which is the node whose subtree the
    /// analysis asked about and therefore the node a conflicting class load has
    /// to be reported against.
    ///
    /// # Why the static type and not the implementor
    ///
    /// The implementor is already covered: the body is spliced, so
    /// [`Self::InlinedCallee`] names it and a redefinition of the implementor
    /// retires the caller. What that does NOT catch is the load that actually
    /// invalidates the ANALYSIS — a *second* implementor appearing. The class
    /// define listener (`vm/src/vm/vm_init.rs`) calls
    /// `JitCache::invalidate_for_class_change` once for the newly defined class
    /// and once for every class in its supertype closure, so a `Z extends
    /// Base` reports `Base`, which is exactly this triple's class. Naming the
    /// implementor here instead would leave `Z`'s arrival invisible.
    ///
    /// # What this dependency is, and is not, load-bearing for
    ///
    /// **As issued by [`plan_inline`] in this tree the bind is GUARDED** — an
    /// exact `CMP DWORD [recv+0], guard_class_id` against the unique
    /// implementor's class id, with the miss edge falling through to unchanged
    /// dispatch, byte-identical to the profile-guided
    /// [`InlineVerdict::Monomorphic`] lowering. A receiver of a second
    /// implementor therefore MISSES and dispatches normally whether or not the
    /// body is ever retired, so retirement here is a PERFORMANCE obligation,
    /// exactly as it is for [`Self::SpeculatedReceiver`]: without it the caller
    /// pays a permanently-missing compare with no path back to a recompile.
    ///
    /// **An UNGUARDED bind on this dependency would be a correctness
    /// obligation, and this tree cannot discharge it.** Retirement
    /// (`JitCache::invalidate_matching`) withdraws a body from *dispatch*: it
    /// clears the inline caches, evicts the cache entries and sets
    /// [`CompiledMethod::retired`], and the executable pages are then reclaimed
    /// only once no frame can still be inside them
    /// ([`reclaim_retired_jit_code`]). A thread already executing the body when
    /// the define lands keeps executing it — deliberately, and every comment on
    /// that path says so. So a compiled method that loads a class and then
    /// invokes the bound site on an instance of it, which is the ordinary shape
    /// of every reflective framework, would run the wrong method with no guard
    /// to catch it. Closing that needs the define to deoptimize ACTIVE frames
    /// at a safepoint, which this VM has no machinery for. Until it does, do
    /// not issue this dependency behind an unguarded bind.
    UniqueConcreteMethod {
        /// Internal name of the site's static type — the invalidation key.
        class_name: String,
        /// That type's `ClassId` at plan time. Carried for diagnostics and for
        /// a future identity-keyed invalidation channel; the scans in use
        /// today are name-keyed and read [`Self::class_name`].
        class_id: u32,
        method_name: String,
        descriptor: String,
    },
}

impl InlineDependency {
    /// The `(class, method, descriptor)` form the invalidation scans match on.
    pub fn as_invalidation_triple(&self) -> (String, String, String) {
        match self {
            InlineDependency::InlinedCallee {
                class_name,
                method_name,
                descriptor,
            }
            | InlineDependency::SpeculatedReceiver {
                class_name,
                method_name,
                descriptor,
                ..
            }
            | InlineDependency::UniqueConcreteMethod {
                class_name,
                method_name,
                descriptor,
                ..
            } => (class_name.clone(), method_name.clone(), descriptor.clone()),
        }
    }
}

/// What the backend that will consume this plan is actually able to emit.
///
/// Passed in rather than probed, so the policy stays a pure function and so a
/// test can exercise the speculative arm without the production backend
/// silently deciding the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InlineBackendCaps {
    /// The backend splices callee bodies at statically bound sites.
    pub inline_body_at_static_sites: bool,
    /// The backend splices callee bodies at virtual/interface sites behind a
    /// receiver class-id guard, with the miss edge falling through to normal
    /// dispatch.
    pub guarded_inline_body_at_virtual_sites: bool,
}

impl InlineBackendCaps {
    /// What `x64::compile` can do, verified against the source (updated
    /// 2026-08-03, PGO-02):
    ///
    ///  * `invokestatic` consults `inline_sites` and `invokespecial` does
    ///    too (statically bound DirectBind splicing, pre-existing);
    ///  * the `0xb6 | 0xb7 | 0xb9` arm's `op == 0xb6 || op == 0xb9` case NOW
    ///    ALSO consults `inline_sites` plus a companion
    ///    `inline_guard_variants` map, when both carry an entry for the
    ///    pc — populated together, only for an admitted `Monomorphic`
    ///    verdict. See `docs/feature-designs/profile-guided-inlining.md` §5
    ///    for the exact guard-then-splice lowering and why a miss falls
    ///    through to the SAME unguarded direct-call path below rather than a
    ///    deopt (the `guard_class_id` compare inside the String/CRC32
    ///    intrinsic ladders is a separate, pre-existing mechanism this does
    ///    not touch).
    ///
    /// `guarded_inline_body_at_virtual_sites` below is still hard-coded
    /// `false`, and **must stay that way**. It is the *capability* default for
    /// a backend that was handed no `class_id_name_resolver`, and
    /// [`plan_inline`] cannot honour a guard it has no way to name a class for.
    /// The flip to `true` happens at the `plan_inline` call site in
    /// `try_compile_inner`, derived from whether a resolver was threaded in —
    /// which the VM now does by DEFAULT (N8 inverted
    /// `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` to an opt-**out**: set it to `0`
    /// to get the pre-PGO-02 behaviour back with one variable). Callers with
    /// no resolver — every hand-built fixture, and any embedder that passes
    /// `CompileRequest::new` — still get this const fn's `false`.
    pub const fn single_pass_x64() -> InlineBackendCaps {
        InlineBackendCaps {
            inline_body_at_static_sites: true,
            guarded_inline_body_at_virtual_sites: false,
        }
    }

    /// A backend that can emit every lowering this policy knows how to plan.
    /// No production caller: this exists so the speculative arm is testable
    /// before the backend catches up.
    pub const fn unrestricted() -> InlineBackendCaps {
        InlineBackendCaps {
            inline_body_at_static_sites: true,
            guarded_inline_body_at_virtual_sites: true,
        }
    }
}

/// One inlining question, fully specified. Every field is evidence the caller
/// already has; nothing here is resolved or computed by the policy.
pub struct InlineRequest<'a> {
    /// Caller bytecode pc of the invoke.
    pub pc: usize,
    /// `0` virtual, `1` special, `2` interface, `3` static — the same encoding
    /// `try_compile_inner` and [`JitInvokeInfo`] use.
    pub invoke_kind: u8,
    /// The method NAME the call site names, as written in the constant pool.
    ///
    /// Not derivable from [`Self::site`]: that is `None` at every speculative
    /// site, and where it is present it describes the RESOLVED body, whose name
    /// and descriptor can differ from the site's own (a bridge, or an
    /// implementor that inherits rather than declares). A dependency keyed on
    /// the site has to use the site's signature, because that is the signature
    /// a later class define can ask about.
    pub site_method_name: &'a str,
    /// The descriptor the call site names. See [`Self::site_method_name`].
    pub site_descriptor: &'a str,
    /// The CLASS the call site names, as written in the constant pool (`""`
    /// when the caller does not know it). JIT round 21 wave 15 (lane trace;
    /// `r21-review-w15-jit-findings-FIXED-20261020.md` item 3): the
    /// subtype-sensitive dependency of a constant-pool splice whose body
    /// [`Self::site`] names its DECLARING class (wave 14,
    /// `CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME`) is recorded on this class,
    /// whose subtypes can change the selection; see
    /// [`inlined_callee_dependency_class`].
    pub site_class_name: &'a str,
    /// The callee resolved from the CONSTANT-POOL class name.
    ///
    /// `Some` for a statically bound site, which is the only kind whose callee
    /// the constant pool actually determines. `None` is expected for a
    /// virtual/interface site: there the callee depends on the receiver's
    /// runtime class, so it is resolved per speculated class through
    /// [`Self::receiver_callee_resolver`] instead — see that field for why
    /// using the constant-pool name for a guarded site is a wrong-code bug and
    /// not merely a missed opportunity.
    pub site: Option<&'a InlineSite>,
    /// Whether [`call_site_is_hot`] says this site earns the `FreqInlineSize`
    /// tier.
    pub site_is_hot: bool,
    /// This bci's receiver-type profile, if any.
    pub receivers: Option<&'a profile::ReceiverCounts>,
    /// This bci's execution evidence, carried for metrics. Never used to
    /// REFUSE a statically bound site: doing so would stop an unprofiled
    /// compile from inlining anything, which is a regression, not a policy.
    pub call_site_evidence: profile::CallSiteEvidence,
    /// Nesting depth this inline would occupy; the outermost inline is `1`.
    pub depth: usize,
    /// How many copies of this callee are already on the inline stack
    /// (ancestors only — siblings are not recursion).
    pub recursive_copies: usize,
    /// Per-method expansion budget still unspent.
    pub budget_remaining: usize,
    /// Whether `pc` lies inside one of the caller's protected ranges.
    pub pc_in_protected_range: bool,
    /// Whether this compilation requested precise exception frames.
    pub precise_exception_frames: bool,
    /// What the consuming backend can emit.
    pub caps: InlineBackendCaps,
    /// Resolves a receiver class id to its internal class name, so a
    /// speculative plan can record a dependency the name-keyed invalidation
    /// scans will match. `None` — or a `None` answer — refuses the
    /// speculation.
    pub receiver_class_namer: Option<&'a dyn Fn(u32) -> Option<String>>,
    /// Resolves the body a receiver of EXACTLY this class id would dispatch
    /// to at this call site. `None` — or a `None` answer — refuses the
    /// speculation ([`InlineRefusal::CalleeUnresolved`]).
    ///
    /// This is the field that makes a guard mean what it says. The guard
    /// compares the receiver against a class id taken from the *profile*; the
    /// constant pool names the receiver expression's *static* type. Those two
    /// disagree at every site where the speculated class overrides the
    /// declared one — the single most ordinary shape in Java — and splicing
    /// the constant-pool body behind such a guard runs the superclass's
    /// method for a receiver the guard just certified as the subclass. No
    /// crash, no diagnostic, wrong answer. (The same family of bug is written
    /// up on `try_compile_inner`'s statically-bound direct-call arm, which was
    /// restricted to `invokespecial`/`invokestatic` for exactly this reason
    /// after H2 miscompiled `VersionedValue.getCurrentValue`.)
    ///
    /// Resolving from the receiver class is also what gives the lane
    /// `invokeinterface` reach at all: the constant-pool class of an interface
    /// call is the interface, whose declaration has no `Code` attribute.
    pub receiver_callee_resolver: Option<&'a dyn Fn(u32) -> Option<InlineSite>>,
    /// Class-hierarchy analysis for THIS site: the single loaded, concrete
    /// class below the site's static type that provides the called method, or
    /// `None` when there are zero or more than one.
    ///
    /// A closure with no arguments because the site's `(class, name,
    /// descriptor)` is already bound by the caller that built it, and because
    /// the answer costs a subtype walk — [`plan_inline`] calls it at most once,
    /// and only on the arms where it can act on the answer, so a site whose
    /// receiver profile already decides the question never pays for it.
    ///
    /// Returns the class id of the IMPLEMENTOR ITSELF, not of the class that
    /// declares the method: the guard compares a receiver's class word, so the
    /// only useful answer is one a receiver can actually equal. See
    /// `ClassRealm::unique_implementor` on the VM side.
    ///
    /// `None` (resolver absent, or a `None` answer) simply declines this
    /// verdict and lets the receiver-profile ladder decide, so a caller that
    /// does not supply it gets byte-identical planning.
    pub unique_concrete_resolver: Option<&'a dyn Fn() -> Option<UniqueConcreteBind>>,
}

/// What class-hierarchy analysis answered for one call site.
///
/// Carries the site's STATIC type alongside the implementor because the two
/// play different roles and neither can be derived from the other here: the
/// implementor's id is the guard CONSTANT, and the static type is the node
/// whose subtree was analysed and therefore the node an invalidation has to be
/// keyed on. See [`InlineDependency::UniqueConcreteMethod`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UniqueConcreteBind {
    /// `ClassId` of the single loaded concrete class that provides the body.
    /// This is what the receiver's class word is compared against.
    pub implementor_class_id: u32,
    /// `ClassId` of the site's static (constant-pool) type.
    pub static_type_class_id: u32,
    /// Internal name of the site's static type. Empty refuses the bind — the
    /// invalidation channel is name-keyed, and a dependency nothing can match
    /// is worse than no bind.
    pub static_type_name: String,
}

/// The decision, its price, and everything a metrics consumer or an
/// invalidation registrar needs from it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InlinePlan {
    pub verdict: InlineVerdict,
    /// What the receiver profile said. Reported even when the verdict ignored
    /// it (statically bound sites), so a metrics consumer can tell a
    /// devirtualised site from a naturally direct one.
    pub shape: ReceiverShape,
    /// Charge against the per-method budget. `0` for a refusal.
    pub expansion_cost: usize,
    /// Callee bytecodes this plan would splice. `0` for a refusal. This is the
    /// "inlined bytecodes" figure the C2 review asks to be measured.
    pub inlined_bytecodes: usize,
    /// Observed executions of this call site, from whichever profile source
    /// answered.
    pub site_observations: u32,
    /// Assumptions to record. Non-empty for every admitted plan — an admitted
    /// plan with no dependency is unrepresentable, because the last check in
    /// [`plan_inline`] converts that state into
    /// [`InlineRefusal::NoInvalidationDependency`].
    pub dependencies: Vec<InlineDependency>,
    /// For a SPECULATIVE verdict, the body to splice behind each guard, in
    /// guard order: `[0]` is the top-ranked receiver class, `[1]` the second
    /// (Bimorphic only). Each body was resolved by dispatching from that exact
    /// class, so it is the one a receiver the guard admits actually runs — the
    /// backend must splice these and not anything resolved from the constant
    /// pool. Empty for [`InlineVerdict::DirectBind`] and for every refusal.
    pub speculative_sites: Vec<(u32, InlineSite)>,
}

impl InlinePlan {
    pub(crate) fn refuse(
        reason: InlineRefusal,
        shape: ReceiverShape,
        observations: u32,
    ) -> InlinePlan {
        InlinePlan {
            verdict: InlineVerdict::Refuse(reason),
            shape,
            expansion_cost: 0,
            inlined_bytecodes: 0,
            site_observations: observations,
            dependencies: Vec::new(),
            speculative_sites: Vec::new(),
        }
    }

    /// Whether the plan inlines.
    pub fn is_admitted(&self) -> bool {
        !matches!(self.verdict, InlineVerdict::Refuse(_))
    }

    /// Whether the plan rests on a guarded, per-receiver-class body rather than
    /// on the constant-pool callee.
    ///
    /// This is the predicate `try_compile_inner` uses to decide whether to
    /// populate `inline_guard_variants`, so it must be true for EVERY verdict
    /// the backend lowers as a guard chain — including
    /// [`InlineVerdict::UniqueConcreteMethod`], whose guard is emitted by the
    /// same code as [`InlineVerdict::Monomorphic`]'s. A verdict that carries
    /// `speculative_sites` and answers `false` here would have its bodies
    /// dropped and its budget charged anyway.
    pub fn is_speculative(&self) -> bool {
        matches!(
            self.verdict,
            InlineVerdict::Monomorphic { .. }
                | InlineVerdict::Bimorphic { .. }
                | InlineVerdict::UniqueConcreteMethod { .. }
        )
    }

    /// Whether the plan's guard class came from class-hierarchy analysis rather
    /// than from the receiver profile. Reported apart from
    /// [`Self::is_speculative`] so the tally can answer the question CHA exists
    /// to answer: how many sites were devirtualised on a compile that had no
    /// profile to speculate from.
    pub fn is_hierarchy_bound(&self) -> bool {
        matches!(self.verdict, InlineVerdict::UniqueConcreteMethod { .. })
    }

    /// The refusal reason, if the plan refused.
    pub fn refusal(&self) -> Option<&InlineRefusal> {
        match &self.verdict {
            InlineVerdict::Refuse(r) => Some(r),
            _ => None,
        }
    }

    /// Guard class ids this plan requires the backend to compare against.
    /// Empty for [`InlineVerdict::DirectBind`]; an admitted SPECULATIVE plan
    /// always returns at least one.
    pub fn guard_class_ids(&self) -> Vec<u32> {
        match &self.verdict {
            InlineVerdict::Monomorphic { guard_class_id }
            | InlineVerdict::UniqueConcreteMethod { guard_class_id } => vec![*guard_class_id],
            InlineVerdict::Bimorphic { guard_class_ids } => guard_class_ids.to_vec(),
            _ => Vec::new(),
        }
    }

    /// Dependencies in the `(class, method, descriptor)` form
    /// [`CompiledMethod::inlined_methods`] carries.
    pub fn invalidation_triples(&self) -> Vec<(String, String, String)> {
        self.dependencies
            .iter()
            .map(InlineDependency::as_invalidation_triple)
            .collect()
    }
}

/// Classify a call site's receiver-type profile.
///
/// Ranking is deterministic — descending count, ties broken by ascending class
/// id — because two runs over the same profile must produce the same artifact.
/// An `FxHashMap` iteration order is not, which is exactly the kind of
/// difference that makes a JIT bug irreproducible.
pub fn classify_receiver_shape(counts: Option<&profile::ReceiverCounts>) -> ReceiverShape {
    let Some(counts) = counts.filter(|c| !c.is_empty()) else {
        return ReceiverShape::Unprofiled;
    };
    // The speculate policy of `profile::ReceiverEvidence` (tier proposal
    // R12-5): one pass, no allocation, no sort. Its summary ranks by
    // descending count, ties by ascending class id, as the sort here did; it
    // sums in u64 so the TOTAL's own overflow is detectable rather than
    // folded away (a total pinned at `u32::MAX` understates itself, which
    // inflates every share computed against it), and `observations` is that
    // total clamped to `u32::MAX`.
    let evidence = profile::ReceiverEvidence::of(counts);
    let summary = *evidence.summary();
    let observations = summary.observations;
    let types = summary.types;
    if observations == 0 {
        return ReceiverShape::Unprofiled;
    }
    // Saturation check BEFORE any share arithmetic. Once a counter has pinned
    // at `u32::MAX` the recorded proportions are no longer the observed
    // proportions: a site can read as monomorphic because its majority class
    // stopped counting, not because the program settled
    // (`profile::ProfileFidelity` states the same rule on the profile side).
    // Ordered ahead of the `Cold` check too — an unreadable profile is
    // unreadable at any magnitude, and a saturated one is never cold anyway.
    // `is_exact` is false exactly when the u64 total exceeds `u32::MAX` or one
    // count has pinned there.
    if !summary.is_exact() {
        return ReceiverShape::Saturated {
            types,
            observations,
        };
    }
    if observations < INLINE_MIN_SPECULATION_OBSERVATIONS {
        return ReceiverShape::Cold {
            observations,
            types,
        };
    }
    if types > INLINE_MEGAMORPHIC_TYPE_CEILING {
        return ReceiverShape::Megamorphic {
            types,
            observations,
        };
    }
    // A non-empty map always has a top; `Unprofiled` is only the type's answer.
    let Some((top_id, top_n)) = summary.top else {
        return ReceiverShape::Unprofiled;
    };
    // u64 throughout: `count * 100` overflows u32 at 43M observations, which a
    // long-running site reaches.
    let total = u64::from(observations);
    let top1 = u64::from(top_n);
    if top1.saturating_mul(100) >= total.saturating_mul(u64::from(INLINE_MONOMORPHIC_SHARE_PCT)) {
        return ReceiverShape::Monomorphic {
            class_id: top_id,
            observations,
        };
    }
    if let Some((second_id, second_n)) = summary.second.filter(|_| types >= 2) {
        let top2 = top1.saturating_add(u64::from(second_n));
        if top2.saturating_mul(100) >= total.saturating_mul(u64::from(INLINE_BIMORPHIC_SHARE_PCT)) {
            return ReceiverShape::Bimorphic {
                class_ids: [top_id, second_id],
                observations,
            };
        }
    }
    ReceiverShape::Megamorphic {
        types,
        observations,
    }
}

/// Whether `pc` lies inside any protected range of `exception_table`.
///
/// Same `[start_pc, end_pc)` test `precise_exception_frame_sites_supported`
/// uses, factored out so the policy and that predicate cannot disagree about
/// what "inside a `try` block" means.
pub fn pc_in_protected_range(
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    pc: usize,
) -> bool {
    exception_table
        .iter()
        .any(|entry| pc >= entry.start_pc as usize && pc < entry.end_pc as usize)
}

/// The class a spliced body's `InlineDependency::InlinedCallee` names (JIT
/// round 21 wave 15, lane trace; `docs/feature-designs/jit-ir-optimizations.md`
/// item 3). Since wave 14 a constant-pool resolution whose selected body is
/// declared above the constant-pool class names the DECLARING class in its
/// `InlineSite` (the frame label and id: `A.d1:110`, not `K3.d1:-1`). As a
/// dependency that widened the new-subtype withdrawal to every subtype of the
/// declaring class (`names_subtype_sensitive_member`), though only a subtype
/// of the class the site names can change what it selects. So a
/// constant-pool splice (`cp_resolution`) names `site_class_name` when it is
/// known and differs, and `enabled` says so; everything else names the body's
/// class, as before. The redefinition and unload dependency on the declaring
/// class stays: the resolver marks its bytecode copied
/// (`JitCache::note_bytecode_copied`, `CompiledMethod::copied_classes`), which
/// both scans match. Pure, for the unit test; `enabled` is asked only where
/// the answer could differ.
pub(crate) fn inlined_callee_dependency_class<'s>(
    cp_resolution: bool,
    site_class_name: &'s str,
    body_class_name: &'s str,
    enabled: impl FnOnce() -> bool,
) -> &'s str {
    if cp_resolution
        && !site_class_name.is_empty()
        && site_class_name != body_class_name
        && enabled()
    {
        site_class_name
    } else {
        body_class_name
    }
}

/// Rides on `CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME` (default ON, `=0`
/// is the kill word; read by the VM's resolver as
/// `jit_bridge::splice_declaring_class_frame_enabled`): `=0` names the
/// constant-pool class in the `InlineSite` itself, so the two always agree
/// again and this rule has nothing to do. Read per planned constant-pool
/// splice whose names differ (compile time, cold).
pub(crate) fn splice_cp_class_dependency_enabled() -> bool {
    true
}

/// The method slot of the replacement-only dependency triple
/// `(declaring class, DECLARING_CLASS_REPLACE_MARKER, descriptor)` that a
/// constant-pool splice naming its SITE class ([`inlined_callee_dependency_class`])
/// also records for the body's declaring class (JIT round 21 wave 17, lane
/// jit-selfcopy; `docs/feature-designs/jit-ir-optimizations.md`
/// item 7). Since wave 15 the `InlinedCallee` triple of `A.d1` spliced through
/// `K3` names `K3`, and the by-name define withdrawals
/// (`JitCache::invalidate_for_class`, called when a define REPLACES a class of
/// that name: an in-place stub upgrade, an `allow_redefine` re-key) match
/// `inlined_methods` by class name only, so they missed the caller. This
/// triple restores exactly that match. JVMS 4.2.2 admits `<` in a method name
/// only for `<init>` / `<clinit>`, so the marker never names a real member;
/// [`names_subtype_sensitive_member`] skips it, so a new subtype of `A` does
/// not withdraw the caller (the wave-15 point), and the finalizable-subtype
/// match looks for `<init>` only.
pub const DECLARING_CLASS_REPLACE_MARKER: &str = "<declaring-class>";

/// Does `dependency` (an `InlinedCallee`) name a class other than the body's
/// `body_class`, i.e. did [`inlined_callee_dependency_class`] pick the site's?
pub(crate) fn dependency_class_differs(dependency: &InlineDependency, body_class: &str) -> bool {
    matches!(dependency, InlineDependency::InlinedCallee { class_name, .. }
        if class_name != body_class)
}

/// Decide one inline site.
///
/// Checks run cheapest-and-most-universal first, so the reported refusal is the
/// most fundamental one rather than whichever fired last. The order is part of
/// the contract the tests assert.
pub fn plan_inline(req: &InlineRequest<'_>) -> InlinePlan {
    let shape = classify_receiver_shape(req.receivers);
    let observations = match shape {
        ReceiverShape::Unprofiled => req.call_site_evidence.count_or_zero(),
        ReceiverShape::Cold { observations, .. }
        | ReceiverShape::Monomorphic { observations, .. }
        | ReceiverShape::Bimorphic { observations, .. }
        | ReceiverShape::Saturated { observations, .. }
        | ReceiverShape::Megamorphic { observations, .. } => observations,
    };
    let refuse = |reason: InlineRefusal| InlinePlan::refuse(reason, shape, observations);

    // 1. The compilation-wide interlocks, which no per-site evidence can
    //    override.
    if req.precise_exception_frames {
        return refuse(InlineRefusal::PreciseExceptionFrames);
    }
    if req.depth > INLINE_MAX_DEPTH {
        return refuse(InlineRefusal::DepthLimit { depth: req.depth });
    }
    if req.recursive_copies > INLINE_MAX_RECURSIVE_DEPTH {
        return refuse(InlineRefusal::RecursionLimit {
            copies: req.recursive_copies,
        });
    }

    // 2. Kind, and the speculation it does or does not require.
    //
    //    The two arms differ in WHERE the callee body comes from, and that is
    //    the whole safety argument of a guarded inline: a statically bound
    //    site's callee is determined by the constant pool, a guarded site's is
    //    determined by the class the guard admits. `speculative_sites` below is
    //    empty for the first and populated for the second; nothing downstream
    //    is allowed to substitute one for the other.
    let mut dependencies = Vec::new();
    let mut speculative_sites: Vec<(u32, InlineSite)> = Vec::new();
    let verdict = match req.invoke_kind {
        1 | 3 => {
            if !req.caps.inline_body_at_static_sites {
                return refuse(InlineRefusal::GuardNotEmittable);
            }
            if req.site.is_none() {
                return refuse(InlineRefusal::CalleeUnresolved {
                    speculated_class_id: None,
                });
            }
            InlineVerdict::DirectBind
        }
        0 | 2 => {
            // Two independent grounds can name a guard class here, and they are
            // tried in the order that lets each answer what the other cannot.
            //
            //  * The receiver PROFILE says what actually arrived. Where it has
            //    a dominant type that is the strongest evidence available, and
            //    it is consulted first for that reason.
            //  * Class-hierarchy ANALYSIS says what the loaded hierarchy
            //    permits. It is consulted only where the profile declines to
            //    answer at all — an unprofiled first compile, or a site too
            //    cold to speculate from — which is precisely the gap CHA exists
            //    to close: before this, `ReceiverShape::Unprofiled` meant a
            //    virtual site inlined NOTHING, however trivially monomorphic
            //    the program was.
            //
            // Deliberately NOT consulted for `Megamorphic` / `Saturated`. Those
            // shapes are the profile stating that many different classes reach
            // this site; a CHA answer of "one implementor" against that is a
            // contradiction, and the honest reading of a contradiction is that
            // the analysis is looking at an incomplete hierarchy (see
            // `ClassRealm::unique_implementor` on which define paths feed the
            // subtype index). Guarding on the analysis anyway would be safe —
            // the miss edge is unchanged dispatch — but it would spend budget
            // and code space on a compare the profile has already said will
            // fail.
            //
            // `NOTES-deopt2.md` M5 orders the ladder the other way round
            // ("unique-concrete first, profile-guided second"). That order was
            // written for an UNGUARDED bind, where CHA is not evidence about
            // the receiver but a proof about the program and so outranks any
            // observation. For the guarded bind this actually issues, the
            // profile's class is the one receivers are known to have, and
            // preferring it keeps every already-soaked profiled site planning
            // byte-for-byte as it did.
            let mut hierarchy_bind: Option<UniqueConcreteBind> = None;
            let guard_ids: Vec<u32> = match shape {
                ReceiverShape::Unprofiled | ReceiverShape::Cold { .. } => {
                    match req.unique_concrete_resolver.and_then(|f| f()) {
                        Some(bind) => {
                            // Fail closed on an unnameable static type: the
                            // dependency below is what a second implementor's
                            // class define matches against, and an empty name
                            // matches nothing. Same rule, same reason, as the
                            // `receiver_class_namer` check further down.
                            if bind.static_type_name.is_empty() {
                                return refuse(InlineRefusal::NoInvalidationDependency);
                            }
                            let guard = bind.implementor_class_id;
                            hierarchy_bind = Some(bind);
                            vec![guard]
                        }
                        None => {
                            return refuse(match shape {
                                ReceiverShape::Cold { observations, .. } => {
                                    InlineRefusal::ColdSite { observations }
                                }
                                _ => InlineRefusal::NoProfileEvidence,
                            })
                        }
                    }
                }
                ReceiverShape::Saturated { observations, .. } => {
                    return refuse(InlineRefusal::SaturatedProfile { observations })
                }
                ReceiverShape::Megamorphic { types, .. } => {
                    // A type count over the ceiling is a genuinely polymorphic
                    // dispatch hub; anything else that lands here failed the
                    // share thresholds. Report them apart — they call for
                    // different fixes (never speculate vs. wait for a longer
                    // profile).
                    return refuse(if types > INLINE_MEGAMORPHIC_TYPE_CEILING {
                        InlineRefusal::Megamorphic { types }
                    } else {
                        InlineRefusal::ReceiverNotDominant { types }
                    });
                }
                ReceiverShape::Monomorphic { class_id, .. } => vec![class_id],
                ReceiverShape::Bimorphic { class_ids, .. } => class_ids.to_vec(),
            };
            if req.pc_in_protected_range {
                return refuse(InlineRefusal::SpeculationInsideProtectedRange);
            }
            if !req.caps.guarded_inline_body_at_virtual_sites {
                return refuse(InlineRefusal::GuardNotEmittable);
            }
            for &class_id in &guard_ids {
                // Fail closed: a speculation whose receiver class cannot be
                // NAMED cannot be recorded in the name-keyed invalidation
                // channel, so nothing could ever retire it.
                let Some(class_name) = req.receiver_class_namer.and_then(|f| f(class_id)) else {
                    return refuse(InlineRefusal::NoInvalidationDependency);
                };
                if class_name.is_empty() {
                    return refuse(InlineRefusal::NoInvalidationDependency);
                }
                // Fail closed again, on the other half: the body that class
                // actually dispatches to. `req.site` is deliberately NOT a
                // fallback here — using it would splice the constant-pool
                // class's body behind a guard that admits a subclass, which is
                // the wrong-code bug this resolver exists to prevent.
                let Some(callee) = req.receiver_callee_resolver.and_then(|f| f(class_id)) else {
                    return refuse(InlineRefusal::CalleeUnresolved {
                        speculated_class_id: Some(class_id),
                    });
                };
                if callee.class_name.is_empty() {
                    return refuse(InlineRefusal::NoInvalidationDependency);
                }
                dependencies.push(InlineDependency::SpeculatedReceiver {
                    class_name,
                    class_id,
                    method_name: callee.method_name.clone(),
                    descriptor: callee.descriptor.clone(),
                });
                speculative_sites.push((class_id, callee));
            }
            if let Some(bind) = hierarchy_bind {
                // The SECOND dependency a hierarchy-bound site needs, and the
                // one that is about the analysis rather than about the body.
                // The loop above already recorded a `SpeculatedReceiver` naming
                // the implementor, which retires the caller when the
                // implementor itself changes; this one names the STATIC type,
                // which is what a *second* implementor's class define reports
                // through the supertype closure. Without it, `Z extends Base`
                // arriving leaves this caller guarding on the first implementor
                // forever with no path back to a recompile that could plan for
                // both. See [`InlineDependency::UniqueConcreteMethod`] for why
                // that is a performance obligation here and would be a
                // correctness one behind an unguarded bind.
                //
                // Named with the SITE's method name and descriptor, not the
                // resolved body's: the two differ wherever the implementor
                // inherits rather than declares, and the question a class define
                // has to be able to ask is "did anyone bind `Base.m()` by
                // hierarchy", which is the site's own signature.
                if req.site_method_name.is_empty() {
                    return refuse(InlineRefusal::NoInvalidationDependency);
                }
                let dependency = InlineDependency::UniqueConcreteMethod {
                    class_name: bind.static_type_name,
                    class_id: bind.static_type_class_id,
                    method_name: req.site_method_name.to_string(),
                    descriptor: req.site_descriptor.to_string(),
                };
                if !dependencies.contains(&dependency) {
                    dependencies.push(dependency);
                }
                InlineVerdict::UniqueConcreteMethod {
                    guard_class_id: guard_ids[0],
                }
            } else if guard_ids.len() >= 2 {
                InlineVerdict::Bimorphic {
                    guard_class_ids: [guard_ids[0], guard_ids[1]],
                }
            } else {
                InlineVerdict::Monomorphic {
                    guard_class_id: guard_ids[0],
                }
            }
        }
        kind => return refuse(InlineRefusal::UnsupportedInvokeKind { kind }),
    };

    // 3. Price it. The size model is the pre-existing one; only the refusal
    //    reporting is new. A Bimorphic plan splices TWO bodies and is charged
    //    for both — each against its own per-site tier, the sum against the
    //    per-method budget. Anything less would let two guards buy twice the
    //    code for one site's price.
    let priced: Vec<&InlineSite> = if speculative_sites.is_empty() {
        req.site.into_iter().collect()
    } else {
        speculative_sites.iter().map(|(_, s)| s).collect()
    };
    let mut cost = 0usize;
    let mut inlined_bytecodes = 0usize;
    for site in &priced {
        let Some(site_cost) = inline_site_expansion_cost_tiered(site, req.site_is_hot) else {
            return refuse(InlineRefusal::CalleeTooLarge);
        };
        cost = cost.saturating_add(site_cost);
        inlined_bytecodes = inlined_bytecodes.saturating_add(site.callee_code_len);
    }
    if cost > req.budget_remaining {
        return refuse(InlineRefusal::BudgetExhausted {
            cost,
            remaining: req.budget_remaining,
        });
    }

    // 4. The callee body itself is always a dependency — one per body actually
    //    spliced, which for a Bimorphic plan is two different methods.
    for site in &priced {
        if site.class_name.is_empty() {
            return refuse(InlineRefusal::NoInvalidationDependency);
        }
        // JIT round 21 wave 15 (lane trace): a constant-pool splice names
        // the site's own class, not the body's declaring class
        // (`inlined_callee_dependency_class`).
        let cp_resolution = req.site.is_some_and(|cp| std::ptr::eq(cp, *site));
        let dependency = InlineDependency::InlinedCallee {
            class_name: inlined_callee_dependency_class(
                cp_resolution,
                req.site_class_name,
                &site.class_name,
                splice_cp_class_dependency_enabled,
            )
            .to_string(),
            method_name: site.method_name.clone(),
            descriptor: site.descriptor.clone(),
        };
        // JIT round 21 wave 17 (lane jit-selfcopy, w16 review item 7): when
        // that names the site's class, the body's declaring class also gets a
        // replacement-only marker, so a define that REPLACES it by name
        // (`JitCache::invalidate_for_class`) still finds this caller; the
        // new-subtype rule skips the marker (`names_subtype_sensitive_member`).
        let marker = (dependency_class_differs(&dependency, &site.class_name)).then(|| {
            InlineDependency::InlinedCallee {
                class_name: site.class_name.clone(),
                method_name: DECLARING_CLASS_REPLACE_MARKER.to_string(),
                descriptor: site.descriptor.clone(),
            }
        });
        if !dependencies.contains(&dependency) {
            dependencies.push(dependency);
        }
        if let Some(marker) = marker {
            if !dependencies.contains(&marker) {
                dependencies.push(marker);
            }
        }
    }

    InlinePlan {
        verdict,
        shape,
        expansion_cost: cost,
        inlined_bytecodes,
        site_observations: observations,
        dependencies,
        speculative_sites,
    }
}

/// Per-compilation inlining totals — the "inlined bytecodes, call count,
/// deopts, compile ms, code bytes" measurement the C2 review asks for, on the
/// inlining side. Deopt count, wall time and code bytes are already recorded by
/// [`metrics::CompilationReport`]; this supplies the three figures that report
/// has no source for.
///
/// Carried on [`CompiledMethod::inline_tally`] so `metrics::CompileRecorder::
/// installed` can harvest it from the artifact, the same way it already
/// harvests `code_bytes` / `deopt_points` — no new plumbing through
/// `try_compile_inner`'s forty return paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineDecisionTally {
    /// Sites a resolver produced a candidate for.
    pub candidates: u32,
    /// Sites actually inlined.
    pub inlined_sites: u32,
    /// Of those, sites resting on a receiver-type speculation and a guard.
    pub speculative_sites: u32,
    /// Of [`Self::speculative_sites`], the ones whose guard class came from
    /// class-hierarchy analysis rather than from the receiver profile
    /// ([`InlineVerdict::UniqueConcreteMethod`]).
    ///
    /// Reported separately because it is the only number that answers whether
    /// CHA is doing anything: a compile with no profile at all can show
    /// `speculative_sites > 0` only through this counter, and a soak that shows
    /// it flat has a CHA that is planning nothing, however many virtual sites
    /// the method has.
    pub hierarchy_bound_sites: u32,
    /// Callee bytecodes spliced in total.
    pub inlined_bytecodes: u32,
    /// Budget spent.
    pub expansion_cost: u32,
    /// Summed profile-observed executions of the inlined sites — the "call
    /// count" figure. Zero when the compile was unprofiled.
    pub observed_calls_inlined: u64,
    /// Refusals by [`InlineRefusal::category`], in first-seen order.
    pub refusals: Vec<(&'static str, u32)>,
}

impl InlineDecisionTally {
    /// Count one refusal without a plan behind it.
    ///
    /// Used for sites the compiler declines BEFORE paying for a resolver call
    /// — chiefly virtual/interface sites, whose receiver shape is readable from
    /// the profile alone. Without this the most interesting measurement of all
    /// (how many hot virtual sites are actually monomorphic) would cost a full
    /// callee resolution per site to obtain.
    pub fn record_refusal(&mut self, reason: &InlineRefusal) {
        self.candidates = self.candidates.saturating_add(1);
        let category = reason.category();
        match self.refusals.iter_mut().find(|(c, _)| *c == category) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => self.refusals.push((category, 1)),
        }
    }

    /// Fold one decision in.
    pub fn record(&mut self, plan: &InlinePlan) {
        match &plan.verdict {
            InlineVerdict::Refuse(reason) => {
                self.record_refusal(reason);
            }
            _ => {
                self.candidates = self.candidates.saturating_add(1);
                self.inlined_sites = self.inlined_sites.saturating_add(1);
                if plan.is_speculative() {
                    self.speculative_sites = self.speculative_sites.saturating_add(1);
                }
                if plan.is_hierarchy_bound() {
                    self.hierarchy_bound_sites = self.hierarchy_bound_sites.saturating_add(1);
                }
                self.inlined_bytecodes = self
                    .inlined_bytecodes
                    .saturating_add(plan.inlined_bytecodes.min(u32::MAX as usize) as u32);
                self.expansion_cost = self
                    .expansion_cost
                    .saturating_add(plan.expansion_cost.min(u32::MAX as usize) as u32);
                self.observed_calls_inlined = self
                    .observed_calls_inlined
                    .saturating_add(u64::from(plan.site_observations));
            }
        }
    }

    /// How many candidates were refused for `category`.
    pub fn refusal_count(&self, category: &str) -> u32 {
        self.refusals
            .iter()
            .find(|(c, _)| *c == category)
            .map_or(0, |(_, n)| *n)
    }
}

/// A constant-pool value that the JIT can materialize safely.
///
/// String literals deliberately carry their UTF-8 bytes rather than a Java
/// `ObjectRef`: the latter can relocate between compiled invocations.
#[derive(Clone, Debug)]
pub enum JitLdcConstant {
    /// A `CONSTANT_Integer` or `CONSTANT_Float` entry, already reduced to the
    /// machine word both backends push.
    ///
    /// `is_float` (cov-01) records which of the two it was. The single-pass
    /// backend does not read it — it pushes `bits` with `MOV imm64` and lets
    /// the CONSUMING opcode pick the width, exactly as it does for `ldc2_w`.
    /// The IR builder has no consuming opcode to ask: it must type the node it
    /// creates, and an `Op::Const`/`Int` holding a float bit pattern is a
    /// mistyped φ and a mistyped deopt slot. So this flag is the same shape
    /// `cp_ldc2w_resolver`'s `(bits, is_double)` already has, for the same
    /// reason.
    Immediate { bits: i64, is_float: bool },
    /// A `CONSTANT_String` entry: `ldc "…"` pushes the interned literal.
    ///
    /// Carries the *referencing* class id and the CP index rather than the
    /// literal's text, for the reason [`Self::ClassMirror`] carries them plus
    /// one that is specific to strings: JVMS §5.4.3 resolves a constant-pool
    /// entry once and records the result, the record is keyed
    /// `(class, cp index)`, and the text is not that key. Carrying the bytes
    /// instead made every execution re-derive the answer through the string
    /// pool's `RwLock` and a hash of the whole literal.
    String { holder_class_id: u32, cp_idx: u16 },
    /// A `CONSTANT_Class` entry: `ldc <Class>` pushes that class's mirror.
    ///
    /// Carries the *referencing* class id and the CP index rather than a
    /// resolved class id, for the same two reasons [`JitNewSite::Deferred`]
    /// does — the target may not be loaded yet, and loading it is a user
    /// `ClassLoader.loadClass` that must not run inside the compiler. The
    /// mirror is fetched at run time by `helpers.ldc_class_cp`, which is also
    /// what keeps it correct under a relocating collector.
    ClassMirror { holder_class_id: u32, cp_idx: u16 },
    /// A [`Self::String`] site the VM has already resolved, plus the address of
    /// a GC-maintained word that holds the literal
    /// (`vm::jit::helpers::compiled_ldc_slot_for`: a JNI-global-ref box, a root
    /// the collector rewrites in place). Compiled code may read the word
    /// instead of calling `helpers.ldc_string_cp`, which stays the fallback. A
    /// backend that ignores `slot_addr` treats this exactly as `String`.
    StringSlot {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    },
    /// The [`Self::ClassMirror`] twin of [`Self::StringSlot`].
    ClassMirrorSlot {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    },
    /// JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_LDC_CONDY_SLOT`, default ON): a
    /// reference-typed `CONSTANT_Dynamic` the VM has ALREADY resolved, plus
    /// the address of the GC-maintained word holding its result (the
    /// `compiled_ldc_slot_for` slot kind). JVMS §5.4.3.6 runs the bootstrap
    /// once per entry and every later `ldc` pushes the recorded result, so
    /// reading the word IS the whole instruction: no bootstrap, no loading,
    /// no side effect moves. `slot_addr` is never `0` (an unslotted entry is
    /// [`Self::Dynamic`]).
    ///
    /// The shape this exists for: a JDK-spun lambda proxy whose forwarder
    /// loads its implementation `MethodHandle` as class data (`ldc` of a
    /// `MethodHandles.classData` condy, `InnerClassLambdaMetafactory`'s
    /// `useImplMethodHandle`) used to bail permanently
    /// (`ldc-constant-unsupported`) and run its `invokeExact` interpreted on
    /// every call (`r23-w2-boxing-method-ref-call-costs-microseconds`).
    ///
    /// The single-pass backend emits it as an `ldc <Class>` site with this
    /// slot (the probe is always filled; the helper arm behind it is never
    /// reached, and would answer the same recorded result from its recorded-hit
    /// arm if it were). The IR builder has no table row for it and bails the
    /// method to single-pass, as for `MethodHandle` / `MethodType` constants.
    DynamicSlot {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    },
    /// A reference-typed `CONSTANT_Dynamic` with no slot yet (not resolved,
    /// the background worker before the offer mint, or the slot cache off).
    /// The compile is declined for now, NOT permanently: the entry is resolved
    /// by the interpreter's first execution of the `ldc`, and the next offer
    /// mints its slot (`mint_ldc_slots_at_offer`).
    Dynamic { holder_class_id: u32, cp_idx: u16 },
}

/// Compile-time resolution of a `new` (0xbb) / `anewarray` (0xbd)
/// constant-pool site, as reported by `cp_new_resolver`.
///
/// # Why this is an enum and not just `Option<(class_id, …)>`
///
/// The resolver can only name a `class_id` for a class that is ALREADY
/// LOADED — it deliberately does not run `ClassLoader.loadClass` (arbitrary
/// Java code from inside the compile path, with the class-manager lock in
/// play, loading classes the program itself never would). Before this enum
/// existed, "not loaded yet" and "this CP entry can never be a class"
/// collapsed into the same `None`, which bailed the WHOLE compile.
///
/// That made any hot method whose only un-taken branch does
/// `throw new SomeException(...)` permanently uncompilable: `SomeException`
/// is never loaded while nothing throws, so the resolver missed, the compile
/// bailed, and after `MAX_TIER_FAIL_RETRIES` the method interpreted forever
/// (json-smart's `JSONParserBase.readMain` — 293,940 interpreted invocations
/// of a workload's hottest method; see
/// `jit-compile-bail-unresolved-new-cold-class.md`).
///
/// [`JitNewSite::Deferred`] separates the two: the site compiles, and the
/// class is resolved at RUN TIME by the `new_object_cp` /
/// `anewarray_object_cp` helper — on the same thread, at the same program
/// point, and by the same code path the interpreter's own `0xbb`/`0xbd`
/// handler uses. A `None` return is now reserved for a genuinely malformed
/// site (the holder class is gone, or the CP entry is not a class
/// reference), which stays a permanent bail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitNewSite {
    /// The target class is loaded and its layout is known.
    ///
    /// `needs_post_alloc_init` / `has_finalizer` feed the inline-TLAB fast path,
    /// which skips the `jit_post_tlab_init` helper call when BOTH are false.
    /// A resolver that cannot compute them must report `true, true` so the
    /// helper call stays in place.
    Resolved {
        class_id: u32,
        num_fields: usize,
        needs_post_alloc_init: bool,
        has_finalizer: bool,
    },
    /// The target class is not loaded yet. Compile the site to the
    /// CP-indexed slow-path helper, which resolves + initialises it on first
    /// execution. `holder_class_id` is the class whose constant pool
    /// `cp_idx` indexes — i.e. the *referencing* class, which also supplies
    /// the initiating loader.
    Deferred { holder_class_id: u32, cp_idx: u16 },
}

/// JIT round 19 wave 3, lane eager3 (EAGER2-3 of `jit-r19-eager2-proposals.md`,
/// `CRATONVM_JIT_UNINIT_NEW_GUARD`): a `new` whose class is LOADED but owes its
/// initialization, admitted by the VM to a class-initialized guard
/// (`CompileRequest::cp_uninit_new_guard_resolver`). The first four fields are
/// [`JitNewSite::Resolved`]'s for the same class; `init_state` is the class's
/// own init word (`Class::init_state`), which the guard reads at a baked
/// address and the artifact keeps alive
/// (`CompiledMethod::_jit_class_init_cells`); `initialized` is the byte it
/// holds once the class is initialized.
#[derive(Clone, Debug)]
pub struct UninitNewGuardSite {
    pub class_id: u32,
    pub num_fields: usize,
    pub needs_post_alloc_init: bool,
    pub has_finalizer: bool,
    pub init_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    pub initialized: u8,
}

/// Is the `invoke_targets` row at `pc` one the resolver kept only for code its
/// prune feed called dead? A plain call for a builder that declines that prune;
/// the admission and cost rules leave it out, as they did before it had a row.
pub(crate) fn ir_splice_row_is_prune_dead(live_mask: &Option<Vec<bool>>, pc: usize) -> bool {
    live_mask
        .as_ref()
        .is_some_and(|m| !m.get(pc).copied().unwrap_or(true))
}
pub(crate) fn ir_splice_live_pc_mask(site: &InlineSite) -> Option<Vec<bool>> {
    if site.callee_always_taken_pcs.is_empty()
        && site.callee_never_taken_pcs.is_empty()
        && site.callee_pruned_switch_edges.is_empty()
    {
        return None;
    }
    splice_live_pcs_without_pruned_and_switch(
        &site.callee_code,
        site.callee_code_len,
        &site.callee_always_taken_pcs,
        &site.callee_never_taken_pcs,
        &site.callee_pruned_switch_edges,
    )
    .map(|(mask, _)| mask)
}

pub(crate) fn ir_splice_live_code_len(site: &InlineSite) -> usize {
    if site.callee_always_taken_pcs.is_empty()
        && site.callee_never_taken_pcs.is_empty()
        && site.callee_pruned_switch_edges.is_empty()
    {
        return site.callee_code_len;
    }
    splice_live_pcs_without_pruned_and_switch(
        &site.callee_code,
        site.callee_code_len,
        &site.callee_always_taken_pcs,
        &site.callee_never_taken_pcs,
        &site.callee_pruned_switch_edges,
    )
    .map_or(site.callee_code_len, |(_, live_bytes)| live_bytes)
}

pub(crate) fn is_ir_lowered_buffer_or_charset_intrinsic(t: &InlineInvokeTarget) -> bool {
    (t.invoke_kind == 3
        && t.class_name == "java/nio/ByteBuffer"
        && t.method_name == "wrap"
        && t.descriptor == "([BII)Ljava/nio/ByteBuffer;")
        || (t.invoke_kind == 1
            && (t.class_name == "java/nio/charset/Charset"
                || t.class_name == "sun/nio/cs/ISO_8859_1")
            && t.method_name == "decode"
            && t.descriptor == "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;")
}

pub(crate) fn ir_splice_site_has_several_returns(site: &InlineSite) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    if site.callee_always_taken_pcs.is_empty()
        && site.callee_never_taken_pcs.is_empty()
        && site.callee_pruned_switch_edges.is_empty()
    {
        return ir_splice_body_has_several_returns(code, len);
    }
    let Some((live, _)) = splice_live_pcs_without_pruned_and_switch(
        code,
        len,
        &site.callee_always_taken_pcs,
        &site.callee_never_taken_pcs,
        &site.callee_pruned_switch_edges,
    ) else {
        return ir_splice_body_has_several_returns(code, len);
    };
    let mut returns = 0usize;
    let mut pc = 0usize;
    while pc < len {
        if live.get(pc).copied().unwrap_or(false) && matches!(code.get(pc), Some(0xac..=0xb1)) {
            returns += 1;
            if returns > 1 {
                return true;
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}
