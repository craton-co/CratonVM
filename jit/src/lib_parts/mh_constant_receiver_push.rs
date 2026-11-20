#[allow(unused_imports)]
use super::super::*;

/// [`mh_constant_receiver_getstatic`], with `accept_ldc` also admitting an
/// `ldc` (2 bytes) or `ldc_w` (3 bytes) as the receiver's instruction: `(ldc
/// pc, constant-pool index)`.
pub(crate) fn mh_constant_receiver_push(
    code: &[u8],
    code_len: usize,
    pc: usize,
    site_params: usize,
    leaders: &[bool],
    invoke_shape: &dyn Fn(u16) -> Option<(usize, bool)>,
    accept_ldc: bool,
) -> Option<(usize, u16)> {
    let code_len = code_len.min(code.len());
    if pc >= code_len || leaders.get(pc).copied().unwrap_or(true) {
        return None;
    }
    let mut starts: Vec<usize> = Vec::new();
    let mut cur = 0usize;
    while cur < pc {
        starts.push(cur);
        cur += crate::bytecode_analysis::step(code, cur).max(1);
    }
    if cur != pc {
        return None;
    }
    // Values on the stack above the receiver, just after the instruction
    // being looked at.
    let mut above = site_params;
    for (walked, &at) in starts.iter().rev().enumerate() {
        if walked >= MH_CONSTANT_RECEIVER_WINDOW {
            return None;
        }
        let (pops, pushes) = mh_constant_window_effect(code, at, invoke_shape)?;
        if pushes > above {
            // This instruction pushed the receiver: a `getstatic`, whose one
            // value is the receiver, or nothing (a `dup` copies another value).
            if above != 0 {
                return None;
            }
            return match code.get(at) {
                Some(&0xb2) => {
                    let field_cp = u16::from_be_bytes([*code.get(at + 1)?, *code.get(at + 2)?]);
                    Some((at, field_cp))
                }
                Some(&0x12) if accept_ldc => Some((at, u16::from(*code.get(at + 1)?))),
                Some(&0x13) if accept_ldc => Some((
                    at,
                    u16::from_be_bytes([*code.get(at + 1)?, *code.get(at + 2)?]),
                )),
                _ => None,
            };
        }
        // No instruction after the receiver's may be a join.
        if leaders.get(at).copied().unwrap_or(true) {
            return None;
        }
        above = above - pushes + pops;
    }
    None
}

/// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time,
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE` in the VM, default ON): which
/// `invokevirtual MethodHandle.invokeExact` / `invoke` sites of `invoke_ops`
/// the optimizing tier plans as an `invokestatic` of the constant handle's
/// member, from the VM's per-site answer
/// (`CompileRequest::cp_mh_constant_resolver`).
///
/// A site is asked only when straight-line code proves its receiver is a
/// `getstatic` ([`mh_constant_receiver_getstatic`]). Admitted only where
/// joining the invoke plan cannot cost the method its plan, as
/// [`ir_indy_direct_plan`] admits: the static-call gate is on,
/// `static_call_shape` admits the descriptor, the member is not this method,
/// and its descriptor IS the site's (the call takes the site's values, so the
/// stack shape is the invoke's less its receiver). The sites of one field are
/// admitted only when every one of them answers the same call: the plan's
/// resolvers are keyed by the field.
///
/// JIT round 17 wave 3 (lane mh4; MHCONST17-2, `CRATONVM_JIT_MH_CONSTANT_VIRTUAL`
/// in the VM, default OFF): an answer of `invoke_kind == 0` (an unbound
/// virtual handle) is planned as an `invokevirtual` of the member under the
/// virtual-call gate, when the site's descriptor is the member's with one
/// leading reference (the receiver, [`mh_constant_virtual_site_matches`]) and
/// the site's receiver null guard has not already failed in an earlier body
/// (`guard_trapped(pc)`, as [`ir_indy_direct_plan`] asks): the builder guards
/// the receiver with a deopt that re-executes the `invokeExact`, so a `null`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrArrayListLayout {
    pub class_id: u32,
    pub element_data_field_index: u32,
    pub element_data_compact_offset: u32,
    pub size_field_index: u32,
    pub size_compact_offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrHashMapLayout {
    pub hashmap_class_id: u32,
    pub table_field_index: u32,
    pub table_compact_offset: i32,
    pub node_class_id: u32,
    pub node_hash_field_index: u32,
    pub node_hash_compact_offset: i32,
    pub node_key_field_index: u32,
    pub node_key_compact_offset: i32,
    pub node_value_field_index: u32,
    pub node_value_compact_offset: i32,
    pub integer_class_id: u32,
    pub integer_value_field_index: u32,
    pub integer_value_compact_offset: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IrCollectionLayouts {
    pub vm_ctx: usize,
    pub array_list: Option<IrArrayListLayout>,
    pub hash_map: Option<IrHashMapLayout>,
    pub object_array_class_id: u32,
    pub object_class_id: u32,
}

thread_local! {
    static COMPILE_COLLECTION_LAYOUTS: std::cell::Cell<Option<IrCollectionLayouts>> =
        const { std::cell::Cell::new(None) };
}

pub fn clear_ir_collection_layouts() {
    COMPILE_COLLECTION_LAYOUTS.with(|c| c.set(None));
}

pub fn register_ir_collection_layouts(
    vm_ctx: usize,
    array_list: Option<IrArrayListLayout>,
    hash_map: Option<IrHashMapLayout>,
) {
    register_ir_collection_layouts_with_objects(vm_ctx, array_list, hash_map, 0, 0);
}

pub fn register_ir_collection_layouts_with_object_array(
    vm_ctx: usize,
    array_list: Option<IrArrayListLayout>,
    hash_map: Option<IrHashMapLayout>,
    object_array_class_id: u32,
) {
    register_ir_collection_layouts_with_objects(
        vm_ctx,
        array_list,
        hash_map,
        object_array_class_id,
        0,
    );
}

pub fn register_ir_collection_layouts_with_objects(
    vm_ctx: usize,
    array_list: Option<IrArrayListLayout>,
    hash_map: Option<IrHashMapLayout>,
    object_array_class_id: u32,
    object_class_id: u32,
) {
    COMPILE_COLLECTION_LAYOUTS.with(|c| {
        c.set(Some(IrCollectionLayouts {
            vm_ctx,
            array_list,
            hash_map,
            object_array_class_id,
            object_class_id,
        }));
    });
}

pub fn ir_collection_layouts_cached(vm_ctx: usize) -> bool {
    vm_ctx != 0
        && COMPILE_COLLECTION_LAYOUTS.with(|c| {
            c.get().is_some_and(|l| {
                l.vm_ctx == vm_ctx
                    && l.array_list.is_some()
                    && l.hash_map.is_some()
                    && l.object_array_class_id != 0
                    && l.object_class_id != 0
            })
        })
}

pub fn ir_collection_layouts_for_vm(vm_ctx: usize) -> Option<IrCollectionLayouts> {
    COMPILE_COLLECTION_LAYOUTS.with(|c| c.get().filter(|l| l.vm_ctx == vm_ctx))
}

pub fn ir_collection_layouts_current() -> Option<IrCollectionLayouts> {
    COMPILE_COLLECTION_LAYOUTS.with(std::cell::Cell::get)
}

pub struct IrCollectionLayoutsScope {
    prev: Option<IrCollectionLayouts>,
}

impl Drop for IrCollectionLayoutsScope {
    fn drop(&mut self) {
        COMPILE_COLLECTION_LAYOUTS.with(|c| c.set(self.prev));
    }
}

pub fn enter_ir_collection_layouts_scope(
    layouts: Option<IrCollectionLayouts>,
) -> IrCollectionLayoutsScope {
    let prev = COMPILE_COLLECTION_LAYOUTS.with(|c| c.replace(layouts));
    IrCollectionLayoutsScope { prev }
}

/// meets the handle's own door and its `NullPointerException`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_mh_constant_plan(
    resolver: Option<&dyn Fn(u16, usize, u16) -> Option<(String, String, String, u8, u32)>>,
    invoke_names: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
    invoke_ops: &[(usize, u16, u8)],
    (static_calls, virtual_calls): (bool, bool),
    own: (&str, &str, &str),
    guard_trapped: &dyn Fn(usize) -> bool,
) -> IrMhConstantPlan {
    let (Some(resolver), Some(invoke_names)) = (resolver, invoke_names) else {
        return IrMhConstantPlan::default();
    };
    if !static_calls && !virtual_calls {
        return IrMhConstantPlan::default();
    }
    let invoke_shape = |cp_idx: u16| -> Option<(usize, bool)> {
        let (_, _, desc) = invoke_names(cp_idx)?;
        Some((count_param_slots(&desc), return_type(&desc) != b'V'))
    };
    let mut leaders: Option<Vec<bool>> = None;
    let mut by_cp: HashMap<u16, Option<IrMhConstantSite>> = HashMap::new();
    let accept_condy = mh_constant_condy_receiver_enabled();
    for &(pc, cp_idx, opcode) in invoke_ops {
        if opcode != 0xb6 {
            continue;
        }
        let Some((class, door, site)) = invoke_names(cp_idx) else {
            continue;
        };
        if class != "java/lang/invoke/MethodHandle"
            || !matches!(door.as_str(), "invokeExact" | "invoke")
        {
            continue;
        }
        let leaders = leaders.get_or_insert_with(|| {
            let mut map = crate::bytecode_analysis::branch_target_map(code, code_len);
            for &h in handler_pcs {
                if let Some(slot) = map.get_mut(h) {
                    *slot = true;
                }
            }
            map
        });
        // JIT r23 wave 4b (lane L2, L2W4B-1): or an `ldc` of a resolved condy.
        let Some((getstatic_pc, field_cp)) = mh_constant_receiver_push(
            code,
            code_len,
            pc,
            count_param_slots(&site),
            leaders,
            &invoke_shape,
            accept_condy,
        ) else {
            continue;
        };
        let answer = resolver(cp_idx, pc, field_cp)
            .filter(|(class, name, desc, kind, class_id)| {
                *class_id != 0
                    && match *kind {
                        3 => static_calls && *desc == site,
                        // JIT round 17 wave 3 (lane mh4; MHCONST17-2).
                        // JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an
                        // unbound interface handle, the same rule; the builder
                        // guards the receiver with an `instanceof` of the
                        // interface (`IrBuilder::mh_constant_opcode`).
                        0 | 2 => {
                            virtual_calls
                                && mh_constant_virtual_site_matches(&site, desc)
                                && !guard_trapped(pc)
                        }
                        // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): a
                        // REAL `bindTo` result of a virtual member, whose
                        // receiver is the handle's bound value: the member's
                        // descriptor IS the site's, and the builder's null
                        // guard on the value must not have trapped. Its
                        // `(slot, class)` row is asked after the walk
                        // (`IrMhConstantPlan::with_bound_rows`).
                        4 => virtual_calls && *desc == site && !guard_trapped(pc),
                        // j27 wave 6 (lane T1LAM2; `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`
                        // in the VM): a REAL `bindTo` result of a STATIC member,
                        // called with the handle's bound value first: the member's
                        // descriptor is the site's with that leading reference, and
                        // no guard is planted (the callee takes any value of its
                        // parameter type). Its `argL1` slot is asked after the walk
                        // (`IrMhConstantPlan::with_bound_rows`).
                        6 => static_calls && mh_constant_virtual_site_matches(desc, &site),
                        // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a
                        // REAL filter species, two plain calls. The shape and
                        // the gates of both are asked with its row
                        // (`IrMhConstantPlan::with_filter_rows`); its guards
                        // must not have trapped.
                        5 => (static_calls || virtual_calls) && !guard_trapped(pc),
                        7 => (static_calls || virtual_calls) && !guard_trapped(pc),
                        _ => false,
                    }
                    && static_call_shape(desc).is_some()
                    && (class.as_str(), name.as_str(), desc.as_str()) != own
            })
            .map(
                |(class, name, desc, invoke_kind, class_id)| IrMhConstantSite {
                    pcs: vec![(pc, getstatic_pc)],
                    class,
                    name,
                    desc,
                    class_id,
                    invoke_kind,
                    invoke_cp: cp_idx,
                    bound: None,
                    site: site.clone(),
                    filter: None,
                    convert: None,
                    pre_key: 0,
                },
            );
        match by_cp.entry(field_cp) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(answer);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let same = match (slot.get(), &answer) {
                    (Some(a), Some(b)) => {
                        (&a.class, &a.name, &a.desc, a.class_id, a.invoke_kind)
                            == (&b.class, &b.name, &b.desc, b.class_id, b.invoke_kind)
                    }
                    _ => false,
                };
                match slot.get_mut() {
                    Some(s) if same => s.pcs.push((pc, getstatic_pc)),
                    other => *other = None,
                }
            }
        }
    }
    IrMhConstantPlan {
        by_cp: by_cp
            .into_iter()
            .filter_map(|(cp_idx, site)| site.map(|s| (cp_idx, s)))
            .collect(),
    }
}

/// `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL` -- perf, **default ON** (JIT round 21,
/// lane lambda2: TLOCAL21-1 of `jit-r21-tlocal-proposals.md` and
/// LAMBDATRAP21-3 of `jit-proposals/jit-r21-lambdatrap-proposals-RETIRED-20261030.md`; `=0` keeps the
/// interface call): an `invokeinterface` SAM call whose receiver straight-line
/// code proves is the value of this method's own lambda `invokedynamic` is
/// planned as an `invokestatic` of the lambda's static impl
/// ([`ir_lambda_impl_plan`], `IrBuilder::lambda_impl_opcode`). Read per
/// compile (no static).
pub(crate) fn ir_lambda_impl_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES` -- perf, **default ON** (JIT
/// round 21, lane lambda3; page `jr21-tlocal-capture-new-kernel-20261014.md`,
/// "Round 21 lane lambda3"; `=0` restores lane lambda2's plan exactly): under
/// [`ir_lambda_impl_call_enabled`], a CAPTURING lambda's SAM call is planned
/// as its impl call only where the impl splices -- the one route that removes
/// both the call and the proxy's last use -- and never makes the method lose
/// its optimizing body:
///
/// * the door plans a capturing site only when the IR inline resolver answers
///   its impl, and not at a pc the splice rebuild already dropped
///   ([`IrLambdaImplPlan::without_capture_pcs`]);
/// * the builder refuses a capturing site it would not splice
///   (`IrBuilder::set_lambda_impl_captures_need_splice`);
/// * a build or lowering refused while any lambda plan was in force is
///   retried once with the whole plan withheld (`ir_tier`), which is the
///   `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL=0` body.
///
/// Measured before it (host, `--real-jdk`, A vs `=0`): the unspliced
/// capturing route lost 3.5x on `R21Lambda2Rows capture-two-args` (1451 vs
/// 421 ns) and `R21TlocalLambda capture-create-apply` (1371 vs 392). Read per
/// compile (no static).
pub(crate) fn ir_lambda_impl_spliced_captures_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_IMPL_CAPTURING` -- perf, default ON since the w3l A/B, `=0` off (JIT round 21,
/// lane lambda3, second host round): under [`ir_lambda_impl_call_enabled`],
/// plan a CAPTURING lambda's SAM call as its impl call at all. Off, only
/// non-capturing sites (published singletons, static method references) are
/// planned and a capturing site keeps the interface call. Measured on the
/// round head with the spliced-captures restriction ON (host, `--real-jdk`,
/// medians, plan vs `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL=0`): `R21Lambda2Rows
/// capture-two-args` 1294 vs 387 and `R21TlocalLambda capture-create-apply`
/// 1194 vs 357, although the census named the shape `call=spliced
/// proxy=elided` there; `capture-ref-apply` won (42 vs 704). Until the cause
/// is named (page `jr21-tlocal-capture-new-kernel-20261014.md`, "second host
/// round"), capturing sites were opt-in. Lane lambdacap named it (the C2 body
/// dropped the context ABI the C1 body had, so no forward reached it;
/// `CRATONVM_JIT_IR_LAMBDA_PROXY_KEEPS_CONTEXT`), and the orchestrator flipped
/// this switch DEFAULT ON on the w3l A/B: capture-create-apply 15 vs 363 ns,
/// capture-two-args 29 vs 372, capture-ref-apply 43 vs 749, no row lost;
/// `=0` turns it off. Read per compile (no static).
pub(crate) fn ir_lambda_impl_capturing_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_IMPL_IN_SPLICE` -- perf, **default ON** (JIT round
/// 21 wave 15, lane lambda3: LAMBDACAP21-2 of `jit-proposals/jit-r21-lambdacap-proposals-RETIRED-20261030.md`;
/// `=0` keeps a spliced body's SAM calls the inline cache): under
/// [`ir_lambda_impl_call_enabled`], a SAM `invokeinterface` INSIDE a spliced
/// body whose receiver straight-line code proves is the value of that body's
/// own lambda `invokedynamic` gets its lambda's static impl spliced in its
/// place (`ir_graft_spliced_lambda_impls`), the captures in front of the SAM
/// arguments (`IrBuilder::lambda_impl_opcode`), exactly as the method's own
/// sites are planned. A capturing site needs the same switches as there
/// ([`ir_lambda_impl_capturing_enabled`]). Read per compile (no static).
///
/// Why: `R21Lambda2Rows capture-new-kernel` (~340 ns, HotSpot 5) and
/// `R21TlocalLambda word-create-apply` (77 vs 21 for the same kernel unspliced)
/// are kernels spliced into their caller `run(II)I`, where the plan never
/// applied: the SAM call stayed the inline cache + lambda thunk and the proxy
/// escaped into it, so every iteration paid the allocation and the call.
pub(crate) fn ir_lambda_impl_in_splice_enabled() -> bool {
    true
}

/// The nesting depth up to which [`ir_graft_spliced_lambda_impls`] grafts:
/// the impl is one more level, and the resolver's own bound is
/// [`MAX_IR_INLINE_NEST_DEPTH`].
pub(crate) const IR_LAMBDA_GRAFT_MAX_DEPTH: usize = MAX_IR_INLINE_NEST_DEPTH - 1;

/// JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2,
/// [`ir_lambda_impl_in_splice_enabled`]): for `site` and every body nested in
/// it, each SAM `invokeinterface` whose receiver straight-line code of that
/// body proves is the value of the body's own lambda `invokedynamic`
/// ([`lambda_value_producer`], [`lambda_local_store`], the method-own plan's
/// walk) gets its lambda's static impl, resolved by `resolver` (the IR inline
/// resolver), as a [`NestedInlineSite`] at the same callee pc, with a row in
/// [`InlineSite::ir_lambda_impl_rows`]. Returns how many were grafted.
///
/// A site qualifies exactly as a method-own site does in
/// [`ir_lambda_impl_lambdas`] / [`ir_lambda_impl_plan`]: the VM names the
/// lambda's static impl (`impls`, keyed by the instruction's bridge site:
/// [`InlineSite::ir_indy_rows`] for a constant-word site,
/// [`InlineSite::ir_indy_capture_sites`] for a capturing one) with the
/// site's capture count, proxy class and capture tags; the call's name and
/// descriptor are the SAM's; the impl is a static method of the right arity
/// that is neither the compiling method (`own`) nor the enclosing body. Not
/// in a synchronized body or one with handler rows, and never an impl body
/// with handler rows (the planner would refuse the WHOLE top-level site over
/// them) or a synchronized one. Capturing sites only with `capturing`.
///
/// t1 wave 8 (LAMBDA321-6): a grafted impl body is itself walked (one level
/// deeper, bounded by [`IR_LAMBDA_GRAFT_MAX_DEPTH`]), so an impl that builds
/// and applies its own lambda gets that inner impl grafted too.
///
/// The SAM call's own `invoke_targets` row is kept: a graft the append
/// refuses leaves the inline-cache call it always was (`append_ir_inline_site`
/// registers a row only for an appended body).
///
/// `loop_slot` (JIT round 21 wave 19, lane lambda3fix, LAMBDA321-4,
/// [`ir_lambda_impl_loop_slot_enabled`]): a receiver local the straight-line
/// walk cannot follow back to its `astore` (a loop header between) also
/// qualifies when the body writes it only once, outside every loop
/// ([`lambda_single_store_outside_loops`], the body's own parameter slots).
pub(crate) fn ir_graft_spliced_lambda_impls(
    site: &mut InlineSite,
    impls: &dyn Fn(usize) -> Option<compile_request::IndyLambdaImplAnswer>,
    resolver: &dyn Fn(&str, &str, &str) -> Option<InlineSite>,
    capturing: bool,
    own: (&str, &str, &str),
    loop_slot: bool,
    depth: usize,
) -> usize {
    if depth > IR_LAMBDA_GRAFT_MAX_DEPTH {
        return 0;
    }
    let mut grafted = 0usize;
    for n in site.nested_sites.iter_mut() {
        grafted += ir_graft_spliced_lambda_impls(
            &mut n.site,
            impls,
            resolver,
            capturing,
            own,
            loop_slot,
            depth + 1,
        );
    }
    if site.ir_indy_rows.is_empty() && site.ir_indy_capture_rows.is_empty() {
        return grafted;
    }
    // j27 wave 7 (lane T1LAM3, LAMBDACAP21-2 / LAMBDA321-3): the body's
    // name for the `[ir] lambda-impl-skip-in-splice` census.
    let body_name = (
        site.class_name.as_str(),
        site.method_name.as_str(),
        site.descriptor.as_str(),
    );
    if site.callee_is_synchronized || !site.ir_handler_rows.is_empty() {
        let refused = crate::lambda_impl_site_skip::refused_body_rows(
            &site.ir_indy_rows,
            &site.ir_indy_capture_rows,
        );
        crate::lambda_impl_site_skip::print_spliced_skips(own, body_name, &refused);
        return grafted;
    }
    let code_len = site.callee_code_len.min(site.callee_code.len());
    // Every indy's stack effect `(argument entries, return tag)` for the
    // receiver walk, a capture row's over its word row.
    let mut shapes: HashMap<usize, (usize, u8)> = site
        .ir_indy_rows
        .iter()
        .map(|&(ipc, args, ret, _)| (ipc, (args, ret)))
        .collect();
    shapes.extend(
        site.ir_indy_capture_rows
            .iter()
            .map(|(ipc, _, tags)| (*ipc, (tags.len(), b'L'))),
    );
    // `indy pc -> (capture count, the VM's answer)`, and why each other row
    // was refused.
    let mut skipped: Vec<(usize, crate::lambda_impl_site_skip::LambdaSiteSkip)> = Vec::new();
    let lambdas = crate::lambda_impl_site_skip::spliced_body_lambdas(
        &site.ir_indy_rows,
        &site.ir_indy_capture_rows,
        &site.ir_indy_capture_sites,
        capturing,
        impls,
        &mut skipped,
    );
    if lambdas.is_empty() {
        crate::lambda_impl_site_skip::print_spliced_skips(own, body_name, &skipped);
        return grafted;
    }
    // The body's invokes, by constant-pool index, from the resolver's rows
    // (the callee's pool is not ours to read).
    let code = &site.callee_code[..code_len];
    let mut cp_descs: HashMap<u16, String> = HashMap::new();
    for (tpc, t) in &site.invoke_targets {
        if let (Some(&hi), Some(&lo)) = (code.get(tpc + 1), code.get(tpc + 2)) {
            cp_descs.insert(u16::from_be_bytes([hi, lo]), t.descriptor.clone());
        }
    }
    let invoke_shape = |cp_idx: u16| -> Option<(usize, bool)> {
        let desc = cp_descs.get(&cp_idx)?;
        Some((count_param_slots(desc), return_type(desc) != b'V'))
    };
    let effect = |at: usize| -> Option<(usize, usize)> {
        if code.get(at) == Some(&0xba) {
            let &(args, ret) = shapes.get(&at)?;
            return Some((args, usize::from(ret != b'V')));
        }
        mh_constant_window_effect(code, at, &invoke_shape)
    };
    let leaders = crate::bytecode_analysis::branch_target_map(code, code_len);
    let enclosing = (
        site.class_name.as_str(),
        site.method_name.as_str(),
        site.descriptor.as_str(),
    );
    let mut new_nested: Vec<NestedInlineSite> = Vec::new();
    let mut new_rows: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (tpc, target) in &site.invoke_targets {
        let tpc = *tpc;
        if target.invoke_kind != 2
            || code.get(tpc) != Some(&0xb9)
            || site.nested_sites.iter().any(|n| n.callee_pc == tpc)
            || new_rows.iter().any(|r| r.0 == tpc)
        {
            continue;
        }
        let sam_args = count_param_slots(&target.descriptor);
        let Some(producer) =
            lambda_value_producer(code, code_len, tpc, sam_args, &leaders, &effect)
        else {
            continue;
        };
        let indy_pc = if code.get(producer) == Some(&0xba) {
            producer
        } else {
            let Some(slot) = lambda_aload_slot(code, producer) else {
                continue;
            };
            let store = match lambda_local_store(code, code_len, producer, slot, &leaders) {
                Some(store) => store,
                // LAMBDA321-4 (the method-own plan's rule; this body has no
                // handler rows, checked above).
                None if loop_slot => {
                    let params =
                        count_param_slots(&site.descriptor) + usize::from(!site.callee_is_static);
                    let Some(store) =
                        lambda_single_store_outside_loops(code, code_len, slot, params)
                    else {
                        continue;
                    };
                    store
                }
                None => continue,
            };
            match lambda_value_producer(code, code_len, store, 0, &leaders, &effect) {
                Some(at) if code.get(at) == Some(&0xba) => at,
                _ => continue,
            }
        };
        let Some((captures, answer)) = lambdas.get(&indy_pc) else {
            continue;
        };
        let impl_params = count_param_slots(&answer.descriptor);
        let triple = (
            answer.class.as_str(),
            answer.name.as_str(),
            answer.descriptor.as_str(),
        );
        if answer.sam_name != target.method_name
            || answer.sam_descriptor != target.descriptor
            || answer.class_id == 0
            || impl_params != captures + sam_args
            || static_call_shape(&answer.descriptor).is_none()
            || triple == own
            || triple == enclosing
        {
            continue;
        }
        let Some(mut body) = resolver(&answer.class, &answer.name, &answer.descriptor) else {
            continue;
        };
        if !body.callee_is_static
            || body.callee_is_synchronized
            || body.descriptor != answer.descriptor
            || ir_site_tree_has_handler_rows(&body)
        {
            continue;
        }
        // LAMBDA321-6 (t1 wave 8): the impl's own lambdas, one level deeper.
        grafted += ir_graft_spliced_lambda_impls(
            &mut body,
            impls,
            resolver,
            capturing,
            own,
            loop_slot,
            depth + 1,
        );
        new_rows.push((tpc, indy_pc, *captures, impl_params));
        new_nested.push(NestedInlineSite {
            callee_pc: tpc,
            guard_class_id: 0,
            site: body,
            bimorphic_second: None,
        });
    }
    skipped.extend(crate::lambda_impl_site_skip::unplanned(
        lambdas.keys().copied(),
        new_rows.iter().map(|r| r.1),
    ));
    crate::lambda_impl_site_skip::print_spliced_skips(own, body_name, &skipped);
    grafted += new_rows.len();
    site.nested_sites.extend(new_nested);
    site.ir_lambda_impl_rows.extend(new_rows);
    grafted
}

/// JIT round 21 (lane lambda2): one `invokedynamic` constant-pool entry's
/// planned impl call (see [`ir_lambda_impl_plan`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IrLambdaImplSite {
    /// `(invokeinterface pc, invokedynamic pc)` of every SAM call planned
    /// under this entry.
    pub(crate) pcs: Vec<(usize, usize)>,
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    /// The impl's declaring class.
    pub(crate) class_id: u32,
    /// The lambda's capture count: the impl's leading parameters.
    pub(crate) captures: usize,
}

/// JIT round 21 (lane lambda2): the SAM calls the optimizing tier plans as
/// their lambda's impl call, keyed by the `invokedynamic`'s constant-pool
/// index -- the key every resolver of the invoke plan is asked by. An
/// `InvokeDynamic` entry is never a `Methodref`, so a key here never shadows an
/// ordinary invoke's answer (the indy-direct plan's own argument).
#[derive(Default, Debug)]
pub(crate) struct IrLambdaImplPlan {
    pub(crate) by_cp: HashMap<u16, IrLambdaImplSite>,
}

impl IrLambdaImplPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.by_cp.is_empty()
    }

    pub(crate) fn site_count(&self) -> usize {
        self.by_cp.values().map(|s| s.pcs.len()).sum()
    }

    /// The impl's `(class, name, descriptor)`, as `cp_invoke_resolver` answers.
    pub(crate) fn names(&self, cp_idx: u16) -> Option<(String, String, String)> {
        self.by_cp
            .get(&cp_idx)
            .map(|s| (s.class.clone(), s.name.clone(), s.desc.clone()))
    }

    /// The impl's class id, as `cp_invoke_class_id_resolver` answers.
    pub(crate) fn class_id(&self, cp_idx: u16) -> Option<u32> {
        self.by_cp.get(&cp_idx).map(|s| s.class_id)
    }

    /// `invoke pc -> (invokedynamic pc, capture count, impl parameter count)`
    /// for `IrBuilder::set_lambda_impl_sites`.
    pub(crate) fn builder_sites(&self) -> HashMap<usize, (usize, usize, usize)> {
        self.by_cp
            .values()
            .flat_map(|s| {
                let params = count_param_slots(&s.desc);
                s.pcs
                    .iter()
                    .map(move |&(pc, indy_pc)| (pc, (indy_pc, s.captures, params)))
            })
            .collect()
    }

    /// JIT round 21 (lane lambda3): the plan without the CAPTURING sites'
    /// calls at `pcs` (the splice rebuild's dropped sites: an unspliced impl
    /// call there loses to the interface call); an entry left with no call is
    /// dropped. Non-capturing sites are kept.
    pub(crate) fn without_capture_pcs(mut self, pcs: &[usize]) -> Self {
        if pcs.is_empty() {
            return self;
        }
        self.by_cp.retain(|_, site| {
            if site.captures > 0 {
                site.pcs.retain(|(pc, _)| !pcs.contains(pc));
            }
            !site.pcs.is_empty()
        });
        self
    }

    /// JIT round 21 (lane lambda3): one census line per planned SAM call of
    /// the graph about to be lowered -- how the impl call was built
    /// (`spliced`: no call node left at its pc; `bound`: a direct call;
    /// `late-cell`: a late-bound cell; `generic`: the dispatch helper) and
    /// whether the capturing proxy's allocation survived (`kept` /
    /// `elided`; `singleton` for a non-capturing site).
    pub(crate) fn census_lines(
        &self,
        graph: &ir::Graph,
        direct_calls: &std::collections::HashMap<usize, (usize, bool)>,
        late_cells: &std::collections::HashMap<usize, usize>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let mut sites: Vec<(usize, usize, &IrLambdaImplSite)> = self
            .by_cp
            .values()
            .flat_map(|s| s.pcs.iter().map(move |&(pc, indy)| (pc, indy, s)))
            .collect();
        sites.sort_by_key(|&(pc, _, _)| pc);
        for (pc, indy_pc, site) in sites {
            let live = |n: &&ir::Node| !matches!(n.op, ir::Op::Dead);
            let calls = graph
                .nodes
                .iter()
                .filter(live)
                .filter(|n| matches!(n.op, ir::Op::Call { .. }) && n.bytecode_pc == Some(pc))
                .count();
            let call = if calls == 0 {
                "spliced"
            } else if direct_calls.contains_key(&pc) {
                "bound"
            } else if late_cells.contains_key(&pc) {
                "late-cell"
            } else {
                "generic"
            };
            let proxy = if site.captures == 0 {
                "singleton"
            } else if graph.site_trap_pcs.contains(&indy_pc) {
                // The `invokedynamic` is an uncommon trap: no proxy was built
                // and the impl call after it is dead code.
                "trapped"
            } else if graph.nodes.iter().filter(live).any(|n| {
                n.bytecode_pc == Some(indy_pc)
                    && matches!(n.op, ir::Op::New { class_id, .. }
                        if graph.lambda_proxy_alloc_classes.contains(&class_id))
            }) {
                "kept"
            } else {
                "elided"
            };
            lines.push(format!(
                "pc={pc} indy_pc={indy_pc} impl={}.{}{} captures={} call={call} calls={calls} \
                 proxy={proxy}",
                site.class, site.name, site.desc, site.captures,
            ));
        }
        lines
    }

    /// `invoke_ops` with every planned SAM call's `invokeinterface` row
    /// replaced by an `invokestatic` row keyed by its lambda's constant-pool
    /// index.
    pub(crate) fn rewritten_invoke_ops(
        &self,
        invoke_ops: &[(usize, u16, u8)],
    ) -> Vec<(usize, u16, u8)> {
        if self.by_cp.is_empty() {
            return invoke_ops.to_vec();
        }
        let planned: HashMap<usize, u16> = self
            .by_cp
            .iter()
            .flat_map(|(&cp, s)| s.pcs.iter().map(move |&(pc, _)| (pc, cp)))
            .collect();
        invoke_ops
            .iter()
            .map(|&(pc, cp_idx, op)| match planned.get(&pc) {
                Some(&indy_cp) if op == 0xb9 => (pc, indy_cp, 0xb8),
                _ => (pc, cp_idx, op),
            })
            .collect()
    }
}

/// The start pc of the instruction that pushed the value lying `above` values
/// below the top of the operand stack just before the instruction at `pc`
/// runs -- or `None` unless straight-line code proves it (JIT round 21, lane
/// lambda2; the walk of [`mh_constant_receiver_getstatic`], answering any
/// producer that pushes exactly that one value). `effect(at)` answers an
/// instruction's `(values popped, values pushed)`. Fails at a branch or
/// handler target after the producer (`pc` included), at an unmodelled
/// instruction, or past [`MH_CONSTANT_RECEIVER_WINDOW`] instructions.
pub(crate) fn lambda_value_producer(
    code: &[u8],
    code_len: usize,
    pc: usize,
    above: usize,
    leaders: &[bool],
    effect: &dyn Fn(usize) -> Option<(usize, usize)>,
) -> Option<usize> {
    let code_len = code_len.min(code.len());
    if pc >= code_len || leaders.get(pc).copied().unwrap_or(true) {
        return None;
    }
    let mut starts: Vec<usize> = Vec::new();
    let mut cur = 0usize;
    while cur < pc {
        starts.push(cur);
        cur += crate::bytecode_analysis::step(code, cur).max(1);
    }
    if cur != pc {
        return None;
    }
    let mut above = above;
    for (walked, &at) in starts.iter().rev().enumerate() {
        if walked >= MH_CONSTANT_RECEIVER_WINDOW {
            return None;
        }
        let (pops, pushes) = effect(at)?;
        if pushes > above {
            // Exactly the one value (a `dup` copies another value).
            return (pushes == 1 && above == 0).then_some(at);
        }
        if leaders.get(at).copied().unwrap_or(true) {
            return None;
        }
        above = above - pushes + pops;
    }
    None
}

/// The local slot an `aload` at `pc` reads, or `None` for any other opcode.
pub(crate) fn lambda_aload_slot(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x19 => code.get(pc + 1).map(|&n| usize::from(n)),
        op @ 0x2a..=0x2d => Some(usize::from(op - 0x2a)),
        _ => None,
    }
}

/// The local slots the store (or `iinc`) at `pc` writes, or `None` for an
/// instruction that writes none. A `long` / `double` store writes two.
pub(crate) fn lambda_written_slots(code: &[u8], pc: usize) -> Option<(usize, usize, bool)> {
    let op = *code.get(pc)?;
    let idx = || code.get(pc + 1).map(|&n| usize::from(n));
    // `(first slot, slot count, is astore)`
    Some(match op {
        0x36 | 0x38 | 0x84 => (idx()?, 1, false),
        0x37 | 0x39 => (idx()?, 2, false),
        0x3a => (idx()?, 1, true),
        0x3b..=0x3e => (usize::from(op - 0x3b), 1, false),
        0x3f..=0x42 => (usize::from(op - 0x3f), 2, false),
        0x43..=0x46 => (usize::from(op - 0x43), 1, false),
        0x47..=0x4a => (usize::from(op - 0x47), 2, false),
        0x4b..=0x4e => (usize::from(op - 0x4b), 1, true),
        _ => return None,
    })
}

/// The `astore` of slot `slot` whose value the `aload` at `load_pc` reads, or
/// `None` unless straight-line code proves it: walking back from `load_pc`
/// (itself no branch or handler target), the first instruction that writes
/// the slot must be an `astore` of exactly it, with no branch or handler
/// target and no `wide` in between.
pub(crate) fn lambda_local_store(
    code: &[u8],
    code_len: usize,
    load_pc: usize,
    slot: usize,
    leaders: &[bool],
) -> Option<usize> {
    let code_len = code_len.min(code.len());
    if load_pc >= code_len || leaders.get(load_pc).copied().unwrap_or(true) {
        return None;
    }
    let mut starts: Vec<usize> = Vec::new();
    let mut cur = 0usize;
    while cur < load_pc {
        starts.push(cur);
        cur += crate::bytecode_analysis::step(code, cur).max(1);
    }
    if cur != load_pc {
        return None;
    }
    for (walked, &at) in starts.iter().rev().enumerate() {
        if walked >= MH_CONSTANT_RECEIVER_WINDOW || code.get(at) == Some(&0xc4) {
            return None;
        }
        if let Some((first, count, is_astore)) = lambda_written_slots(code, at) {
            if slot >= first && slot < first + count {
                return (is_astore && first == slot).then_some(at);
            }
        }
        if leaders.get(at).copied().unwrap_or(true) {
            return None;
        }
    }
    None
}

/// `CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT` -- perf, **default ON** (JIT round
/// 21 wave 19, lane lambda3fix: LAMBDA321-4 of `jit-proposals/jit-r21-lambda3-proposals-RETIRED-20261030.md`;
/// `=0` keeps such a call the inline cache): under
/// [`ir_lambda_impl_call_enabled`], a SAM call whose receiver is an `aload` of
/// a local that straight-line code cannot follow back to its `astore` (a loop
/// header lies between: `IntSupplier f = () -> k; for (..) s += f.getAsInt();`)
/// is planned as its impl call when the WHOLE method writes that local only
/// once, with an `astore` of the lambda's value outside every loop
/// ([`lambda_single_store_outside_loops`]). The builder then meets the
/// receiver as the loop header's phi of that local, and admits it only when
/// the phi resolves to the proxy it built (`IrBuilder::lambda_impl_opcode`,
/// re-checked once the back edges are patched). Read per compile (no static).
///
/// Why: `R21Lambda3Rows` / `R21TlocalLambda` `capture-apply-only` paid the
/// inline cache and the lambda thunk 16 times per kernel call (87 ns against
/// HotSpot's 4).
pub(crate) fn ir_lambda_impl_loop_slot_enabled() -> bool {
    true
}

/// JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4,
/// [`ir_lambda_impl_loop_slot_enabled`]): the pc of the ONE instruction of the
/// method that writes local `slot`, when it is an `astore` of exactly that
/// slot outside every loop (no back edge's `[target, source]` range holds it),
/// `slot` is no parameter slot (`param_slots`), and the method has no `wide`,
/// `jsr`, `jsr_w` or `ret` (operands this walk does not decode). `None`
/// otherwise.
///
/// Why that proves an `aload` of `slot` anywhere in the method reads the value
/// that store wrote in THIS activation: the verifier admits an `aload` only of
/// a local assigned on every path to it, the store is the only assignment, and
/// outside every loop it runs at most once (every cycle of the control flow
/// passes some back edge whose pc range holds each of its instructions). The
/// caller excludes methods with exception handlers, whose edges are no back
/// edges.
pub(crate) fn lambda_single_store_outside_loops(
    code: &[u8],
    code_len: usize,
    slot: usize,
    param_slots: usize,
) -> Option<usize> {
    let code_len = code_len.min(code.len());
    if slot < param_slots {
        return None;
    }
    let mut store: Option<usize> = None;
    let mut pc = 0usize;
    while pc < code_len {
        if matches!(code[pc], 0xc4 | 0xa8 | 0xc9 | 0xa9) {
            return None;
        }
        if let Some((first, count, is_astore)) = lambda_written_slots(code, pc) {
            if slot >= first && slot < first + count {
                if !is_astore || first != slot || store.is_some() {
                    return None;
                }
                store = Some(pc);
            }
        }
        pc += crate::bytecode_analysis::step(code, pc).max(1);
    }
    let store = store?;
    let in_loop = crate::bytecode_analysis::back_edges(code, code_len)
        .iter()
        .any(|&(target, source)| target <= store && store <= source);
    (!in_loop).then_some(store)
}

/// JIT round 21 (lane lambda2): the method's own lambda `invokedynamic`
/// sites a planned SAM call may stand on, `pc -> (constant-pool index,
/// capture count, the VM's impl row)`, and the stack effect of every resolved
/// `invokedynamic` (`pc -> (arguments, return tag)`, for the receiver walk).
///
/// A site qualifies when it returns a reference, has a bridge site, and the VM
/// names its static impl (`impls`, `CompileRequest::indy_lambda_impl_resolver`)
/// with the site's capture count and, capture by capture, the site's own
/// argument tags as the impl's leading parameter tags. A CAPTURING site also
/// needs the builder to allocate its proxy inline (`capture_new`: the
/// capture-new plan's switches) from the VM's `(proxy class, capture count)`
/// fact for the very proxy the impl row names -- the builder hands the call
/// the values that allocation stores. A non-capturing site's value is its
/// published singleton (the VM answers only for one), whatever arm builds it.
pub(crate) fn ir_lambda_impl_lambdas(
    req: &CompileRequest<'_>,
    resolver: &dyn Fn(u16) -> Option<(String, usize)>,
    impls: &dyn Fn(usize) -> Option<compile_request::IndyLambdaImplAnswer>,
    capture_new: bool,
    indy_ops: &[(usize, u16)],
    // j27 wave 7 (lane T1LAM3, LAMBDA321-3): each dropped site and why.
    skipped: &mut Vec<(usize, lambda_impl_site_skip::LambdaSiteSkip)>,
) -> (
    HashMap<usize, (u16, usize, compile_request::IndyLambdaImplAnswer)>,
    HashMap<usize, (usize, u8)>,
) {
    let mut lambdas = HashMap::new();
    let mut shapes: HashMap<usize, (usize, u8)> = HashMap::new();
    for &(pc, cp_idx) in indy_ops {
        let Some((descriptor, bridge_site)) = resolve_indy_site(req, resolver, cp_idx, pc) else {
            skipped.push((pc, lambda_impl_site_skip::LambdaSiteSkip::Unresolved));
            continue;
        };
        shapes.insert(
            pc,
            (count_param_slots(&descriptor), return_type(&descriptor)),
        );
        let captures = req.indy_capture_alloc_resolver.filter(|_| capture_new);
        match lambda_impl_site_skip::admit_lambda_site(&descriptor, bridge_site, captures, impls) {
            Ok((args, answer)) => {
                lambdas.insert(pc, (cp_idx, args, answer));
            }
            Err(why) => skipped.push((pc, why)),
        }
    }
    (lambdas, shapes)
}

/// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3;
/// [`ir_lambda_impl_call_enabled`]): which `invokeinterface` SAM calls of
/// `invoke_ops` the optimizing tier plans as an `invokestatic` of their
/// lambda's impl.
///
/// `lambdas` answers, per `invokedynamic` pc of the method, `(constant-pool
/// index, capture count, the VM's impl row)` for the lambda sites the builder
/// will build as a value of the proxy class (a capturing site's inline `New`,
/// a non-capturing site's published singleton). A call qualifies when:
///
/// * straight-line code proves its receiver is that value: pushed by the
///   `invokedynamic` itself, or read by an `aload` of a slot the
///   `invokedynamic`'s value was `astore`d to ([`lambda_value_producer`],
///   [`lambda_local_store`]) -- no branch or handler target anywhere from the
///   `invokedynamic` to the call;
/// * its name and descriptor are the lambda's SAM (a bridge or default method
///   of the interface is another call);
/// * the static-call gate is on, `static_call_shape` admits the impl, and the
///   impl is not this method;
/// * the site is resolved (`unresolved(pc)` false: a first-call site keeps
///   its guard on the interface call).
///
/// The calls of one constant-pool entry are admitted only when every one of
/// them answers the same impl: the plan's resolvers are keyed by the entry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_lambda_impl_plan(
    invoke_names: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    lambdas: &HashMap<usize, (u16, usize, compile_request::IndyLambdaImplAnswer)>,
    indy_shapes: &HashMap<usize, (usize, u8)>,
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
    invoke_ops: &[(usize, u16, u8)],
    static_calls: bool,
    own: (&str, &str, &str),
    unresolved: &dyn Fn(usize) -> bool,
    // JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4,
    // `CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT`): `Some((parameter slots,
    // capturing sites admitted))` admits a receiver local written once outside
    // every loop ([`lambda_single_store_outside_loops`]); `None` keeps the
    // straight-line proof alone.
    loop_slot: Option<(usize, bool)>,
) -> IrLambdaImplPlan {
    let Some(invoke_names) = invoke_names else {
        return IrLambdaImplPlan::default();
    };
    if !static_calls || lambdas.is_empty() {
        return IrLambdaImplPlan::default();
    }
    let invoke_shape = |cp_idx: u16| -> Option<(usize, bool)> {
        let (_, _, desc) = invoke_names(cp_idx)?;
        Some((count_param_slots(&desc), return_type(&desc) != b'V'))
    };
    let effect = |at: usize| -> Option<(usize, usize)> {
        if code.get(at) == Some(&0xba) {
            let &(args, ret) = indy_shapes.get(&at)?;
            return Some((args, usize::from(ret != b'V')));
        }
        mh_constant_window_effect(code, at, &invoke_shape)
    };
    let mut leaders: Option<Vec<bool>> = None;
    let mut by_cp: HashMap<u16, Option<IrLambdaImplSite>> = HashMap::new();
    for &(pc, cp_idx, opcode) in invoke_ops {
        if opcode != 0xb9 || unresolved(pc) {
            continue;
        }
        let Some((_, sam_name, sam_desc)) = invoke_names(cp_idx) else {
            continue;
        };
        let leaders: &[bool] = leaders.get_or_insert_with(|| {
            let mut map = crate::bytecode_analysis::branch_target_map(code, code_len);
            for &h in handler_pcs {
                if let Some(slot) = map.get_mut(h) {
                    *slot = true;
                }
            }
            map
        });
        let Some(producer) = lambda_value_producer(
            code,
            code_len,
            pc,
            count_param_slots(&sam_desc),
            leaders,
            &effect,
        ) else {
            continue;
        };
        // LAMBDA321-4: the receiver local, when only the whole-method
        // single-store proof found its `astore`.
        let mut via_slot: Option<usize> = None;
        let indy_pc = if code.get(producer) == Some(&0xba) {
            producer
        } else {
            let Some(slot) = lambda_aload_slot(code, producer) else {
                continue;
            };
            let store = match lambda_local_store(code, code_len, producer, slot, leaders) {
                Some(store) => store,
                None => {
                    let Some(store) =
                        loop_slot
                            .filter(|_| handler_pcs.is_empty())
                            .and_then(|(params, _)| {
                                lambda_single_store_outside_loops(code, code_len, slot, params)
                            })
                    else {
                        continue;
                    };
                    via_slot = Some(slot);
                    store
                }
            };
            let Some(at) = lambda_value_producer(code, code_len, store, 0, leaders, &effect) else {
                continue;
            };
            if code.get(at) != Some(&0xba) {
                continue;
            }
            at
        };
        let Some((indy_cp, captures, answer)) = lambdas.get(&indy_pc) else {
            continue;
        };
        if via_slot.is_some() && *captures > 0 && !loop_slot.is_some_and(|(_, c)| c) {
            continue;
        }
        let admitted = answer.sam_name == sam_name
            && answer.sam_descriptor == sam_desc
            && answer.num_captures == *captures
            && answer.class_id != 0
            && count_param_slots(&answer.descriptor) == captures + count_param_slots(&sam_desc)
            && static_call_shape(&answer.descriptor).is_some()
            && (
                answer.class.as_str(),
                answer.name.as_str(),
                answer.descriptor.as_str(),
            ) != own;
        if let (true, Some(slot)) = (admitted, via_slot) {
            if x64::dbg_jitc_enabled() || ir_stage_reporting() {
                eprintln!(
                    "[ir] lambda-impl-loop-slot {}.{}{} pc={pc} indy_pc={indy_pc} slot={slot} \
                     captures={captures} (CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT)",
                    own.0, own.1, own.2,
                );
            }
        }
        let site = admitted.then(|| IrLambdaImplSite {
            pcs: vec![(pc, indy_pc)],
            class: answer.class.clone(),
            name: answer.name.clone(),
            desc: answer.descriptor.clone(),
            class_id: answer.class_id,
            captures: *captures,
        });
        match by_cp.entry(*indy_cp) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(site);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let same = match (slot.get(), &site) {
                    (Some(a), Some(b)) => {
                        (&a.class, &a.name, &a.desc, a.class_id, a.captures)
                            == (&b.class, &b.name, &b.desc, b.class_id, b.captures)
                    }
                    _ => false,
                };
                match slot.get_mut() {
                    Some(s) if same => s.pcs.push((pc, indy_pc)),
                    other => *other = None,
                }
            }
        }
    }
    IrLambdaImplPlan {
        by_cp: by_cp
            .into_iter()
            .filter_map(|(cp_idx, site)| site.map(|s| (cp_idx, s)))
            .collect(),
    }
}

/// Would splicing `site` into an optimizing-tier body lose a String or
/// StringBuilder call-site intrinsic the single-pass backend applies to the
/// callee's own body? `true` when the callee (or a body nested in it) makes
/// a virtual call [`try_resolve_string_intrinsic`] matches, other than the
/// three field accessors (`length`, `isEmpty`, `charAt`), whose fallback in a
/// splice is a cheap compiled call. The predicate is the method-level pin's
/// (`string_intrinsic_pin_verdict`), asked of the callee's resolved targets.
pub(crate) fn splice_loses_string_intrinsic(
    site: &InlineSite,
    layout: Option<StringFieldLayout>,
) -> bool {
    let Some(layout) = layout else { return false };
    site.nested_sites.iter().any(|nested| {
        let loses_here = site
            .invoke_targets
            .iter()
            .find(|(pc, _)| *pc == nested.callee_pc)
            .is_some_and(|(_, target)| string_target_loses_intrinsic(target, layout));
        loses_here
            || splice_loses_string_intrinsic(&nested.site, Some(layout))
            || nested
                .bimorphic_second
                .as_ref()
                .is_some_and(|second| splice_loses_string_intrinsic(&second.site, Some(layout)))
    })
}

/// Whether recursively splicing this String call would discard the call's own
/// String intrinsic implementation. A call that stays in `Op::Call` retains
/// the normal compiled Java method and its intrinsic sites.
pub(crate) fn string_target_loses_intrinsic(
    target: &InlineInvokeTarget,
    layout: StringFieldLayout,
) -> bool {
    let accessor = |entry: usize| {
        entry == JitIntrinsic::StringLength.as_entry()
            || entry == JitIntrinsic::StringIsEmpty.as_entry()
            || entry == JitIntrinsic::StringCharAt.as_entry()
    };
    matches!(target.invoke_kind, 0 | 2)
        && try_resolve_string_intrinsic(
            &target.class_name,
            &target.method_name,
            &target.descriptor,
            Some(layout),
        )
        .is_some_and(|(entry, ..)| !accessor(entry))
        // nio20 wave 3 (L2): a search member the optimizing tier's inline
        // prefix answers is not lost by splicing -- the prefix is a lowering
        // of the spliced Op::Call like any other.
        && !ir_string_search_prefix_handled_kind(
            &target.class_name,
            &target.method_name,
            &target.descriptor,
        )
        .is_some_and(|k| k.admits_invoke_kind(target.invoke_kind))
}

/// Keep calls to String intrinsics as ordinary invoke sites while allowing
/// their surrounding helper bodies to splice. The invoke metadata and
/// bytecode stay intact; only the nested body that would lose the intrinsic
/// is removed from the splice plan.
pub(crate) fn ir_splice_strip_string_intrinsic_nested(
    site: &mut InlineSite,
    layout: Option<StringFieldLayout>,
) -> usize {
    let Some(layout) = layout else { return 0 };
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    let mut dropped = 0usize;
    for mut n in nested {
        let loses_here = site
            .invoke_targets
            .iter()
            .find(|(pc, _)| *pc == n.callee_pc)
            .is_some_and(|(_, target)| string_target_loses_intrinsic(target, layout));
        if loses_here {
            dropped += 1;
            continue;
        }
        dropped += ir_splice_strip_string_intrinsic_nested(&mut n.site, Some(layout));
        if let Some(second) = &mut n.bimorphic_second {
            dropped += ir_splice_strip_string_intrinsic_nested(&mut second.site, Some(layout));
        }
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}

pub use crate::ir::ir_hashmap_get_inline_enabled;
