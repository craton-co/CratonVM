#[allow(unused_imports)]
use super::super::*;

/// JIT round 19 proposal wave 5 (lane mega5, MEGA5-1 of
/// `jit-r19-mega5-proposals.md`): `CRATONVM_JIT_SP_NESTED_GUARD_STRICT_SHARE`, default ON
/// since the round-19 proposal round (`=0` turns it off; iface-nested-mono 69 -> 6 ns single-pass).
/// The SINGLE-PASS nested resolver (`jit_bridge` `nest-virtual`, the VM's
/// `0 | 2` arm) plans a guarded nested splice from the callee's own receiver
/// row only when its dominant class holds at least
/// [`SP_NESTED_GUARD_STRICT_SHARE_PCT`] of it (80 % with the switch off).
///
/// Why the single-pass tier only: that guard's miss edge
/// (`x64::inlining` `emit_guarded_nested_inline`) is the BLIND dispatch helper
/// (`emit_inline_dispatch_call`: no MIC, no PIC), measured 2026-08-18 at
/// ~175 ns a call against ~4 ns for a bound call, so a 20 % miss share costs
/// ~35 ns a call on average. Refusing the row costs the enclosing splice
/// instead (the body is then refused as "neither spliced nor direct-bound")
/// and the call reaches the callee's own compiled body, whose site has its
/// own seeded inline cache. The optimizing tier's nested guard keeps its
/// inline-cached call on the miss edge and is not affected. Answers cannot
/// change: a row only chooses between a guarded splice and a call. Read once
/// per nested virtual / interface site at resolution time.
pub fn sp_nested_guard_strict_share_enabled() -> bool {
    true
}

/// The share (percent) [`sp_nested_guard_strict_share_enabled`] asks of a
/// single-pass nested guard's class: a miss rate of 3 % at ~175 ns per miss is
/// ~5 ns a call, about what the splice saves.
pub const SP_NESTED_GUARD_STRICT_SHARE_PCT: u32 = 97;

/// JIT round 19 proposal wave 5 (lane mega5; review note of
/// `docs/feature-designs/deopt-osr.md` section 3):
/// `CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_DECLARING_KEY=1`. MEGAOSR19-1's decline
/// ([`osr_door_blind_megamorphic_splice_site`]) looks the callee's profile up
/// under `InlineSite::class_id`, which for a constant-pool resolution is the
/// CONSTANT-POOL class, while the interpreter records the profile under the
/// DECLARING class. So `Sub.m()` declared in `Super` was never declined. On:
/// the VM's profile lookup (`jit_bridge` `osr_door_blind_megamorphic_splice_early`)
/// also tries the declaring class of the selected method when the
/// constant-pool key has no profile. Performance only (the decline picks
/// between two correct bodies). Read once per looked-up callee, only while
/// the decline itself is on.
pub fn osr_decline_blind_mega_declaring_key_enabled() -> bool {
    true
}

/// JIT round 19 proposal wave 5 (lane mega5), under
/// [`ir_splice_callee_profile_seed_enabled`]:
/// `CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED_MEGA=1` restores the
/// MEGAOSR19-2 seed's MEGAMORPHIC half (`JitPICSlot::seed_megamorphic_from_profile`
/// on a spliced site); without it the seed sets the MIC's dominant class only.
///
/// Why it was split off (orchestrator measurement, integration i7):
/// `R19Megaosr4Seeded own-site-wide-call` went 27 -> 150 ns under
/// `CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_SPLICE=0` + the seed, while the seed
/// moved no blind row. The own site is not spliced and its own seed is
/// unchanged, so the cost comes through state the two sites SHARE: the
/// selector's [`inline_cache_pic::MegaDispatchTable`] ways and cells and the
/// callees' supersede trail. Read (not measured): a slot seeded megamorphic
/// at birth publishes every receiver to the hashed ways / cell / shared table
/// from the first call, i.e. while the wide `visit` bodies are still their
/// first tier; their tier-up then RETIRES those shared publications, and a
/// retirement is graced only when every thread returns to compiled depth 0,
/// which a thread inside an OSR loop (both loops run optimizing OSR bodies
/// in that arm) never does (`same_key_refill_enabled`'s note). The own site's
/// receivers then miss the shared table to the helper on every call (~150 ns
/// is a helper round trip). Unseeded, the spliced slot's first receivers fill
/// its OWN inline ways, and the shared keys are first written after the
/// callees tiered up. Positive control: this switch ON must reproduce the
/// 150 ns row; OFF must not. Read live, once per planned spliced site.
pub fn ir_splice_callee_profile_seed_mega_enabled() -> bool {
    true
}

/// What [`ir_seed_spliced_inline_cache`] did to a spliced site's cache, for
/// the `ir-splice-callee-seed` DBG_JITC line and the unit tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SplicedSeedOutcome {
    /// The dominant receiver's class id put in the MIC, if any.
    pub(crate) dominant: Option<u32>,
    /// Whether the PIC slot was seeded megamorphic.
    pub(crate) megamorphic: bool,
    /// Distinct receiver classes in the callee's row.
    pub(crate) classes: usize,
}

/// MEGAOSR19-2: seed a spliced site's freshly built MIC / PIC pair from the
/// callee's receiver row `counts`, in the order the caller's own sites are
/// seeded (MIC first, then `seed_from_mic`, then the megamorphic seed). The
/// caller has NOT yet called `seed_from_mic`; this does. Returns what it set.
pub(crate) fn ir_seed_spliced_inline_cache(
    mic: &JitMICSlot,
    pic: &JitPICSlot,
    counts: Option<&profile::ReceiverCounts>,
) -> SplicedSeedOutcome {
    let dominant = counts.and_then(|c| profile::dominant_receiver(c, 80));
    if let Some(dom) = dominant {
        mic.prepopulate(dom);
    }
    // As at the caller's own sites: the MIC holds a class id only, so the
    // policy argument is unreachable (see the own-site comment).
    pic.seed_from_mic(mic, false);
    // JIT round 19 proposal wave 5 (lane mega5): the megamorphic half only
    // under its own opt-in (`ir_splice_callee_profile_seed_mega_enabled`).
    let megamorphic = ir_splice_callee_profile_seed_mega_enabled()
        && counts.is_some_and(|c| pic.seed_megamorphic_from_profile(c));
    SplicedSeedOutcome {
        dominant,
        megamorphic,
        classes: counts.map_or(0, |c| c.len()),
    }
}

/// A blind megamorphic site an optimizing OSR body entered at `entry_pc` would
/// splice (MEGAOSR19-1, [`osr_decline_blind_mega_splice_enabled`]):
/// `Some((call_pc, callee_pc))` names the loop's statically bound call at
/// `call_pc` and the `invokevirtual` / `invokeinterface` at `callee_pc` of its
/// resolved body whose receiver profile, in the CALLEE's own method profile,
/// [`inline_cache_pic::profile_names_megamorphic_site`] calls megamorphic (more
/// classes than the four inline ways, the overflow past them at least a
/// sixteenth). `None` with the switch off, when `entry_pc` is in no loop, or
/// when no such site exists.
///
/// * `static_candidates`: the method's own `invokestatic` / `invokespecial`
///   sites (kinds 3 / 1, as [`osr_door_single_pass_polymorphic_splice_pc`]
///   takes them); a candidate counts only inside a loop the entry names
///   ([`osr_entry_loop_ranges`]) and only at its own opcode.
/// * `static_resolve`: the VM's static splice resolver (the body the optimizing
///   planner would splice).
/// * `callee_profile(class id, method, descriptor)`: the callee's own method
///   profile (the VM's profile store), asked with `InlineSite::class_id` --
///   for an unguarded resolution the CONSTANT-POOL class, while profiles are
///   recorded under the declaring class; under
///   [`osr_decline_blind_mega_declaring_key_enabled`] the VM's lookup also
///   tries the declaring class (an inherited static).
///
/// One level deep on purpose: it names the shape the page measured, and a
/// deeper body is spliced only if this level was. A superset of what the
/// planner would splice (size tiers and budget are not asked): a decline the
/// planner would not have needed costs the loop the optimizing body, never an
/// answer.
pub fn osr_door_blind_megamorphic_splice_site(
    code: &[u8],
    code_len: usize,
    entry_pc: usize,
    static_candidates: &[OsrSpliceCandidate<'_>],
    static_resolve: &dyn Fn(&str, &str, &str) -> Option<InlineSite>,
    callee_profile: &dyn Fn(u32, &str, &str) -> Option<profile::MethodProfile>,
) -> Option<(usize, usize)> {
    if !osr_decline_blind_mega_splice_enabled() {
        return None;
    }
    let len = code_len.min(code.len());
    let ranges = osr_entry_loop_ranges(code, code_len, entry_pc);
    if ranges.is_empty() {
        return None;
    }
    for c in static_candidates {
        let own_opcode = matches!(
            (c.invoke_kind, code.get(c.pc).copied()),
            (3, Some(0xb8)) | (1, Some(0xb7))
        );
        if c.pc >= len
            || !own_opcode
            || !ranges
                .iter()
                .any(|&(header, back)| header <= c.pc && c.pc <= back)
        {
            continue;
        }
        let Some(site) = static_resolve(c.class_name, c.method_name, c.descriptor) else {
            continue;
        };
        let nested = osr_decline_blind_mega_nested_enabled();
        if let Some(callee_pc) =
            osr_blind_megamorphic_site_in_body(&site, callee_profile, nested, 0)
        {
            return Some((c.pc, callee_pc));
        }
    }
    None
}

/// JIT round 19 proposal wave 6 (lane osrentry6, MEGA5-3 of
/// `jit-r19-mega5-proposals.md`, the depth-walk half):
/// `CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_NESTED=1`. MEGAOSR19-1's decline
/// ([`osr_door_blind_megamorphic_splice_site`]) asks only the loop's directly
/// called body; with this on it also walks the bodies the resolver nested into
/// it (`InlineSite::nested_sites`, and a nested row's `bimorphic_second`), so a
/// megamorphic site two levels down (`run` -> static `a` -> static `b` ->
/// `v.visit`) declines the optimizing OSR body too. Each nested body is asked
/// under its OWN profile key, as the first level is. Performance only (the
/// decline picks between two correct bodies). Default OFF; read live, once per
/// resolved loop call, only while the decline itself is on.
pub fn osr_decline_blind_mega_nested_enabled() -> bool {
    false
}

/// The deepest nested body [`osr_blind_megamorphic_site_in_body`] walks. The
/// resolver already bounds the tree (`MAX_INLINE_NEST_DEPTH`); this only keeps
/// the walk finite whatever a resolver hands it.
pub(crate) const OSR_BLIND_MEGA_NESTED_MAX_DEPTH: usize = 8;

/// The callee pc, in `site`'s own code or (with `nested`) in a body nested
/// into it, of an `invokevirtual` / `invokeinterface` whose receiver row in
/// THAT body's own profile [`inline_cache_pic::profile_names_megamorphic_site`]
/// calls megamorphic. `class_id == 0` (the resolver's unknown-class sentinel)
/// has no profile key, so that body's own sites are not asked; its nested
/// bodies still are. Bodies first, then their nested rows in order.
pub(crate) fn osr_blind_megamorphic_site_in_body(
    site: &InlineSite,
    callee_profile: &dyn Fn(u32, &str, &str) -> Option<profile::MethodProfile>,
    nested: bool,
    depth: usize,
) -> Option<usize> {
    if site.class_id != 0 {
        let callee_len = site.callee_code_len.min(site.callee_code.len());
        let virtual_pcs: Vec<usize> = osr_door_own_invoke_ops(&site.callee_code[..callee_len])
            .into_iter()
            .filter(|&(_, _, op)| matches!(op, 0xb6 | 0xb9))
            .map(|(pc, _, _)| pc)
            .collect();
        if !virtual_pcs.is_empty() {
            if let Some(profile) =
                callee_profile(site.class_id, &site.method_name, &site.descriptor)
            {
                // JIT round 21, lane revjit2: the published body's slots hold
                // the MIC plus four ways under `CRATONVM_JIT_IC_MIC_PLUS_FOUR`
                // (five classes inline), the capacity its own profile seed
                // counts (`JitPICSlot::seed_megamorphic_from_profile`).
                let inline_capacity = inline_cache_pic::JIT_PIC_ENTRIES
                    + usize::from(inline_cache_pic::ic_mic_plus_four_enabled());
                for callee_pc in virtual_pcs {
                    if profile.receivers.get(&callee_pc).is_some_and(|counts| {
                        inline_cache_pic::profile_names_megamorphic_site_over(
                            counts,
                            inline_capacity,
                        )
                    }) {
                        return Some(callee_pc);
                    }
                }
            }
        }
    }
    if !nested || depth >= OSR_BLIND_MEGA_NESTED_MAX_DEPTH {
        return None;
    }
    for n in &site.nested_sites {
        let rows = std::iter::once(&n.site).chain(n.bimorphic_second.as_deref().map(|b| &b.site));
        for body in rows {
            if let Some(pc) =
                osr_blind_megamorphic_site_in_body(body, callee_profile, nested, depth + 1)
            {
                return Some(pc);
            }
        }
    }
    None
}

/// `(pc, constant-pool index, opcode)` of every `invokevirtual` (0xb6),
/// `invokespecial` (0xb7), `invokestatic` (0xb8) and `invokeinterface` (0xb9)
/// instruction of `code`, walked by instruction so an operand byte is never
/// read as an opcode. A truncated last instruction is skipped. The VM's
/// optimizing OSR door builds [`osr_door_single_pass_polymorphic_splice_pc`]'s
/// candidates from these.
pub fn osr_door_own_invoke_ops(code: &[u8]) -> Vec<(usize, u16, u8)> {
    let mut ops = Vec::new();
    let mut pc = 0usize;
    while let Some(&op) = code.get(pc) {
        if matches!(op, 0xb6..=0xb9) {
            if let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) {
                ops.push((pc, u16::from_be_bytes([hi, lo]), op));
            }
        }
        // `max(1)`: the walk terminates whatever the decoder answers.
        pc += bytecode_insn_len(code, pc).max(1);
    }
    ops
}

/// Plan the guarded splices of a single-pass OSR body's in-loop virtual and
/// interface sites into `plan` (see the section comment above). Runs after
/// [`plan_osr_door_splices`] and charges the same per-method budget
/// (`plan.budget_spent`).
///
/// `candidates` are the body's dynamically dispatched sites (`invoke_kind` 0
/// at an `invokevirtual`, 2 at an `invokeinterface`; anything else is skipped).
/// `class_namer` names a receiver class id; `receiver_resolve(class id, cp
/// class, name, descriptor)` is the VM's receiver-class body resolver. Nothing
/// is planned outside a loop or with the switch off. With
/// `precise_exception_frames` (round nio20 wave 4, lane L3, W3L3-2), under
/// [`sp_splice_precise_per_site_enabled`], a site outside every protected
/// range plans as without a table (the guard's miss edge is the site's
/// unchanged dispatch, whose exception check takes the frame-less exit at an
/// unprotected pc, and the body lowers as without precise frames); a
/// protected site is refused (`plan_inline`'s
/// `SpeculationInsideProtectedRange`, and a virtual body is never one that
/// cannot throw). With the switch off nothing is planned under precise
/// frames, as before.
#[allow(clippy::too_many_arguments)]
pub fn plan_osr_door_guarded_splices(
    plan: &mut OsrSplicePlan,
    code: &[u8],
    code_len: usize,
    enclosing: (&str, &str, &str),
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    precise_exception_frames: bool,
    profile: Option<&profile::MethodProfile>,
    osr_entry_pc: Option<usize>,
    candidates: &[OsrSpliceCandidate<'_>],
    class_namer: &dyn Fn(u32) -> Option<String>,
    receiver_resolve: &dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>,
) {
    plan_osr_door_guarded_splices_bound(
        plan,
        code,
        code_len,
        enclosing,
        exception_table,
        precise_exception_frames,
        profile,
        osr_entry_pc,
        candidates,
        class_namer,
        receiver_resolve,
        None,
    );
}

/// [`plan_osr_door_guarded_splices`] with `super_bound_resolve`: the same
/// receiver resolver handed the door's direct binder (j27 wave 4, lane
/// OSRSUPER). Under [`osr_guarded_super_bind::osr_guarded_super_bind_enabled`]
/// a body the plain resolver refused is asked of it, and kept only when every
/// call left in it is a bound `super.m()`
/// ([`osr_guarded_super_bind::osr_guarded_receiver_body`]). `None`, or the
/// switch off, is exactly [`plan_osr_door_guarded_splices`].
#[allow(clippy::too_many_arguments)]
pub fn plan_osr_door_guarded_splices_bound(
    plan: &mut OsrSplicePlan,
    code: &[u8],
    code_len: usize,
    enclosing: (&str, &str, &str),
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    precise_exception_frames: bool,
    profile: Option<&profile::MethodProfile>,
    osr_entry_pc: Option<usize>,
    candidates: &[OsrSpliceCandidate<'_>],
    class_namer: &dyn Fn(u32) -> Option<String>,
    receiver_resolve: &dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>,
    super_bound_resolve: Option<osr_guarded_super_bind::ReceiverResolve<'_>>,
) {
    let super_bound = super_bound_resolve
        .filter(|_| osr_guarded_super_bind::osr_guarded_super_bind_enabled());
    if !osr_door_guarded_splices_enabled()
        || (precise_exception_frames && !sp_splice_precise_per_site_enabled())
    {
        return;
    }
    let (enclosing_class, enclosing_method, enclosing_desc) = enclosing;
    let hot_loops = osr_door_hot_loops(code, code_len, profile, osr_entry_pc);
    let total_budget = if hot_loops.is_empty() {
        MAX_INLINE_BUDGET
    } else {
        MAX_INLINE_BUDGET_HOT
    };
    let polymorphic = true;
    let dbg = crate::x64::dbg_jitc_enabled();
    for c in candidates {
        let guarded_arm = c.pc < code_len
            && matches!(
                (c.invoke_kind, code.get(c.pc).copied()),
                (0, Some(0xb6)) | (2, Some(0xb9))
            );
        if !guarded_arm
            || !bytecode_pc_in_loop(code, code_len, c.pc)
            || plan.inline_sites.contains_key(&c.pc)
        {
            continue;
        }
        if jit_force_interpret(c.class_name, c.method_name) {
            plan.tally.record_refusal(&InlineRefusal::ForceInterpreted);
            continue;
        }
        // W3L3-2: under precise frames only an unprotected site is planned.
        if precise_exception_frames && pc_in_protected_range(exception_table, c.pc) {
            plan.tally
                .record_refusal(&InlineRefusal::PreciseExceptionFrames);
            continue;
        }
        // Round nio20 wave 3 (L3W2-6): the budget left, plus the hot-loop
        // overdraft the statically bound pass left for a site in the hot loop.
        let base_left = total_budget.saturating_sub(plan.budget_spent);
        let budget = base_left.saturating_add(osr_door_overdraft_for(plan, &hot_loops, c.pc));
        if budget == 0 {
            plan.tally
                .record_refusal(&InlineRefusal::BudgetAlreadySpent);
            continue;
        }
        let receiver_body = |cid: u32| {
            osr_guarded_super_bind::osr_guarded_receiver_body(receiver_resolve, super_bound, cid, c)
        };
        let site_is_hot = call_site_is_hot(c.pc, &hot_loops, profile);
        let recursive_copies = usize::from(
            c.class_name == enclosing_class
                && c.method_name == enclosing_method
                && c.descriptor == enclosing_desc,
        );
        let ask = |receivers: Option<&profile::ReceiverCounts>, budget_remaining: usize| {
            plan_inline(&InlineRequest {
                pc: c.pc,
                invoke_kind: c.invoke_kind,
                site_method_name: c.method_name,
                site_descriptor: c.descriptor,
                site_class_name: c.class_name,
                // A guarded site's bodies come from the receiver classes,
                // never from the constant pool (see `InlineRequest::site`).
                site: None,
                site_is_hot,
                receivers,
                call_site_evidence: profile
                    .map(|p| p.call_site_count(c.pc))
                    .unwrap_or(profile::CallSiteEvidence::None),
                depth: 1,
                recursive_copies,
                budget_remaining,
                pc_in_protected_range: pc_in_protected_range(exception_table, c.pc),
                // W3L3-2: answered for this site above (only an unprotected
                // site reaches here under precise frames).
                precise_exception_frames: false,
                caps: InlineBackendCaps {
                    guarded_inline_body_at_virtual_sites: true,
                    ..InlineBackendCaps::single_pass_x64()
                },
                receiver_class_namer: Some(class_namer),
                receiver_callee_resolver: Some(&receiver_body),
                // No CHA: an OSR'd loop always has a receiver profile, and an
                // unprofiled site is left to its inline cache.
                unique_concrete_resolver: None,
            })
        };
        let decided = ask(profile.and_then(|p| p.receivers.get(&c.pc)), budget);
        let polymorphic_ask = polymorphic
            && matches!(
                decided.refusal(),
                Some(InlineRefusal::ReceiverNotDominant { .. })
            );
        let planned = if decided.is_admitted() && decided.is_speculative() {
            plan.tally.record(&decided);
            Some((
                decided.speculative_sites.clone(),
                decided.invalidation_triples(),
                decided.expansion_cost,
                "guarded",
            ))
        } else if polymorphic_ask {
            match osr_door_polymorphic_bodies(profile, c.pc, budget, &ask) {
                Some((first_plan, variants, triples, cost)) => {
                    plan.tally.record(&first_plan);
                    Some((variants, triples, cost, "polymorphic"))
                }
                None => {
                    plan.tally.record(&decided);
                    None
                }
            }
        } else {
            plan.tally.record(&decided);
            None
        };
        let Some((variants, triples, cost, how)) = planned else {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] osr-splice-guarded-refused {}.{}{} @pc={}: {:?}{}",
                    c.class_name,
                    c.method_name,
                    c.descriptor,
                    c.pc,
                    decided.verdict,
                    osr_guarded_refusal_hint::osr_guarded_refusal_hint(decided.refusal())
                );
            }
            continue;
        };
        let Some((_, primary)) = variants.first().cloned() else {
            continue;
        };
        // The part of `cost` the budget could not pay came out of the
        // overdraft (`budget` above admitted it only for this site).
        plan.hot_loop_overdraft_left = plan
            .hot_loop_overdraft_left
            .saturating_sub(cost.saturating_sub(base_left));
        plan.budget_spent = plan.budget_spent.saturating_add(cost);
        if variants.iter().any(|(_, s)| s.needs_heap) {
            plan.needs_heap = true;
        }
        for dependency in triples {
            if !plan.inlined_methods.contains(&dependency) {
                plan.inlined_methods.push(dependency);
            }
        }
        if dbg {
            let classes: Vec<u32> = variants.iter().map(|(cid, _)| *cid).collect();
            eprintln!(
                "[cratonvm-jitc] osr-splice-guarded-planned {}.{}{} @pc={} how={how} \
                 classes={classes:?} cost={cost} budget_left={}",
                c.class_name,
                c.method_name,
                c.descriptor,
                c.pc,
                total_budget.saturating_sub(plan.budget_spent),
            );
        }
        plan.inline_sites.insert(c.pc, primary);
        plan.guard_variants.insert(c.pc, variants);
        if how == "polymorphic" {
            plan.polymorphic_pcs.push(c.pc);
        }
    }
}

/// The polymorphic arm of [`plan_osr_door_guarded_splices`]: the classes
/// [`ir::profile_polymorphic_receiver_classes`] names for `pc` (three or four,
/// best first, the optimizing tier's bars), each admitted by `ask` -- the
/// site's own `plan_inline` -- as a ONE-class guarded splice, so every body
/// passes the same naming, resolution, protected-range, size-tier and budget
/// rules a Monomorphic plan does. The share bars were already applied to the
/// real profile by `polymorphic_set`; the one-class profile handed to `ask`
/// carries [`INLINE_MIN_SPECULATION_OBSERVATIONS`] samples of that class only,
/// which is what makes the policy answer for that body alone. The first
/// refusal ends the list (the rest take the dispatch); fewer than two bodies
/// plan nothing. Returns the first body's plan (for the tally), the variants,
/// the dependencies and the total cost.
#[allow(clippy::type_complexity)]
pub(crate) fn osr_door_polymorphic_bodies(
    profile: Option<&profile::MethodProfile>,
    pc: usize,
    budget: usize,
    ask: &dyn Fn(Option<&profile::ReceiverCounts>, usize) -> InlinePlan,
) -> Option<(
    InlinePlan,
    Vec<(u32, InlineSite)>,
    Vec<(String, String, String)>,
    usize,
)> {
    let classes = ir::profile_polymorphic_receiver_classes(profile, pc)?;
    let mut first: Option<InlinePlan> = None;
    let mut variants: Vec<(u32, InlineSite)> = Vec::with_capacity(classes.len());
    let mut triples: Vec<(String, String, String)> = Vec::new();
    let mut cost = 0usize;
    for class_id in classes {
        let mut one = profile::ReceiverCounts::default();
        one.insert(class_id, INLINE_MIN_SPECULATION_OBSERVATIONS);
        let p = ask(Some(&one), budget.saturating_sub(cost));
        if !p.is_admitted() || !p.is_speculative() || p.speculative_sites.len() != 1 {
            break;
        }
        cost = cost.saturating_add(p.expansion_cost);
        variants.extend(p.speculative_sites.iter().cloned());
        for t in p.invalidation_triples() {
            if !triples.contains(&t) {
                triples.push(t);
            }
        }
        if first.is_none() {
            first = Some(p);
        }
    }
    if variants.len() < 2 {
        return None;
    }
    Some((first?, variants, triples, cost))
}


/// Round 16 wave 13 (lane mega): the single-pass OSR door's guarded splices
/// (`plan_osr_door_guarded_splices`).


/// The entry cell behind a compile-time recursion cycle's CLOSING edge (round
/// 9 wave 7, lane `mutrec7`; the design is the "Status after wave 4" section of
/// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch-20260918.md`).
///
/// # The problem it solves
///
/// `a -> b -> a` compiled eagerly: `b` is compiled INSIDE `a`'s compile, so its
/// call back to `a` targets a method with no entry yet. That edge used to stay
/// on `jit_invoke_dispatch` for the life of `b`'s body, and it is the only
/// non-baked edge on the cycle, so every traversal of the recursion paid the
/// whole Rust dispatch round trip (7.9x on `MutualRec`).
///
/// # How
///
/// The closing caller's single-pass body gets one of these per closing site,
/// owned by the artifact (`CompiledMethod::_jit_cycle_cells`) exactly as its
/// inline-cache slots are, and bakes the cell's ADDRESS. The site emits:
///
/// ```text
///   <stack-floor guard, the self-call one>      ; SOE instead of a native overflow
///   <arguments staged for the direct entry ABI + copied to the service range>
///   mov  r11, cell ; mov r11, [r11] ; test r11, r11 ; jnz .call
///   <jit_invoke_dispatch ABI, args buffer = the service range>
///   mov  r11, jit_invoke_dispatch
/// .call:
///   call r11                                    ; ONE return address, ONE oop map
/// ```
///
/// [`JitCache::put`] fills the cell with the entry of every suitable body
/// published for the target key (the newest wins), and
/// `JitCache::invalidate_matching_collecting` / [`JitCache::clear_all`] clear
/// it again.
///
/// # Why this is sound
///
/// * **Lifetime.** The cell holds an owning `Arc` of the body its word names,
///   exactly as [`JitMICSlot`] does, and a replaced or cleared owner goes
///   through the retirement queue ([`defer_jit_owner`]). The word is loaded
///   ONCE, immediately before the `CALL` that uses it, with no blocking point in
///   between, so a thread that loaded it is inside compiled code until the
///   callee's frame exists, and the quiescence proof cannot free the body under
///   it. (The wave-4 design reloaded the word after the guard and so needed the
///   cell to be monotone; loading once, late, removes that requirement, and the
///   word may go back to `0`.)
/// * **Stack depth.** The baked edges of the cycle all point at OLDER
///   artifacts (see [`jit_cycle_direct_bind_enabled`]), so unbounded recursion
///   must cross a cell edge at least once per traversal, and every cell edge
///   runs the self-call stack-floor guard first. `MutualSoe` still throws.
/// * **ABI.** Only a body whose [`CompiledMethod::needs_context`] matches the
///   convention the site staged, that needs no wrapped entry, and that is not
///   retired is ever installed. Anything else leaves the word as it was (still
///   a valid, older body, or `0` = dispatch).
/// * **Identity.** Keyed by (class, method, descriptor, declaring class id),
///   the id taken from the call site's own constant-pool resolution, so a
///   same-named class of another loader can never fill it.
#[repr(C)]
pub struct JitCycleEdgeCell {
    /// The callee entry, or `0` = unresolved (the site dispatches).
    /// **JIT-hot — offset [`JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET`].**
    pub(crate) entry_word: std::sync::atomic::AtomicU64,
    /// Keeps the body `entry_word` names mapped. Its lock is also the writer
    /// lock for `entry_word`.
    pub(crate) owner: parking_lot::Mutex<Option<Arc<CompiledMethod>>>,
    pub(crate) class_name: Arc<str>,
    pub(crate) method_name: Arc<str>,
    pub(crate) descriptor: Arc<str>,
    pub(crate) declaring_class_id: cratonvm_types::ClassId,
    /// The entry ABI the site stages (`true` = the VM context pointer in the
    /// first argument register).
    pub(crate) needs_context: bool,
    /// True for IR-tier cells whose call site strips `JIT_IC_NEEDS_CONTEXT_TAG`
    /// and marshals both `needs_context == true` and `needs_context == false`.
    pub(crate) any_abi: bool,
    /// Round 9 wave 9 (`evict9`): non-zero for a RETIRE cell
    /// ([`JIT_RETIRE_CELL_GUARD`], [`retirable_direct_call`]) -- the one entry
    /// the cell may ever hold. Such a cell is never a key's waiting cell: it
    /// is filled once, at its caller's publication, with the body its caller
    /// baked, and cleared when that body (or any body of the method its names
    /// give) is withdrawn. `0` for a cycle-edge cell.
    pub(crate) pinned_entry: usize,
}

/// Review rev-x64 item 6 (JIT round 21 wave 5, lane codecache): whoever drops a
/// cell last -- the owning body's field drop, or a thread that upgraded it from
/// a registry and installed into it meanwhile -- releases its owner through the
/// retirement queue, never as a bare `Arc` (`CRATONVM_JIT_IC_OWNERS_THROUGH_QUEUE`,
/// `=0`: the bare drop, as before).
impl Drop for JitCycleEdgeCell {
    fn drop(&mut self) {
        let Some(owner) = self.owner.get_mut().take() else {
            return;
        };
        {
            defer_jit_owner(Some(owner));
        }
    }
}

impl JitCycleEdgeCell {
    /// A fresh, unresolved cell for a call to `(class, method, descriptor)`
    /// declared by `declaring_class_id`.
    pub fn new(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
        needs_context: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            entry_word: std::sync::atomic::AtomicU64::new(0),
            owner: parking_lot::Mutex::new(None),
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            declaring_class_id,
            needs_context,
            any_abi: false,
            pinned_entry: 0,
        })
    }

    /// An unresolved cell for an IR-tier site that accepts both context-taking
    /// and context-free compiled bodies via `JIT_IC_NEEDS_CONTEXT_TAG`.
    pub fn new_any_abi(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Arc<Self> {
        Arc::new(Self {
            entry_word: std::sync::atomic::AtomicU64::new(0),
            owner: parking_lot::Mutex::new(None),
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            declaring_class_id,
            needs_context: true,
            any_abi: true,
            pinned_entry: 0,
        })
    }

    /// A RETIRE cell for a baked direct call to `entry`, a body of
    /// `(class, method, descriptor)` (round 9 wave 9, `evict9`; see
    /// [`retirable_direct_call`]). Starts at `0`; [`JitCache::put`] /
    /// [`JitCache::put_osr`] fill it from the caller's own pinned callee roots.
    pub fn new_retire(
        entry: usize,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        needs_context: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            entry_word: std::sync::atomic::AtomicU64::new(0),
            owner: parking_lot::Mutex::new(None),
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            // Unused for a retire cell: it is never matched by key (`targets`),
            // and the by-name clear in `JitCache::remove` ignores the loader.
            declaring_class_id: cratonvm_types::ClassId::new(0),
            needs_context,
            any_abi: false,
            pinned_entry: entry,
        })
    }

    /// The one entry a retire cell may hold, `0` for a cycle-edge cell.
    pub fn pinned_entry(&self) -> usize {
        self.pinned_entry
    }

    /// [`Self::pinned_entry`] of the cell at `cell_addr` -- what a
    /// `JIT_RETIRE_CELL_GUARD` row's `entry` holds -- for the backend, which
    /// sees only the address. `0` for a null address.
    ///
    /// # Safety
    ///
    /// `cell_addr` must be `0` or the address of a live `JitCycleEdgeCell`
    /// (`Arc::as_ptr`), kept alive for the duration of the call; the compile
    /// that bakes it owns it.
    pub unsafe fn pinned_entry_at(cell_addr: usize) -> usize {
        if cell_addr == 0 {
            return 0;
        }
        // SAFETY: the caller guarantees a live cell at this address.
        unsafe { (*(cell_addr as *const Self)).pinned_entry }
    }

    /// Whether this is a retire cell naming `(class, method, descriptor)`
    /// (any loader: the by-name clear is conservative).
    pub(crate) fn retires_with(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        self.pinned_entry != 0
            && *self.class_name == *class_name
            && *self.method_name == *method_name
            && *self.descriptor == *descriptor
    }

    /// The entry generated code would call now, `0` when unresolved.
    pub fn current_entry(&self) -> u64 {
        self.entry_word.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The [`compute_jit_key_hash`] of the target this cell waits for.
    pub(crate) fn target_hash(&self) -> u64 {
        compute_jit_key_hash(
            &self.class_name,
            &self.method_name,
            &self.descriptor,
            self.declaring_class_id,
        )
    }

    /// Whether `key` is this cell's target.
    pub(crate) fn targets(&self, key: &JitKey) -> bool {
        // A retire cell waits for nothing: it holds its pinned body or `0`.
        self.pinned_entry == 0
            && *key.class_name == *self.class_name
            && *key.method_name == *self.method_name
            && *key.descriptor == *self.descriptor
            && key.declaring_class_id == self.declaring_class_id
    }

    /// Whether `body` may be called through this cell (see "ABI" above).
    pub(crate) fn admits(&self, body: &CompiledMethod) -> bool {
        let entry = body.entry_ptr() as u64;
        entry != 0
            && entry & JIT_IC_NEEDS_CONTEXT_TAG == 0
            && (self.any_abi || body.needs_context() == self.needs_context)
            && !body.requires_wrapped_entry
            // The VM's direct-bind gate refuses these too (`indy-trap`).
            && !body.has_indy_trap
            && !body.retired.load(std::sync::atomic::Ordering::SeqCst)
            // A retire cell admits only the body its caller baked: anything
            // newer could close a cycle of cell edges with no stack guard.
            && (self.pinned_entry == 0 || entry as usize == self.pinned_entry)
    }

    /// Point the cell at `body` if [`Self::admits`] it. Returns whether it did.
    /// The replaced owner goes through the retirement queue.
    pub(crate) fn install(&self, body: &Arc<CompiledMethod>) -> bool {
        if !self.admits(body) {
            return false;
        }
        let word = if self.any_abi && body.needs_context() {
            (body.entry_ptr() as u64) | JIT_IC_NEEDS_CONTEXT_TAG
        } else {
            body.entry_ptr() as u64
        };
        let previous = {
            let mut owner = self.owner.lock();
            let previous = owner.replace(Arc::clone(body));
            self.entry_word
                .store(word, std::sync::atomic::Ordering::SeqCst);
            previous
        };
        defer_jit_owner(previous);
        true
    }

    /// Re-point a RETIRE cell from `expected_old` to `body` on a stale-call
    /// rebind (`JitCache::rebind_stale_call_site`). The caller has already
    /// verified `NotEntrantRecord::rebind_refusal` (no reference cycle, and a
    /// kept forward owns `body`); this checks the cell's own ABI invariants and
    /// that its current entry is still `expected_old`.
    pub(crate) fn rebind_retire(&self, body: &Arc<CompiledMethod>, expected_old: usize) -> bool {
        let entry = body.entry_ptr() as u64;
        if self.pinned_entry == 0
            || entry == 0
            || entry & JIT_IC_NEEDS_CONTEXT_TAG != 0
            || body.needs_context() != self.needs_context
            || body.requires_wrapped_entry
            || body.has_indy_trap
            || body.retired.load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        let previous = {
            let mut owner = self.owner.lock();
            if self.entry_word.load(std::sync::atomic::Ordering::SeqCst) as usize != expected_old {
                return false;
            }
            let previous = owner.replace(Arc::clone(body));
            self.entry_word
                .store(entry, std::sync::atomic::Ordering::SeqCst);
            previous
        };
        defer_jit_owner(previous);
        true
    }

    /// Back to unresolved: the site dispatches again. The owner goes through
    /// the retirement queue, because a thread may have loaded the word a moment
    /// ago and be about to `CALL` it.
    pub(crate) fn clear(&self) {
        let previous = {
            let mut owner = self.owner.lock();
            self.entry_word
                .store(0, std::sync::atomic::Ordering::SeqCst);
            owner.take()
        };
        defer_jit_owner(previous);
    }

    /// [`Self::clear`] when the current target is one of `targets`.
    pub(crate) fn invalidate_target(&self, targets: &std::collections::HashSet<usize>) {
        let entry = self.current_entry() & !JIT_IC_NEEDS_CONTEXT_TAG;
        if entry != 0 && targets.contains(&(entry as usize)) {
            self.clear();
        }
    }
}

/// Drop registry-upgraded cells without ever releasing a published body
/// outside the retirement queue: a cell whose owning artifact went away while
/// the caller held it is cleared (owner deferred) before its last `Arc` goes.
pub(crate) fn release_cycle_cells(cells: Vec<Arc<JitCycleEdgeCell>>) {
    for cell in cells {
        if Arc::strong_count(&cell) == 1 {
            cell.clear();
        }
        drop(cell);
    }
}

/// Inline-cache publications withdrawn again because an invalidation retired
/// the target between its admission and the publication. Diagnostic counter.
pub(crate) static IC_RETIRED_INSTALL_ROLLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Monomorphic inline cache (MIC) slot for invokevirtual/invokeinterface call sites.
///
/// Caches the receiver ClassId, class name, and resolved method entry pointer
/// so that repeated calls with the same receiver type skip vtable dispatch entirely.
///
/// On **cache hit** (receiver ClassId matches `cached_class_id`):
///   - If `cached_entry_word` is non-zero → direct call to the cached entry,
///     with the calling convention its bit 0 names
///   - Else → fast name-based dispatch (skip class_manager lookup)
///
/// On **cache miss**:
///   - Full vtable dispatch via class_manager + invoke_or_native
///   - Update all cached fields atomically
///
/// Hit/miss counters feed [`CompiledMethod::dominant_receiver_at_bci`] and the
/// dispatch diagnostics.
///
/// # JDK-ONLY-WAVE2 — CLOSED 2026-08-06: no gap, and the prescription was wrong
///
/// `cached_entry_word` holds a raw address that generated code `CALL R11`s on a
/// class-id guard hit. When the target is a native/builtin trampoline the slot
/// has kept the callback and thrown away its kind, so **nothing on the hit path
/// can re-check policy** — the defining shape
/// `docs/book/src/user-guide/real-jdk-mode.md` marks for wave 2.
///
/// Wave 1 closes it by refusal rather than by storing the kind: under
/// `JdkOnly`, `jit_entry_publishable` declines to publish an unowned (i.e.
/// native) entry at all, so the slot never holds one and the site keeps taking
/// the policy-checked dispatch helper. Storing the kind was rejected for this
/// wave because it is not free here — this struct is `#[repr(C)]` with two
/// offsets (`0`, `8`) baked as immediates into emitted machine code in
/// `jit/src/x64.rs`, `jit/src/ir_lower.rs` and `jit/src/runtime_lowering.rs`,
/// and the only population site ([`Self::update`]) is called from
/// `vm/src/jit/helpers.rs`, which this wave's owner cannot edit; widening the
/// `update` signature would break it.
///
/// **Do not do that.** The paragraph above used to prescribe appending a
/// `cached_native_kind: AtomicU8` at the tail, populating it at install time
/// and replacing the wave-1 refusal with a kind check. Checked 2026-08-06, it
/// fails on both halves:
///
/// * **Nothing would read it.** The hit path is emitted machine code, not
///   Rust: `ir_lower.rs` documents and emits
///   `MOV R11,[R10+ENTRY_PTR_OFFSETS[i]] ; CALL R11` after the class-id guard.
///   A kind byte in the slot has no reader on the one path it is meant to fix.
///   Consulting it would mean emitting the load, test and branch into the guard
///   sequence — a throughput cost on the JIT's hottest path, to police a state
///   the next point establishes cannot occur.
/// * **A native is never in the slot to begin with — in EITHER mode.** Both
///   population clusters in `vm/src/jit/helpers.rs` take their entry from
///   `try_jit_compile_callee`, which yields a JIT-compiled callee held by a
///   live `Arc<CompiledMethod>` pin. `jit_entry_publishable`'s
///   `owner.is_some()` early return therefore fires before any policy branch,
///   and that early return is **not policy-dependent**. So the strict refusal
///   below is unreachable (measured: `jit_inline_cache_natives` 0), and the
///   `Compatible` census gap this record also claimed is unreachable for
///   exactly the same reason — there is no native in the slot to dispatch
///   uncounted. The wave-2 record asserted both "a native trampoline never
///   reaches the refusal from these sites" and "the missing kind leaves the
///   census incomplete in `Compatible` mode as much as strict"; those cannot
///   both be true, and it is the second that is wrong.
///
/// If a future change ever routes a native trampoline into these slots, the
/// refusal below is what stops it, and THAT is the invariant to keep — not a
/// kind byte.
///
/// # Memory layout (CRIT-8 prerequisite)
///
/// `#[repr(C)]` plus a fixed field order with explicit padding pins the
/// JIT-hot fields at known offsets at the *start* of the struct so the
/// x86-64 codegen in `jit/src/x64.rs` can safely emit instructions like
/// `MOV eax, [mic_ptr + JitMICSlot::CACHED_CLASS_ID_OFFSET]` for inline
/// MIC dispatch. The `Mutex<Option<String>>` field — whose internal
/// representation is not guaranteed stable across `parking_lot`
/// versions — is moved to the **tail** so its layout cannot disturb
/// the hot-path offsets.
///
/// Offsets are asserted to match the constants in
/// [`JitMICSlot::CACHED_CLASS_ID_OFFSET`] et al. via a runtime test
/// (`test_jit_mic_slot_offsets`). A compile-time assert would require
/// `core::mem::offset_of!` (Rust 1.77+); the workspace MSRV is 1.75.
#[repr(C)]
pub struct JitMICSlot {
    /// Cached receiver ClassId. [`Self::EMPTY_CLASS_ID`] = empty/unpopulated
    /// (a fresh slot); `0` is ALSO read as empty for slots some other path
    /// zeroed, but is never what a fresh slot holds, because `ClassId(0)` is a
    /// real receiver (`java/lang/Object` and the untyped synthetic containers)
    /// and would match an empty slot's guard. [`Self::INSTALLING_CLASS_ID`]
    /// while a publication is in flight.
    /// **JIT-hot — offset 0.**
    pub cached_class_id: std::sync::atomic::AtomicU32,
    /// Redefinition epoch this slot was last validated against.
    ///
    /// Occupies the 4 bytes that used to be pure padding at offset 4, so the
    /// slot's size and every JIT-hot offset (`cached_class_id` 0,
    /// `cached_entry_word` 8) are unchanged — generated code never loads
    /// offset 4.
    ///
    /// Exists so a redefinition flushes each inline cache EXACTLY ONCE instead
    /// of forever. The previous gate asked "has this class ever been
    /// redefined?", which is permanently true, so a mocked class had its MIC
    /// and PIC erased on every single dispatch and re-resolved from scratch
    /// each call. That was the bulk of the ~30 µs/call cost measured in
    /// `docs/known-issues/repros/redefine-call-cost`.
    pub redefine_epoch: std::sync::atomic::AtomicU32,
    /// The cached target as ONE word: the entry address, with
    /// [`JIT_IC_NEEDS_CONTEXT_TAG`] set when the target takes the VM context
    /// pointer as its first argument. `0` = not resolved. Decode with
    /// [`Self::cached_entry`]. **JIT-hot — offset 8.**
    ///
    /// Generated code loads it once into R11, tests it for zero, and moves the
    /// tag into the carry flag to choose the ABI, so the address it calls and
    /// the convention it calls it with always come from the same publication.
    /// When they were two fields a reader could load one publication's address
    /// and the next one's flag.
    pub cached_entry_word: std::sync::atomic::AtomicU64,
    /// Set, and never cleared, once the dispatch helper publishes into this
    /// site's MIC or its co-allocated PIC a callee that declares an exception
    /// table. Read by the helper's own MIC-hit and megamorphic-PIC arms, which
    /// call the entry from Rust: such a callee's `i64::MIN` sentinel can be
    /// serviced by RE-RUNNING it at its handler with this call's arguments, so
    /// those arms root the reference arguments across the call
    /// (r11w4-rt-callee-rerun-reads-pre-call-argument-words). Per SITE, not per
    /// receiver class: a polymorphic site with one table callee pins for every
    /// receiver, which costs a pin and is never wrong. Generated code never
    /// reads it.
    ///
    /// Lives in the first byte of what used to be the reserved
    /// `cached_needs_context` bytes, so every offset is unchanged.
    pub(crate) callee_rerun_pins: std::sync::atomic::AtomicBool,
    /// The rest of the bytes that held `cached_needs_context` and its padding
    /// before the flag moved into the entry word. Reserved so the counters
    /// below keep their offsets; nothing reads them.
    pub(crate) _pad1: [u8; 7],
    /// Cache hit counter (diagnostic).
    pub hits: std::sync::atomic::AtomicU64,
    /// Cache miss counter (diagnostic).
    pub misses: std::sync::atomic::AtomicU64,
    /// Cached receiver class name (avoids class_manager read lock on hit).
    /// Moved to the tail: the mutex has an unstable layout we must not
    /// expose to JIT codegen. `Arc<str>` so the per-hit clone in the
    /// dispatch helper is a refcount bump, not a `String` heap copy.
    pub cached_class_name: parking_lot::Mutex<Option<std::sync::Arc<str>>>,
    /// Keeps a compiled cache target alive while generated code can load its
    /// raw entry pointer. Native targets have no owner and leave this empty.
    ///
    /// Its lock is also the WRITER lock for `cached_entry_word`: the word and
    /// the owner that keeps its body mapped are only ever changed together
    /// with it held, so no interleaving of a publication and a withdrawal can
    /// leave a callable word with no owner behind it.
    pub(crate) compiled_owner: parking_lot::Mutex<Option<Arc<CompiledMethod>>>,
    /// The bytecode index this slot's call site lives at, or `usize::MAX` for a
    /// slot with no site (tests, and the loop-unroll clones that share a site
    /// with their original).
    ///
    /// The compiler builds `mic_slots` as `(pc, *const JitMICSlot)` but the
    /// artifact keeps only `_jit_mic_slots: Vec<Box<JitMICSlot>>`, so the
    /// pc→slot mapping was DISCARDED at publication. That mapping is the only
    /// receiver evidence that exists for a method compiled by the
    /// eager-callee-chain before it ever ran its virtual calls interpreted —
    /// its `MethodProfile::receivers` map is empty at exactly those pcs (see
    /// `jit_inline_splice_devirt`). Recording it here costs one `usize` per
    /// slot and makes [`CompiledMethod::dominant_receiver_at_bci`] possible.
    ///
    /// A TAIL field on purpose: `cached_class_id` (0) and `cached_entry_word`
    /// (8) are read by generated code at fixed offsets, so nothing may be
    /// inserted before them.
    pub bci: usize,
    /// This slot's own `(holder body, index in its _jit_mic_slots)`, bound at
    /// publication (round 11 wave 19, lane lhm; see [`IcHolderRef`]). A TAIL
    /// field, for the same reason as `bci`. Generated code never reads it.
    pub(crate) holder: IcSlotHolder,
    /// nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`): set by the
    /// optimizing planner, before the slot is shared, on an own-site
    /// `invokevirtual` / `invokeinterface` in a loop whose receiver profile
    /// was STARVED at compile time (an eagerly compiled callee that never ran
    /// interpreted). The VM's miss helper then records each distinct receiver
    /// class it serves here into the method's receiver profile and asks for
    /// one optimizing recompile, which splices the site behind a class guard
    /// ([`IcProfileSeedSite`]). `None` for every other slot. A TAIL field, as
    /// `bci`; generated code never reads it.
    pub(crate) profile_seed: Option<Box<IcProfileSeedSite>>,
}

/// The most distinct receiver classes one [`IcProfileSeedSite`] records (the
/// polymorphic guarded splice's width). A fifth class closes the site.
pub const IC_PROFILE_SEED_MAX_CLASSES: usize = 4;

/// The most miss-helper entries one [`IcProfileSeedSite`] inspects. A site
/// whose callee the inline cascade cannot publish enters the helper on every
/// call; past this many entries the site is closed, so its cost is bounded.
pub const IC_PROFILE_SEED_MAX_ENTRIES: u32 = 64;

/// nio21 wave 16, lane L4 (`r21-eagerly-compiled-callee-has-no-receiver-profile-to-splice-CLOSED-20261012.md`,
/// P4 of `jit-r21-nio-proposals-w15-L1.md`): the receiver-profile identity of
/// an inline-cache site armed for profile seeding.
///
/// The optimizing tier splices an abstract-receiver site only from the
/// receiver profile (`ir::profile_guarded_receiver_class`), and the
/// interpreter is the only recorder. A kernel the eager callee chain compiled
/// before it ever ran has an EMPTY profile, so its `b.putInt(..)` stayed an
/// inline-cache call for the body's life (`Nio20BufferTyped directput`). The
/// inline cache is the only observer such a site has: its miss helper sees
/// every DISTINCT receiver class that reaches the site (a class the MIC or
/// PIC already serves never enters it). That set is what this records, as
/// the profile row a recompile reads.
///
/// Soundness does not depend on the counts: a guarded splice tests the exact
/// class and takes the ordinary call (or the trap) on a miss, whatever the
/// profile said. The counts only choose WHICH classes get a body.
pub struct IcProfileSeedSite {
    /// The compiling method's profile key: its declaring class id, name and
    /// descriptor (`CachedBytecodeMethod`), which is the key the compile door
    /// reads the profile under.
    pub class_id: u32,
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub descriptor: Arc<str>,
    /// The site's own bytecode index in that method.
    pub pc: usize,
    /// Distinct receiver classes recorded so far.
    pub(crate) seen: parking_lot::Mutex<Vec<u32>>,
    /// Miss-helper entries inspected so far.
    pub(crate) entries: std::sync::atomic::AtomicU32,
    /// Closed: nothing more is recorded.
    pub(crate) closed: std::sync::atomic::AtomicBool,
}

impl IcProfileSeedSite {
    pub fn new(
        class_id: u32,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        pc: usize,
    ) -> Self {
        Self {
            class_id,
            class_name,
            method_name,
            descriptor,
            pc,
            seen: parking_lot::Mutex::new(Vec::new()),
            entries: std::sync::atomic::AtomicU32::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the site still records (one relaxed load).
    #[inline]
    pub fn is_open(&self) -> bool {
        !self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// One miss-helper entry with a receiver of exactly `class_id`: `true`
    /// when the class is new to the site and should be recorded. Closes the
    /// site after [`IC_PROFILE_SEED_MAX_ENTRIES`] entries or at a class past
    /// [`IC_PROFILE_SEED_MAX_CLASSES`].
    pub fn note_receiver(&self, class_id: u32) -> bool {
        use std::sync::atomic::Ordering;
        if !self.is_open() {
            return false;
        }
        if self.entries.fetch_add(1, Ordering::Relaxed) + 1 >= IC_PROFILE_SEED_MAX_ENTRIES {
            self.closed.store(true, Ordering::Relaxed);
        }
        let mut seen = self.seen.lock();
        if seen.contains(&class_id) {
            return false;
        }
        if seen.len() >= IC_PROFILE_SEED_MAX_CLASSES {
            self.closed.store(true, Ordering::Relaxed);
            return false;
        }
        seen.push(class_id);
        true
    }

    /// The classes recorded so far, in arrival order (tests, DBG).
    pub fn recorded(&self) -> Vec<u32> {
        self.seen.lock().clone()
    }
}

/// `CRATONVM_JIT_IR_IC_PROFILE_SEED` (nio21 wave 16, lane L4; default ON,
/// `=0` is the kill switch): arm the inline-cache profile seed
/// ([`IcProfileSeedSite`]) on the optimizing tier's starved in-loop virtual
/// sites. Read once per compile that has a candidate site, and by the VM's
/// miss helper only for an armed slot.
pub fn ir_ic_profile_seed_enabled() -> bool {
    true
}

/// Should the optimizing planner arm the profile seed on its inline-cache site
/// at `pc` (nio21 wave 16, lane L4)? All of:
///
/// * a real virtual / interface site of the compiling method's own code
///   (`own_virtual_site`: not a bound-site cache, `pc < code_len`);
/// * a method-entry compile (`method_entry`): an OSR body is not what the
///   recompile replaces;
/// * inside a textual loop (`in_loop`): the site runs many times per entry, so
///   the one recompile it may cost is paid for; a straight-line site of a
///   method that ran once is not worth a compile;
/// * a starved receiver profile at the site: fewer than
///   [`MIN_GUARDED_RECEIVER_EVIDENCE`] observations (`observations`), the
///   evidence the guarded planner needs;
/// * the switch.
pub(crate) fn ir_ic_profile_seed_planned(
    own_virtual_site: bool,
    method_entry: bool,
    in_loop: bool,
    observations: u32,
) -> bool {
    own_virtual_site
        && method_entry
        && in_loop
        && observations < MIN_GUARDED_RECEIVER_EVIDENCE
        && ir_ic_profile_seed_enabled()
}



impl JitMICSlot {
    /// Temporary class-id value used while publishing a new cache entry.
    ///
    /// The generated code compares the receiver id before loading the target;
    /// keeping the guard non-matching until *all* companion fields are ready
    /// makes the publication atomic from its point of view. Class ids are
    /// allocated densely from zero and never use this all-ones reservation.
    pub(crate) const INSTALLING_CLASS_ID: u32 = u32::MAX;
    /// Class-id value of an EMPTY slot (round 9 wave 2).
    ///
    /// An empty slot used to hold `0`, and `ClassId(0)` is a real receiver
    /// class: a class-0 receiver whose guard `CMP` ran while the slot was
    /// still empty, and whose entry-word load ran after another thread's
    /// `update` had published class X's target, `CALL`ed X's method. A value
    /// no receiver can have closes that window: the guard never matches an
    /// empty slot. Distinct from [`Self::INSTALLING_CLASS_ID`] (`u32::MAX`)
    /// and from the PIC's `RETIRED_WAY_CLASS_ID` / `EMPTY_WAY_CLASS_ID`
    /// (`u32::MAX - 1`); all three sit above any allocatable class id.
    ///
    /// Readers that ask "is this slot empty?" must accept both this value and
    /// `0` (see [`Self::is_empty_class_id`]).
    pub const EMPTY_CLASS_ID: u32 = u32::MAX - 2;
    /// Byte offset of [`Self::cached_class_id`] from the start of the
    /// struct. JIT codegen uses this to emit
    /// `MOV eax, [mic_ptr + CACHED_CLASS_ID_OFFSET]`.
    pub const CACHED_CLASS_ID_OFFSET: usize = 0;
    /// Byte offset of [`Self::cached_entry_word`] from the start of the
    /// struct. JIT codegen loads the tagged target word from here on a cache
    /// hit.
    pub const CACHED_ENTRY_PTR_OFFSET: usize = 8;

    pub fn new() -> Self {
        Self {
            cached_class_id: std::sync::atomic::AtomicU32::new(Self::EMPTY_CLASS_ID),
            redefine_epoch: std::sync::atomic::AtomicU32::new(0),
            cached_entry_word: std::sync::atomic::AtomicU64::new(0),
            callee_rerun_pins: std::sync::atomic::AtomicBool::new(false),
            _pad1: [0; 7],
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            cached_class_name: parking_lot::Mutex::new(None),
            compiled_owner: parking_lot::Mutex::new(None),
            bci: usize::MAX,
            holder: std::sync::OnceLock::new(),
            profile_seed: None,
        }
    }

    /// Arm the inline-cache profile seed (nio21 wave 16, lane L4; see
    /// [`IcProfileSeedSite`]). Called by the planner on a slot it has not
    /// shared yet.
    pub fn arm_profile_seed(&mut self, site: IcProfileSeedSite) {
        self.profile_seed = Some(Box::new(site));
    }

    /// The armed profile seed while it still records; `None` for an unarmed
    /// slot (one null test) or a closed site.
    #[inline]
    pub fn open_profile_seed(&self) -> Option<&IcProfileSeedSite> {
        self.profile_seed.as_deref().filter(|s| s.is_open())
    }

    /// Record, on `owner`, that this slot now publishes it (round 11 wave 19,
    /// lane lhm). No-op for an unbound slot. If `owner` was already superseded
    /// when the record landed, a supersede walk may have run before it and
    /// missed this slot, so the publication is withdrawn here instead: the
    /// next call re-resolves to the current body. The record and the flag
    /// are ordered by `ic_holders`' lock against the walk, which marks the
    /// flag before it takes the list, so one side always sees the other.
    pub(crate) fn note_published_owner(&self, owner: &Arc<CompiledMethod>) {
        let Some((holder, index)) = self.holder.get() else {
            return;
        };
        owner.note_ic_holder(holder, *index, IcSlotKind::Mic);
        if owner.superseded.load(std::sync::atomic::Ordering::Acquire) {
            self.clear_compiled_entry_owned_by(owner);
        }
    }

    /// [`bind_inline_cache_holders`]' half for a slot that published before it
    /// was bound.
    pub(crate) fn register_current_owner(&self) {
        let owner = self.compiled_owner.lock().clone();
        if let Some(owner) = owner {
            self.note_published_owner(&owner);
        }
    }

    /// [`Self::clear_compiled_entry`], only while the slot still publishes
    /// `body`. Returns whether it did. The owner compare and the clear happen
    /// under the owner lock every writer takes, so a slot that has since been
    /// re-resolved to another body is left alone.
    pub(crate) fn clear_compiled_entry_owned_by(&self, body: &CompiledMethod) -> bool {
        let owner = {
            let mut slot_owner = self.compiled_owner.lock();
            if !slot_owner
                .as_ref()
                .is_some_and(|o| std::ptr::eq(Arc::as_ptr(o), body))
            {
                return false;
            }
            self.cached_entry_word
                .store(0, std::sync::atomic::Ordering::SeqCst);
            slot_owner.take()
        };
        defer_jit_owner(owner);
        true
    }

    /// [`Self::new`] for a slot that serves a known call site.
    pub fn new_at(bci: usize) -> Self {
        Self { bci, ..Self::new() }
    }

    /// Whether `class_id`, read from [`Self::cached_class_id`], means "no
    /// class cached": [`Self::EMPTY_CLASS_ID`] (what a fresh slot holds) or
    /// `0` (the pre-round-9 empty value, still accepted so a slot some other
    /// path zeroed keeps working).
    #[inline]
    pub const fn is_empty_class_id(class_id: u32) -> bool {
        class_id == 0 || class_id == Self::EMPTY_CLASS_ID
    }

    /// Seed the guard with a profile-derived receiver class (entry word left
    /// 0: a hint, not an installed target).
    ///
    /// A `class_id` of `0` is stored as [`Self::EMPTY_CLASS_ID`]: seeding the
    /// guard with `0` would re-open the empty-slot window for class-0
    /// receivers that the sentinel exists to close, and `update` treats both
    /// values alike, so the first dispatch still installs the target.
    pub fn prepopulate(&self, class_id: u32) {
        let seeded = if class_id == 0 {
            Self::EMPTY_CLASS_ID
        } else {
            class_id
        };
        self.cached_class_id
            .store(seeded, std::sync::atomic::Ordering::Relaxed);
    }

    /// The published target and whether it takes the VM context pointer,
    /// decoded from the single entry word. `(0, false)` means no target.
    #[inline]
    pub fn cached_entry(&self) -> (u64, bool) {
        jit_ic_entry_decode(
            self.cached_entry_word
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Whether a callee published at this site declares an exception table
    /// (see the `callee_rerun_pins` field). Pairs with the `Acquire`
    /// load of the entry word or class id that found the target: the flag is
    /// stored before [`Self::update`] publishes either.
    #[inline]
    pub fn callee_rerun_pins(&self) -> bool {
        self.callee_rerun_pins
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record that a callee about to be published at this site (MIC or PIC)
    /// declares an exception table. Call it BEFORE the `update` / `install`
    /// that publishes the callee, so a reader that finds the entry also finds
    /// the flag. Sticky.
    #[inline]
    pub fn mark_callee_rerun_pins(&self) {
        self.callee_rerun_pins
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Update all cached fields after a cache miss.
    pub fn update(
        &self,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_context: bool,
        jdk_only: bool,
    ) {
        use std::sync::atomic::Ordering;

        // A raw inline MIC has no helper boundary between its guard load and
        // indirect CALL. Retargeting a populated slot can therefore pair one
        // receiver's class guard with another receiver's entry. Make the slot
        // monomorphic for its lifetime: reserve an empty slot with CAS, publish
        // its companion fields, then publish the class id last. A different
        // receiver simply takes the ordinary helper path; this is slower only
        // for polymorphic sites and cannot redirect native control flow.
        //
        // `prepopulate` seeds `cached_class_id` from profiling data with
        // the entry word still 0 (a guard hint, not an installed target).
        // Treat that shape as an empty slot for THIS class id too — otherwise
        // the very first `update` for the profiled-dominant receiver would
        // match `id == class_id` and return before ever publishing an entry,
        // permanently stranding the slot at "unresolved" for its whole
        // lifetime. The same shape is what `clear_compiled_entry` leaves, so a
        // withdrawn target can be re-resolved for the same receiver.
        //
        // The `INSTALLING_CLASS_ID` arm waits for another writer's few stores
        // (see `mic_install_wait`). `waits` counts its checks; `bounded` is the
        // kill switch, read once, only if the wait outlasts the pure spin.
        let mut waits: u32 = 0;
        let mut bounded: Option<bool> = None;
        loop {
            let current = self.cached_class_id.load(Ordering::Acquire);
            if current == class_id {
                if self.cached_entry_word.load(Ordering::Acquire) != 0 {
                    // Already fully installed for this class: idempotent no-op.
                    return;
                }
                if self
                    .cached_class_id
                    .compare_exchange(
                        class_id,
                        Self::INSTALLING_CLASS_ID,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
                continue;
            }
            match current {
                // A `0` that carries an installed entry is NOT empty: it is a
                // slot monomorphic on class 0 (`java/lang/Object`). Retargeting
                // it would reopen the window the sentinel closes -- a class-0
                // reader that matched before the CAS pairing with the new
                // class's entry.
                0 if self.cached_entry_word.load(Ordering::Acquire) != 0 => return,
                // Empty: a fresh slot (`EMPTY_CLASS_ID`) or one some other path
                // zeroed. CAS from the value actually observed.
                0 | Self::EMPTY_CLASS_ID => {
                    if self
                        .cached_class_id
                        .compare_exchange(
                            current,
                            Self::INSTALLING_CLASS_ID,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
                Self::INSTALLING_CLASS_ID => {
                    waits = waits.saturating_add(1);
                    if mic_install_wait(self, waits, &mut bounded) {
                        return;
                    }
                }
                _ => return,
            }
        }
        // `compiled_owner` is what keeps the callee's code mapped while this
        // slot's raw entry is callable, so an entry we cannot retain — or one
        // an invalidation has already retired — is downgraded to "class
        // cached, target unresolved" (the same shape `prepopulate` leaves)
        // rather than published. See `jit_entry_publishable`.
        let owner = resolve_jit_entry_owner(entry_ptr as usize);
        let word =
            if jit_entry_publishable(entry_ptr, &owner, jdk_only) && !owner_is_retired(&owner) {
                jit_ic_entry_word(entry_ptr, needs_context)
            } else {
                0
            };
        // Publish the word BEFORE the class id so a reader that has not yet
        // matched the guard cannot pair it with stale target metadata. The
        // word is one store, so a reader already past a prepopulated guard
        // loads either 0 or the whole new target — never an address from one
        // publication with the ABI of another.
        // Kept for the supersede reverse index (round 11 wave 19, lane lhm):
        // recorded on the callee after the publication below. Only a bound
        // slot records anything, so an unbound one does not pay the clone.
        let published_owner = if word != 0 && self.holder.get().is_some() {
            owner.clone()
        } else {
            None
        };
        let previous = {
            let mut slot_owner = self.compiled_owner.lock();
            let previous =
                std::mem::replace(&mut *slot_owner, if word != 0 { owner } else { None });
            self.cached_entry_word.store(word, Ordering::SeqCst);
            previous
        };
        *self.cached_class_name.lock() = Some(std::sync::Arc::from(class_name));
        self.cached_class_id.store(class_id, Ordering::Release);
        // Never drop a replaced owner by assignment: it may be the last thing
        // mapping a body some raw reader loaded a moment ago.
        defer_jit_owner(previous);
        // An invalidation marks its bodies `retired` under the cache's mutation
        // lock and only THEN clears inline caches. If it marked this body after
        // the admission above but visited this slot before the store, its pass
        // missed the publication. Re-checking after the store closes that
        // window: one of the two always observes the other.
        if word != 0 && self.compiled_owner_retired() {
            IC_RETIRED_INSTALL_ROLLBACKS.fetch_add(1, Ordering::Relaxed);
            self.clear_compiled_entry();
        }
        if let Some(owner) = published_owner {
            self.note_published_owner(&owner);
        }
    }

    /// Whether the owner this slot currently retains has been retired.
    pub(crate) fn compiled_owner_retired(&self) -> bool {
        owner_is_retired(&self.compiled_owner.lock())
    }

    /// Drop only the compiled-entry half of the MIC.
    ///
    /// The receiver class/name cache remains useful for helper-side dispatch,
    /// but generated inline code treats a zero entry word as unresolved. The
    /// word and its owner change together under the owner lock, and the owner
    /// is released through the retirement queue: a raw reader that loaded the
    /// word just before the store may still be about to call it.
    pub fn clear_compiled_entry(&self) {
        let owner = {
            let mut slot_owner = self.compiled_owner.lock();
            self.cached_entry_word
                .store(0, std::sync::atomic::Ordering::SeqCst);
            slot_owner.take()
        };
        defer_jit_owner(owner);
    }

    pub(crate) fn invalidate_target(&self, targets: &std::collections::HashSet<usize>) {
        let (entry, _) = self.cached_entry();
        if entry != 0 && targets.contains(&(entry as usize)) {
            self.clear_compiled_entry();
        }
    }

    pub(crate) fn invalidate_target_single(&self, target: usize) {
        let (entry, _) = self.cached_entry();
        if entry != 0 && (entry as usize) == target {
            self.clear_compiled_entry();
        }
    }

    /// [`Self::clear_compiled_entry`] when the compiled body this slot retains
    /// satisfies `stale` (interpreter round i1 wave 21, lane L1): matched by
    /// the owner, so a body no longer published (a tier-up superseded it)
    /// is found too. A publication racing the check at worst loses a fresh
    /// entry, which the next miss refills.
    pub(crate) fn invalidate_owned_target(&self, stale: &dyn Fn(&Arc<CompiledMethod>) -> bool) {
        let doomed = self.compiled_owner.lock().as_ref().is_some_and(stale);
        if doomed {
            self.clear_compiled_entry();
        }
    }

    /// Record a cache hit.
    #[inline]
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a cache miss.
    #[inline]
    pub fn record_miss(&self) {
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `hits` value past which [`Self::note_dispatch_hit`] stops counting.
    /// Enough for `mic_slot_census` to tell "warmed up" from "too few
    /// samples", which is all anything reads `hits` for.
    pub(crate) const DISPATCH_HIT_CAP: u64 = 4096;

    /// The dispatch helper's per-call hit record: [`Self::record_hit`] until
    /// `hits` reaches [`Self::DISPATCH_HIT_CAP`], then a plain load.
    ///
    /// `hits`/`misses` share the cache line generated code reads
    /// (`cached_class_id` at 0, `cached_entry_word` at 8) on EVERY call
    /// through the slot. A `lock xadd` per helper entry — the "hit, no
    /// compiled entry" arm runs per call for a callee that never compiles —
    /// took that line exclusive and made every other thread's inline guard
    /// miss on it (r11-rt-mic-hit-miss-counters-share-the-inline-cache-line).
    /// Once saturated the line stays shared.
    #[inline]
    pub fn note_dispatch_hit(&self) {
        use std::sync::atomic::Ordering;
        if self.hits.load(Ordering::Relaxed) < Self::DISPATCH_HIT_CAP {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The dispatch helper's per-call miss record: [`Self::record_miss`]
    /// until `misses` passes [`MIC_TO_PIC_THRESHOLD`] — the only threshold
    /// anything reads (`CompiledMethod::dominant_receiver_at_bci`) — then a
    /// plain load. See [`Self::note_dispatch_hit`] for why.
    #[inline]
    pub fn note_dispatch_miss(&self) {
        use std::sync::atomic::Ordering;
        if self.misses.load(Ordering::Relaxed) <= MIC_TO_PIC_THRESHOLD {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Total observations (hits + misses).
    pub fn total_observations(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
            + self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Hit rate as a percentage (0-100). Returns 0 if no observations.
    pub fn hit_rate_pct(&self) -> u32 {
        let total = self.total_observations();
        if total == 0 {
            return 0;
        }
        let h = self.hits.load(std::sync::atomic::Ordering::Relaxed);
        ((h * 100) / total) as u32
    }

    /// Returns true if the cache is monomorphic (only one receiver type observed
    /// and hit rate ≥ 90%).
    pub fn is_monomorphic(&self) -> bool {
        self.total_observations() >= 10 && self.hit_rate_pct() >= 90
    }
}

impl Default for JitMICSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure `PAUSE` spins before [`JitMICSlot::update`]'s wait for another
/// writer's `INSTALLING_CLASS_ID` publication starts yielding the CPU (round 13
/// wave 10, lane spin; lane mega8's proposal M8-2).
pub(crate) const MIC_INSTALL_WAIT_SPINS: u32 = 64;
/// Checks after which that wait gives up without publishing.
pub(crate) const MIC_INSTALL_WAIT_LIMIT: u32 = 4096;

/// One check of [`JitMICSlot::update`]'s wait for another writer's
/// `INSTALLING_CLASS_ID` publication. Answers whether `update` should give up
/// and return WITHOUT publishing.
///
/// The holder is `update` itself (between its CAS and its class-id store) or
/// `forget_dead_receiver_deferring`; both run a few stores under leaf mutexes
/// with no safepoint poll, so the wait is bounded by them. But a holder that
/// is descheduled on a loaded host costs a pure spinner its whole time slice
/// (and several spinning mutators can keep it off a core), and a future holder
/// that could stop between the CAS and the store would turn every miss at this
/// site into a silent, pollless hang (see
/// `r13w10-orch-codecache-discard-churn-compatible-spin-hang-FIXED-20260928.md`).
/// So after [`MIC_INSTALL_WAIT_SPINS`]
/// spins the wait yields, and at [`MIC_INSTALL_WAIT_LIMIT`] checks it gives
/// up: a MIC fill is a cache, every caller has already resolved the target it
/// is about to call, and the next miss at the site simply tries again.
///
/// `CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND=0` restores the unbounded pure spin;
/// `bounded` caches that read for the one `update` call.
/// `CRATONVM_DBG_JIT_SPIN_WARN=1` makes a wait that reaches the limit print
/// one `[cratonvm] WARNING` naming the slot (once per `update` call when
/// bounded; at every power of two past the limit when not).
pub(crate) fn mic_install_wait(slot: &JitMICSlot, waits: u32, bounded: &mut Option<bool>) -> bool {
    if waits <= MIC_INSTALL_WAIT_SPINS {
        std::hint::spin_loop();
        return false;
    }
    let on = *bounded.get_or_insert_with(|| {
        true
    });
    if !on {
        if waits >= MIC_INSTALL_WAIT_LIMIT && waits.is_power_of_two() {
            note_long_mic_install_wait(slot, waits, false);
        }
        std::hint::spin_loop();
        return false;
    }
    if waits >= MIC_INSTALL_WAIT_LIMIT {
        note_long_mic_install_wait(slot, waits, true);
        return true;
    }
    std::thread::yield_now();
    false
}

/// The `CRATONVM_DBG_JIT_SPIN_WARN` line for [`mic_install_wait`].
#[cold]
#[inline(never)]
pub(crate) fn note_long_mic_install_wait(slot: &JitMICSlot, waits: u32, giving_up: bool) {
    if !false {
        return;
    }
    eprintln!(
        "[cratonvm] WARNING: JitMICSlot::update (jit/src/lib.rs) waited {waits} checks for another \
         thread's INSTALLING_CLASS_ID publication on the MIC slot at {:p} (bci {}): {}",
        slot as *const JitMICSlot,
        slot.bci,
        if giving_up {
            "giving up without publishing (CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND)"
        } else {
            "still spinning, the wait is unbounded (CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND=0)"
        },
    );
}

// ---------------------------------------------------------------------------
// JIT cache
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct JitKey {
    pub(crate) class_name: Arc<str>,
    pub(crate) method_name: Arc<str>,
    pub(crate) descriptor: Arc<str>,
    // Loader-identity fix (2026-07-22): two classes with the SAME binary
    // name loaded by DIFFERENT `ClassLoader`s (e.g. a custom
    // `ClassLoader(null)` re-loading an old H2 jar's own
    // `org.h2.mvstore.RootReference` alongside the identically-named class
    // already on the application classpath — see
    // `bug-h2-suite-residual-fail-triage-FIXED.md`'s
    // `TestUpgrade` residual) are DISTINCT classes with unrelated bytecode,
    // but the interpreter's own dispatch (`resolve_method_ref` /
    // `execute_invoke_kind` / `try_stackless_invoke`) already correctly
    // disambiguates them via `ClassId` (loader-aware) at every call site.
    // This cache was the ONE place that didn't: keyed purely by
    // (class_name, method_name, descriptor) STRINGS, so once EITHER
    // same-named class's method got JIT-compiled first, every subsequent
    // call to the OTHER class's identically-named-and-shaped method hit
    // this cache and silently executed the WRONG class's compiled body —
    // whichever compiled first permanently "won" the name, `put()`
    // additionally evicting the loser's entry outright. Including the
    // resolved declaring `ClassId` in the key gives each class's compiled
    // method its own cache slot, exactly mirroring the interpreter's own
    // per-`ClassId` dispatch and letting both coexist.
    pub(crate) declaring_class_id: cratonvm_types::ClassId,
}

/// JIT round 21 wave 3 (lane splice): [`JitCache::put`] drops a body without a
/// word on six early returns, and the publishing door can only report that the
/// read-back answered another body (`c2-publication REFUSED ... the
/// single-pass body stays live`, the R21CfChain `CompletableFuture.complete`
/// line). Under `CRATONVM_DBG_JITC` each drop now names its reason:
/// `jit-cache-put DROPPED <key> (ir|sp): <reason>`. Diagnostic only; nothing
/// is read on a quiet run but the cached debug flag. Wave 4 (lane codecache):
/// [`JitCache::put_osr`]'s five drops too, their reasons prefixed `osr: `.
pub(crate) fn jit_cache_put_dropped(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    compiled: &CompiledMethod,
    reason: &str,
) {
    if x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] jit-cache-put DROPPED {class_name}.{method_name}{descriptor} ({}): {reason}",
            if compiled.used_ir_backend { "ir" } else { "sp" },
        );
    }
    // JIT round 21 (lane accept, `79fbc8a88c`; re-landed t1 w8 ACCEPT): the
    // same drop beside the `put` / `osr` lines of `CRATONVM_DBG_JIT_COMPILED`,
    // which now print only after the last drop check, so "produced a body"
    // with no `put` line names its reason under that flag too.
    if false {
        eprintln!(
            "CRATONVM_DBG_JIT_COMPILED: dropped {class_name}.{method_name}{descriptor} ({reason}; {} body)",
            if compiled.used_ir_backend { "optimizing" } else { "single-pass" },
        );
    }
}

/// Compute a u64 hash key for a JIT cache entry from (class, method,
/// descriptor, declaring class id), written in that order into ONE hasher with
/// a separator after each string.
///
/// This used to hash each component with its own hasher and XOR-fold the four
/// results. The fold is symmetric, so equal components cancelled
/// (`class Foo { void Foo() }` and `class Bar { void Bar() }` hashed to the same
/// value, since `h("Foo") ^ h("Foo")` is zero) and swapped components collided.
/// `JitCache` survived that by verifying the full key on every hit, but the
/// negative memos keyed by this hash verified nothing, so a collision there
/// refused an innocent method. `0xFF` never occurs in modified UTF-8, so the
/// separators also make every component boundary unambiguous (`"ab" + "c"` is
/// not `"a" + "bc"`). No per-call allocation either way.
///
/// Hash collisions are still tolerated by the cache: `JitCache::get` always
/// verifies the full string key match after the hash hit (see PERF-P2
/// fix). A collision degrades to a cache miss, which is correct but
/// slightly suboptimal (triggers a re-compile via the slow path).
#[inline]
pub(crate) fn compute_jit_key_hash(
    class: &str,
    method: &str,
    desc: &str,
    declaring_class_id: cratonvm_types::ClassId,
) -> u64 {
    let mut h = FxHasher::default();
    h.write(class.as_bytes());
    h.write_u8(0xFF);
    h.write(method.as_bytes());
    h.write_u8(0xFF);
    h.write(desc.as_bytes());
    h.write_u8(0xFF);
    h.write_u32(declaring_class_id.as_u32());
    h.finish()
}

/// Per-VM JIT cache: maps method identity to compiled native code.
///
/// Storage uses a precomputed u64 hash as the map key (see
/// [`compute_jit_key_hash`]). The full [`JitKey`] is stored alongside
/// the compiled method so lookups can verify the key fully matches
/// after a hash hit — this protects against (rare) hash collisions
/// while eliminating the three `Arc<str>` clones the prior keyed-map
/// implementation required on every lookup (PERF-P2).
///
/// Lookups are lock-free: the methods map is split into `JIT_CACHE_SHARDS`
/// `ArcSwap` snapshots, and a publication copies only its own shard (see
/// `docs/architecture/jit-cache-sharding-and-code-reclamation.md`). The
/// per-cache `string_arena`/`invoke_info_arena` that once blocked this, and
/// their `intern_string`/`intern_invoke_info` accessors, had no callers and
/// were deleted: every by-pointer string or invoke record a body bakes is owned
/// by that `CompiledMethod` (`_jit_strings`, `_jit_invoke_infos`), or leaked
/// deliberately by an intern table whose key is the pointer.
///
/// # The code arena (round-8 "jit #6") — BUILT 2026-09-16, default OFF
///
/// This carried a `TODO` saying there was "no pooling / LRU / coalescing of
/// `ExecutableBuffer`s", with the cost measured: on Windows each compiled
/// method took its own `VirtualAlloc` at 64 KiB allocation granularity for a
/// typical ~5 KB body, so a JDK-shape workload with ~3,000 hot methods
/// reserved about **177 MB of address space it never used**. Four of the five
/// numbered steps of that plan are now in `platform.rs`; the numbering below
/// is the original plan's, so the two can be read against each other.
///
///  1. **Built**, as a process-wide `JitCodeArena` rather than a `JitCache`
///     field. An arena's whole purpose is that one reservation serves every
///     compile in the process, so per-VM ownership would give up the saving it
///     exists for. Regions are 16 MiB (`JIT_CODE_DEFAULT_REGION_BYTES`).
///  2. **Built**: `ExecutableBuffer::new_in(arena, size)`, with a free list.
///     The plan's "256-byte-rounded size buckets" became **exact page-count
///     classes**, because step 4 rounds every block to a page and that
///     collapses every 256-byte class below 4 KiB into one. **Splitting
///     landed on 2026-09-16** — a request is served from the smallest class
///     at or above it and the remainder goes back as its own entry, both
///     pieces page-aligned and page-sized by induction. **Coalescing is
///     refused, in writing**: blocks only ever get smaller and adjacent free
///     pages never remerge. Two things bound that without fixing it —
///     splitting adds one entry per allocation and only on an exact-class
///     miss, and region reclamation is a coarse coalescer, since a fully-free
///     region's shattered list is discarded with the region. The measurement
///     that would settle building real coalescing is named on
///     `JitCodeArena`: `bytes_free_listed / free_blocks` decaying towards one
///     page while `bytes_free_listed` stays large. Free-on-drop routes by
///     address dispatch in `free_executable`, not a back-pointer, because
///     `ExecutableBuffer` and its `Drop` live here and `platform.rs` cannot
///     see them.
///  3. **Refused, and the plan was wrong.** Registering whole *arenas* in
///     `jit_code_regions` would make `validate_code_ptr` accept a pointer into
///     a freed block, a free-listed block, or never-bumped tail — i.e. it
///     would trade a real validator for a faster one that answers yes to the
///     thing it exists to catch. `new_in` registers the individual block, as
///     `new` does.
///  4. **Built, taking the first option and refusing the "preferred" one.**
///     Every block is rounded up to a page so protections toggle
///     independently; batching protect-RX until quiesce changes *when* code
///     becomes executable, which is an ordering property and not an
///     optimization. Page size is queried rather than assumed, with a 64 KiB
///     fallback — over-estimating is safe, under-estimating breaks the W^X
///     argument. Page rounding also preserves `implicit_null`'s per-4-KiB-page
///     retirement buckets, which are written against "the granularity
///     `alloc_executable` hands out code in".
///  5. **Not built.** OSR trampolines still take `ExecutableBuffer::new`.
///     `JitCodeArena::with_region_bytes` is the knob a small-bucket arena for
///     them would use.
///
/// **Region reclamation landed 2026-09-16.** A region with no live block and
/// its whole span on the free list is unmapped, hysteretically — reclaim only
/// when MORE than one region is empty, then down to exactly one, so an arena
/// oscillating around a single empty region never unmaps. The order is the
/// safety argument: take the region out of its slot, delete every free-list
/// entry naming it, then withdraw its span **and** `platform_free` while still
/// holding the spans lock, because a span that outlived its mapping would
/// claim a later unrelated mapping the OS put at the same address.
///
/// Reclamation *strengthens* the address-reuse obligation below rather than
/// adding to it: a region is reclaimed only from `release`, downstream of every
/// purge, and a block whose registries were not purged is still live from the
/// arena's view so its region is not reclaimable. What changes is the failure
/// mode if the obligation is ever violated — re-handing gives silent wrong
/// control flow, unmapping gives a fault on unmapped memory.
///
/// **LRU eviction is still not built, and "LRU" is not implementable from this
/// tree.** There is an eviction SEAM — `JitCodeArena::pressure()` and
/// `set_eviction_hook`, unset by default — but no policy, and the reason is
/// specific rather than a shrug: nothing in the VM records *recency*.
/// `tiered::MethodState` carries `invocation_count` and `backedge_count` but
/// no last-use time, `CompiledMethod` carries none either, and
/// `last_compile_time_ms` is a duration. A least-INVOKED policy is
/// implementable and is a much worse one. The seam also runs its hook with the
/// arena mutex held, so a hook may only select and queue — dropping a
/// `CompiledMethod` from inside it deadlocks through
/// `ExecutableBuffer::drop` -> `free_executable` -> `free_if_arena`. See
/// `platform.rs`'s REVIEW-NOTE 8.
///
/// **Default OFF** (`CRATONVM_JIT_CODE_ARENA`), and inert by construction on
/// macOS/AArch64, where there is no per-page W^X flip and no 64 KiB
/// granularity tax to recover. Off, `new_in` falls straight through to `new`,
/// so a default configuration allocates exactly as it did before. The saving
/// above is the *measured* cost of not having an arena; the saving *from* this
/// arena is unmeasured, which is why it is not on by default — see the
/// REVIEW-NOTEs in `platform.rs` for the evidence a flip would need.
///
/// **The obligation a caller inherits.** A block may be returned to the free
/// list only after every registry keyed on its address range has been purged:
/// `implicit_null::unregister_range`, the JIT method-name table,
/// `jit_code_regions`, and `code_events`. Otherwise a later allocation gets
/// the same address and a stale entry resumes execution inside the wrong
/// method — hazard 2 of `implicit_null`'s module doc, arriving through a door
/// that did not exist when that doc was written. It is discharged today by
/// inheritance: the only path into `JitCodeArena::release` is
/// `free_executable` ← `ExecutableBuffer::drop` ← `CompiledMethod::drop`,
/// which purges all four first.
impl CompiledMethod {
    pub(crate) fn invalidate_cached_targets(&self, targets: &std::collections::HashSet<usize>) {
        if targets.len() == 1 {
            if let Some(&single) = targets.iter().next() {
                self.invalidate_cached_target_single(single);
                return;
            }
        }
        for slot in &self._jit_mic_slots {
            slot.invalidate_target(targets);
        }
        for slot in &self._jit_pic_slots {
            slot.invalidate_targets(targets);
        }
    }

    pub(crate) fn invalidate_cached_target_single(&self, target: usize) {
        for slot in &self._jit_mic_slots {
            slot.invalidate_target_single(target);
        }
        for slot in &self._jit_pic_slots {
            slot.invalidate_target_single(target);
        }
    }

    /// Send every inline-cache slot of this body whose retained compiled
    /// target satisfies `stale` back to its helper (interpreter round i1 wave
    /// 21, lane L1; see [`JitCache::invalidate_inline_caches_reaching_redefined`]).
    pub(crate) fn invalidate_cached_owners(&self, stale: &dyn Fn(&Arc<CompiledMethod>) -> bool) {
        for slot in &self._jit_mic_slots {
            slot.invalidate_owned_target(stale);
        }
        for slot in &self._jit_pic_slots {
            slot.invalidate_owned_targets(stale);
        }
    }

    pub(crate) fn clear_cached_targets(&self) {
        for slot in &self._jit_mic_slots {
            slot.clear_compiled_entry();
        }
        for slot in &self._jit_pic_slots {
            slot.clear_entries();
        }
    }
}

/// Shard map. Bodies are held as [`RetainedCode`], not bare `Arc`s (round 9
/// wave 2): a concurrent reader's `arc_swap` guard can keep a superseded map —
/// and so a withdrawn body — alive after the publisher has retired its own
/// reference, making the reader's guard drop the body's LAST reference.
/// `RetainedCode::drop` routes a last drop through `defer_jit_owner`, so that
/// release goes through the retirement queue by construction instead of
/// counting in `published_code_free_audit().1`. See
/// `jit-cache-reader-guard-makes-the-last-drop-unauthorised-20260918.md`.
pub(crate) type JitCacheMap = FxHashMap<u64, (JitKey, RetainedCode)>;
pub(crate) const JIT_CACHE_SHARDS: usize = 64;

pub(crate) struct JitCacheShard {
    pub(crate) methods: arc_swap::ArcSwap<JitCacheMap>,
    pub(crate) osr_methods: arc_swap::ArcSwap<JitCacheMap>,
}

impl JitCacheShard {
    pub(crate) fn new() -> Self {
        Self {
            methods: arc_swap::ArcSwap::from_pointee(FxHashMap::default()),
            osr_methods: arc_swap::ArcSwap::from_pointee(FxHashMap::default()),
        }
    }
}

/// The dependency records of one [`JitCache`], counted as HotSpot counts its
/// `DependencyContext` buckets (`sun.ci.nmethodBuckets*`) and its
/// `find_witness_in` calls (`sun.ci.findWitnessIn`).
///
/// A compiled body here depends on every class it inlined from
/// (`CompiledMethod::inlined_methods`): a define that adds a subtype to such a
/// class, or retires it, withdraws the body. One record per (class, body)
/// pair is one of HotSpot's buckets, the entry that links an nmethod to a
/// klass's dependency context:
/// - allocated when the body publishes;
/// - stale once the body is withdrawn (`CompiledMethod::retired`) but still
///   owned by someone (a frame, the retirement queue), as a bucket is stale
///   while its nmethod is unloading but not yet purged;
/// - deallocated when the last owner drops it, found by the purge
///   ([`Self::purge`]), which runs at each read and when the book doubles.
///
/// A body displaced by a tier-up is not stale: it stays callable through its
/// baked callers, as a not-entrant nmethod stays in its contexts. Each
/// deallocated record went stale first (`stale_accumulated` counts it at the
/// purge if no read saw it stale), so `deallocated <= stale_accumulated`, as in
/// HotSpot.
#[derive(Default)]
pub(crate) struct DependencyBucketBook {
    pub(crate) state: parking_lot::Mutex<DependencyBucketState>,
    /// `sun.ci.findWitnessIn`: dependencies checked against a class change
    /// (one per `inlined_methods` entry naming the changed class, per body
    /// examined), HotSpot's `find_witness_in` calls from
    /// `check_klass_dependency`.
    pub(crate) witness_in: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
pub(crate) struct DependencyBucketState {
    /// Live records: the body, its record count, and whether a read already
    /// counted them stale.
    pub(crate) live: Vec<(std::sync::Weak<CompiledMethod>, u32, bool)>,
    /// `live.len()` after the last purge; the next purge at publication runs
    /// once the book has doubled past it.
    pub(crate) purged_len: usize,
    pub(crate) allocated: u64,
    pub(crate) deallocated: u64,
    pub(crate) stale: u64,
    pub(crate) stale_accumulated: u64,
}

impl DependencyBucketBook {
    /// The distinct classes `compiled` depends on: its records.
    pub(crate) fn records_of(compiled: &CompiledMethod) -> u32 {
        let mut classes: Vec<&str> = compiled
            .inlined_methods
            .iter()
            .map(|(class, _, _)| class.as_str())
            .collect();
        classes.sort_unstable();
        classes.dedup();
        u32::try_from(classes.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn note_published(&self, compiled: &Arc<CompiledMethod>) {
        let records = Self::records_of(compiled);
        if records == 0 {
            return;
        }
        let mut state = self.state.lock();
        state.allocated += u64::from(records);
        state.live.push((Arc::downgrade(compiled), records, false));
        if state.live.len() > 2 * state.purged_len.max(512) {
            Self::purge(&mut state);
        }
    }

    /// Count the dependencies of `compiled` on `class` that a change of
    /// `class` checks.
    pub(crate) fn note_checked(&self, compiled: &CompiledMethod, class: &str) {
        let checked = compiled
            .inlined_methods
            .iter()
            .filter(|(c, _, _)| c == class)
            .count();
        if checked != 0 {
            self.witness_in.fetch_add(
                checked as u64, // Widening: usize -> u64
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// Drop the records of bodies nobody owns any more (deallocated), and
    /// recount the stale ones.
    pub(crate) fn purge(state: &mut DependencyBucketState) {
        let (mut stale, mut deallocated, mut newly_stale) = (0u64, 0u64, 0u64);
        state.live.retain_mut(|(body, records, counted_stale)| {
            let records = u64::from(*records);
            match body.upgrade() {
                None => {
                    deallocated += records;
                    if !*counted_stale {
                        newly_stale += records;
                    }
                    false
                }
                Some(cm) => {
                    if cm.retired.load(std::sync::atomic::Ordering::Acquire) {
                        stale += records;
                        if !*counted_stale {
                            *counted_stale = true;
                            newly_stale += records;
                        }
                    }
                    true
                }
            }
        });
        state.deallocated += deallocated;
        state.stale_accumulated += newly_stale;
        state.stale = stale;
        state.purged_len = state.live.len();
    }

    pub(crate) fn values(&self) -> DependencyCounterValues {
        let mut state = self.state.lock();
        Self::purge(&mut state);
        DependencyCounterValues {
            find_witness_in: self.witness_in.load(std::sync::atomic::Ordering::Relaxed),
            buckets_allocated: state.allocated,
            buckets_deallocated: state.deallocated,
            buckets_stale: state.stale,
            buckets_stale_accumulated: state.stale_accumulated,
        }
    }
}

/// [`JitCache::dependency_counter_values`]: the perf counters
/// `sun.ci.findWitnessIn` and `sun.ci.nmethodBuckets{Allocated, Deallocated,
/// Stale, StaleAccumulated}` of one cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DependencyCounterValues {
    pub find_witness_in: u64,
    pub buckets_allocated: u64,
    pub buckets_deallocated: u64,
    pub buckets_stale: u64,
    pub buckets_stale_accumulated: u64,
}


/// Lock-free-read, copy-on-write sharded compiled-method cache.
///
/// A lookup touches one immutable shard snapshot and clones only the returned
/// `Arc<CompiledMethod>`. Mutations are serialized because publication and
/// dependency invalidation are rare and must be atomic as a group; only the
/// affected shard map is copied for an ordinary tier-up publication.
pub struct JitCache {
    pub(crate) shards: Box<[JitCacheShard]>,
    pub(crate) reflect_epoch: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub(crate) mutation: parking_lot::Mutex<()>,
    /// Highest [`JIT_INSTALL_EPOCH`] value this cache has been flushed at.
    ///
    /// An artifact whose [`CompiledMethod::install_epoch`] is below this began
    /// compiling before the flush, so it was compiled against bytecode (or a
    /// field layout) the flush exists to retire. [`Self::put`] and
    /// [`Self::put_osr`] refuse it.
    ///
    /// Raised only by [`Self::clear_all`], under `mutation` — the same lock the
    /// two publication paths hold — so a publication either completes entirely
    /// before the flush (and is then wiped by it) or observes the raised
    /// barrier. There is no third outcome.
    ///
    /// Deliberately per-cache rather than a global watermark: `clear_all` is
    /// fanned out across every live VM by
    /// `vm/src/vm/vm_init.rs::jit_invalidate_adapter`, so a real flush still
    /// arms every cache, while a *test* flushing its own `JitCache` cannot
    /// refuse a concurrent test's publication into a different one.
    pub(crate) flush_barrier: std::sync::atomic::AtomicU64,
    /// How many multi-class redefinition calls of this cache's VM are
    /// installing right now ([`Self::hold_publications`]; interpreter round i1
    /// wave 40, lane L2 follow-up). While it is non-zero, [`Self::put`] and
    /// [`Self::put_osr`] publish nothing: a body compiled from a class the
    /// call has already installed would otherwise run beside a compiled body
    /// of a class it has not installed yet, which the call's install fence
    /// cannot stop (it holds interpreter loops only).
    pub(crate) publication_holds: std::sync::atomic::AtomicU32,
    /// Every class and member signature that appears in some published body's
    /// [`CompiledMethod::inlined_methods`] — an over-approximation, and
    /// deliberately so.
    ///
    /// `invalidate_for_class` and `invalidate_for_class_change` match ONLY on
    /// `inlined_methods`, and they run on every class definition. Almost no
    /// definition invalidates a CHA assumption, so almost every call walked all
    /// 128 shards' method and OSR maps to conclude nothing. `invalidate_matching`
    /// already bails before its three *later* whole-cache passes once the match
    /// set comes back empty; this is the same argument moved in front of the
    /// scan that produces it.
    ///
    /// Over-approximate is the safe direction: a name in this map only means
    /// the scan runs, which is the old behaviour. A name absent from it means no
    /// published body names that class, so the predicate is false everywhere and
    /// the scan could only have returned 0. Entries are therefore never removed
    /// on eviction — only [`Self::clear_all`] empties it, alongside the maps it
    /// summarises.
    pub(crate) inlined_class_names: parking_lot::Mutex<
        std::collections::HashMap<Arc<str>, std::collections::HashSet<(Arc<str>, Arc<str>)>>,
    >,
    /// The dependency records of the published bodies, for the perf counters
    /// (see [`DependencyBucketBook`]).
    pub(crate) dependency_buckets: DependencyBucketBook,
    /// Every dependency invalidation of this cache, logged BEFORE its scan, so
    /// a publication whose compilation began earlier can be checked against it
    /// (see [`Self::dependencies_are_current`]).
    pub(crate) invalidations: parking_lot::Mutex<InvalidationLog>,
    /// This cache's publication generation: advanced by every `put` and
    /// `put_osr`, and by an `invalidate_matching` or `clear_all` that removed
    /// something. Starts at 1, so 0 stays the interpreter memos' "never probed"
    /// value. See [`Self::generation`].
    pub(crate) generation: std::sync::atomic::AtomicU64,
    /// Class redefinitions this VM has performed. See [`Self::redefine_epoch`].
    pub(crate) redefine_epoch: std::sync::atomic::AtomicU32,
    /// Publications (`put`, `put_osr`) per `(method name, descriptor)` bucket
    /// ([`callee_publish_bucket`]); never decremented, and a retirement does
    /// not move them. See [`Self::callee_publish_stamp`] (interpreter round
    /// i4 wave 92, lane L7).
    pub(crate) callee_publish_buckets: Box<[std::sync::atomic::AtomicU64]>,
    /// [`Self::clear_all`] flushes of this cache; part of every bucket's
    /// [`Self::callee_publish_stamp`].
    pub(crate) callee_publish_flushes: std::sync::atomic::AtomicU64,
    /// This cache's cold-body sweep state — RT-8. See
    /// [`Self::sweep_cold_bodies`].
    ///
    /// Per-cache, not a process global: the bodies are this VM's, the usage
    /// signal is this VM's, and a sweep of one VM's cache must not be
    /// rate-limited by another's. `COMMITTED_JIT_CODE_BYTES` and the cap are
    /// process-wide because the *address space* is; the policy is not.
    pub(crate) sweep: parking_lot::Mutex<CodeCacheSweepState>,
    /// Every published body's [`JitCycleEdgeCell`]s, by the
    /// [`compute_jit_key_hash`] of the method each one waits for (round 9
    /// wave 7, `mutrec7`). `Weak`: the owning artifact owns the cell. Read and
    /// written only under `mutation` (by `put`, the invalidation pass and
    /// `clear_all`), so a fill and a clear can never interleave. Per cache,
    /// like the bodies the cells point at.
    pub(crate) cycle_cells: parking_lot::Mutex<FxHashMap<u64, Vec<std::sync::Weak<JitCycleEdgeCell>>>>,
    /// Every published body's RETIRE cells ([`JIT_RETIRE_CELL_GUARD`]), by the
    /// entry each one is pinned to (round 9 wave 9, `evict9`). Same discipline
    /// as `cycle_cells`: `Weak`s, touched only under `mutation`.
    pub(crate) retire_cells: parking_lot::Mutex<FxHashMap<usize, Vec<std::sync::Weak<JitCycleEdgeCell>>>>,
    /// Not-entrant stubs forwarding to a body that reaches them back (a
    /// reference cycle; strings round, `str16`), by the TARGET's entry: each
    /// one the patched body's `(entry, artifact_id)`, resolved through
    /// [`jit_entry_owners`] when the target is withdrawn, so the forward's
    /// owner is released and the cycle broken
    /// ([`Self::release_cycle_forwards`]). Same discipline as `cycle_cells`:
    /// touched only under `mutation`.
    pub(crate) cycle_forwards: parking_lot::Mutex<FxHashMap<usize, Vec<(usize, u64)>>>,
    /// Diagnostic counters for `cycle_forwards`: forwards installed across a
    /// cycle, and owners released again when the target was withdrawn
    /// ([`Self::cycle_forward_census`]). Per cache, not process statics.
    pub(crate) cycle_forwards_installed: std::sync::atomic::AtomicU64,
    pub(crate) cycle_forwards_released: std::sync::atomic::AtomicU64,
    /// `CRATONVM_JIT_IC_SUPERSEDE_RETARGET`, default ON (`=0` is the kill
    /// switch), read once per cache: whether [`Self::put`] retires the inline
    /// caches that still publish the body it supersedes (round 11 wave 19,
    /// lane lhm; [`CompiledMethod::retarget_superseded_inline_caches`]). A
    /// field rather than a `static` flag cache: it is per VM like the cache.
    pub(crate) ic_supersede_retarget: bool,
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION` (default OFF, read once per
    /// cache; interpreter round i6 wave 117, lane L2, page
    /// `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
    /// whether [`Self::mark_reentry_only`] keys are honoured. Off, no body is
    /// ever re-entry-only and every answer below is the historical one.
    pub(crate) reentry_only_enabled: bool,
    /// Name hashes ([`compute_jit_key_hash`] with class id `0`) of the
    /// methods whose bodies publish RE-ENTRY-ONLY
    /// ([`CompiledMethod::reentry_only`]): a JDK method shadowed by a
    /// registered `Intrinsic` whose own bytecode the natives re-enter. A hash
    /// collision only makes another method's body re-entry-only too (fewer
    /// binds, never more), so the set needs no full keys. Per cache (per VM).
    pub(crate) reentry_only_keys: parking_lot::RwLock<FxHashSet<u64>>,
    /// `reentry_only_keys` is non-empty: the per-call question is one load.
    pub(crate) reentry_only_any: std::sync::atomic::AtomicBool,
    /// `CRATONVM_JIT_INVALIDATION_RETIRES_SUPERSEDED`, default OFF, read once
    /// per cache (round 15 wave 5, lane vh7; VH6-1 of
    /// `jit-r15w4-vh6-proposals-RETIRED-20261003.md`): whether [`Self::invalidate_for_class`],
    /// [`Self::invalidate_for_class_change`] and
    /// [`Self::invalidate_unloaded_classes`] also retire the live SUPERSEDED
    /// bodies that match their predicate or bake a raw call into what they
    /// withdraw, as the first-exact-mint sweep does since round 15 wave 4
    /// ([`JitCache::invalidate_matching_collecting_with`]). Off until a
    /// define-heavy run prices the extra registry walk.
    pub(crate) invalidation_retires_superseded: bool,
    /// The bodies THIS cache's `put` / `put_osr` displaced (j27 wave 4, lane
    /// GLOBALS; R15B-23): the candidates of the superseded half of
    /// [`JitCache::invalidate_matching_collecting_with`], which used to walk
    /// the process-wide entry-owner table and so reached other VMs' bodies.
    pub(crate) superseded_bodies: superseded_bodies::SupersededBodies,
    /// Classes some compile for this cache COPIED method bytecode of into
    /// another method's code, or published under another class's key: a body
    /// spliced at any depth by either tier, a constructor elided as proven
    /// empty, an inherited method's body keyed by its receiver class.
    /// [`Self::note_bytecode_copied`];
    /// the VM marks a class under its class-manager read guard BEFORE it
    /// reads the bytecode, and asks [`Self::bytecode_was_copied`] under the
    /// writer that replaced it, so a copy either is marked in time or read
    /// the new bytecode.
    ///
    /// The IR tier records no `inlined_methods` for what it splices, the
    /// single-pass plan records only its outer sites, and an elided
    /// constructor is recorded nowhere, so a redefinition of a marked class
    /// cannot be scoped to its dependents and takes the full flush
    /// (interpreter round i1 wave 20, lane L1; see
    /// [`Self::invalidate_for_redefinition`]). Never cleared: a compile
    /// published outside the install witness (stamped at finalize) could have
    /// copied the class before a flush and published after it.
    ///
    /// Lock order (JIT r22 wave 2, lane inline; item 4 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): a LEAF.
    /// `dependencies_current_since` takes it under `invalidations` (through
    /// `InvalidationRecord::withdraws_in` ->
    /// `finalizer_copy_unrecorded_on_body`), and its only takers,
    /// [`Self::note_bytecode_copied`] and [`Self::bytecode_was_copied`],
    /// acquire nothing while holding it. Keep it that way: a lock taken under
    /// this one would make the `invalidations -> copied_bytecode_classes`
    /// nesting a potential cycle.
    pub(crate) copied_bytecode_classes: parking_lot::Mutex<FxHashSet<cratonvm_types::ClassId>>,
    /// The VM's not-entrant helper and `SharedVm`, once the VM armed this
    /// cache ([`Self::arm_not_entrant`]); `None` patches nothing.
    pub(crate) not_entrant_hook: parking_lot::Mutex<Option<NotEntrantHook>>,
    /// Bodies [`Self::make_not_entrant`] patched, and bodies it was asked to
    /// patch but left alone. See [`Self::not_entrant_counts`].
    pub(crate) not_entrant_patched: std::sync::atomic::AtomicU64,
    pub(crate) not_entrant_refused: std::sync::atomic::AtomicU64,
    /// This VM's shared megamorphic dispatch table (round 12 wave 2, lane
    /// calls, proposal W2-1), allocated on the first publication into it
    /// ([`Self::mega_dispatch_table`]). Every pass below that retires
    /// inline-cache ways retires its ways too.
    pub(crate) mega_dispatch: std::sync::OnceLock<Box<MegaDispatchTable>>,
    /// This VM's lambda-adapter thunks ([`crate::lambda_adapter`]); see
    /// [`Self::lambda_adapters`]. Per cache, not a process `OnceLock`, so a
    /// dropped VM's thunks and the impl bodies they root die with it (t1 wave
    /// 2, lane Y).
    pub(crate) lambda_adapters: crate::lambda_adapter::LambdaAdapterMap,
    /// This VM's `--real-jdk-report` sinks ([`crate::jdk_only_sink`]); see
    /// [`Self::jdk_only_sinks`] (t1 wave 3, lane Y).
    pub(crate) jdk_only_sinks: crate::jdk_only_sink::JdkOnlyReportSinks,
    /// [`Self::make_not_entrant`]'s refusals by reason: one slot per
    /// [`NotEntrantRefusal`] ([`NotEntrantRefusal::index`]), then the bodies
    /// still published (interpreter round i1 wave 23, lane L6). Printed on the
    /// pass's `CRATONVM_DBG_JITC` summary line.
    pub(crate) not_entrant_refusals: [std::sync::atomic::AtomicU64; not_entrant::NOT_ENTRANT_REFUSAL_SLOTS],
    /// Bodies a redefinition withdrew
    /// ([`Self::bodies_withdrawn_by_redefinition`], wave 23, lane L6), plus
    /// one per whole-cache redefinition (wave 24,
    /// [`Self::note_whole_cache_redefinition`]).
    pub(crate) withdrawn_by_redefinition: std::sync::atomic::AtomicU64,
    /// The install barrier (`flush_barrier`) of this cache's last whole-cache
    /// redefinition's flush, `0` before the first: every body whose
    /// compilation began earlier may run a redefined class's old bytecode,
    /// whether or not this cache can still reach it (interpreter round i1
    /// wave 24, lane L6; [`Self::body_withdrawn_by_redefinition`]).
    pub(crate) whole_cache_redefinition_barrier: std::sync::atomic::AtomicU64,
    /// Per redefined class, the [`JIT_INSTALL_EPOCH`] value its last scoped
    /// redefinition ([`Self::invalidate_for_redefinition`]) stood at: a body
    /// of that class whose compilation began earlier runs bytecode the class
    /// no longer has. The per-class half of `flush_barrier` for
    /// [`Self::compiled_since_redefinition_of`]; it refuses no publication
    /// (the invalidation log does). Emptied by [`Self::clear_all`], whose
    /// barrier is above every entry.
    pub(crate) redefinition_barriers: parking_lot::Mutex<FxHashMap<cratonvm_types::ClassId, u64>>,
    /// The VM's tiered manager, told of every withdrawal once the VM armed
    /// this cache ([`Self::install_withdrawal_sink`]); unset, withdrawals
    /// demote nothing (tests, `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0`). `Weak`:
    /// the manager is a sibling of this cache in the VM, not owned by it.
    pub(crate) withdrawal_sink: std::sync::OnceLock<std::sync::Weak<dyn JitBodyWithdrawalSink>>,
    /// Bodies [`Self::make_not_entrant`] retired but could not patch (any
    /// refusal but "still published"), each with its compile's
    /// constant-pool stamp ([`CompiledMethod::compile_cp_stamp`], `0` when
    /// unknown): a baked caller may still enter one and read its old
    /// constant-pool indices, so the VM keeps every redefinition step from
    /// the least live stamp on ([`Self::min_unpatched_cp_stamp`];
    /// interpreter round i1 wave 25, lane L6). A body is forgotten when it
    /// is dropped.
    pub(crate) unpatched_bodies: parking_lot::Mutex<Vec<(std::sync::Weak<CompiledMethod>, u64)>>,
    /// Every body of this cache's VM that recorded exit-poll sites
    /// ([`CompiledMethod::exit_poll_sites`]) and may still be alive: each one
    /// [`Self::put`] / [`Self::put_osr`] published, and each optimizing OSR
    /// body the VM built ([`Self::note_exit_poll_body`]). A class redefinition
    /// reaches through it the bodies a frame may still run although no map,
    /// baked root or memo names them any more -- a superseded method-entry
    /// body, an evicted OSR body, an optimizing OSR body the VM's memo forgot
    /// -- so their exit polls are forced too (interpreter round i1 wave 26,
    /// lane L6). Weak: the list keeps nothing alive; dead entries are pruned
    /// whenever its length reaches a power of two.
    pub(crate) exit_poll_bodies: parking_lot::Mutex<Vec<std::sync::Weak<CompiledMethod>>>,
    /// The value [`Self::bodies_withdrawn_by_redefinition`] answered after the last
    /// redefinition that withdrew a body whose exit polls it could not force,
    /// `0` for never ([`Self::every_withdrawal_forced_since`]; interpreter
    /// round i1 wave 26, lane L6).
    pub(crate) unforced_withdrawal_at: std::sync::atomic::AtomicU64,
    /// How many verdicts a forced post-call exit site has asked for, and how
    /// many of them sent the frame out ([`Self::note_post_call_exit_verdict`];
    /// interpreter round i1 wave 28, lane L6): the positive control of the
    /// wave-27 post-call exits, printed on the `exit polls forced:` line and
    /// by `CRATONVM_DBG_DEOPT`'s `post-call exit verdict` lines.
    pub(crate) post_call_exit_verdicts: std::sync::atomic::AtomicU64,
    pub(crate) post_call_exits_taken: std::sync::atomic::AtomicU64,
    /// JIT round 23 wave 1, lane L1 (THROWS22-1): `CRATONVM_JIT_OSR_PROFIT_EXIT`
    /// (default ON, `=0` kill) as this VM read it at construction
    /// ([`Self::arm_osr_profit_exit`]), so no flag lookup is made per poll
    /// and no process static holds it.
    pub(crate) osr_profit_exit: bool,
    /// Single-pass OSR bodies armed for a profit exit, profit verdicts that
    /// told a frame to leave, and profit exits the OSR door's sink took
    /// ([`Self::osr_profit_exit_counts`]). The first is also the poll slow
    /// path's one-load gate: zero means no body can be told to leave for
    /// profit.
    pub(crate) osr_profit_armed: std::sync::atomic::AtomicU64,
    pub(crate) osr_profit_verdicts: std::sync::atomic::AtomicU64,
    pub(crate) osr_profit_exits_taken: std::sync::atomic::AtomicU64,
    /// Armed bodies taken back by [`Self::disarm_osr_profit_exit`].
    pub(crate) osr_profit_disarms: std::sync::atomic::AtomicU64,
    /// gcg g6/jm: `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` (opt-in since vg6:
    /// jm6_argsp on = off) as this VM read it at construction -- read by the VM's dispatch
    /// doors on every megamorphic / PIC / MIC-hit call, so it is a per-VM
    /// bool, not a flag lookup per call and not a process static
    /// ([`Self::door_owned_args_unpinned`]).
    pub(crate) door_owned_args_unpinned: bool,
    /// gch h1/fr: `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS` (opt-in) as this VM
    /// read it at construction ([`Self::by_name_tail_unpins_args`]); asked on
    /// every by-name bytecode tail, so a per-VM bool like the field above.
    pub(crate) by_name_tail_unpins_args: bool,
    /// gch h2/fj: `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION` (opt-in) as this
    /// VM read it at construction ([`Self::refused_resume_restashes_after_collection`]);
    /// asked on every callee-trap resume of the VM's call-site service.
    pub(crate) refused_resume_restashes_after_collection: bool,
    /// gch h2/fj: `CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS` (default ON since the
    /// 2026-10-17 user-approved WARNING flip, `=0` kills) as this VM
    /// read it at construction ([`Self::deposit_locals_keep_liveness`]); asked
    /// by every in-native root deposit while compiled frames make the
    /// conservative-locals rule active.
    pub(crate) deposit_locals_keep_liveness: bool,
    /// gch h2/fj: `CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK` (default ON since JIT r20 GC wave 9,
    /// host w9c d9d A/B; `=0` kills) as this VM
    /// read it at construction ([`Self::interp_door_args_weak`]); asked by the
    /// interpreter's compiled-callee doors on every call.
    pub(crate) interp_door_args_weak: bool,
    /// gch h3/fd: `CRATONVM_JIT_FIRST_CALL_DOOR_ARGS_WEAK` (default ON since JIT r20 GC
    /// wave 9, host w9c d9d A/B; `=0` kills) as this
    /// VM read it at construction ([`Self::first_call_door_args_weak`]); asked
    /// by `interpreter::execute`'s first-call door on every compiled run.
    pub(crate) first_call_door_args_weak: bool,
    /// Round 16 lane chain (CH5W4-3 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): the
    /// chain points of every body this cache published or the VM built as an
    /// optimizing OSR body ([`Self::note_forced_chain_publication`]), not yet
    /// drained by the VM's eager-chains census
    /// ([`Self::take_forced_chain_publications`]). Only a body with forced
    /// blocks adds a row, so it stays empty unless `CRATONVM_DEOPT_EAGER_CHAINS`
    /// is on. Per cache, so per VM.
    pub(crate) forced_chain_publications: parking_lot::Mutex<Vec<deopt::PublishedChainPoints>>,
    /// nio21 wave 16 (lane L1): `CRATONVM_JIT_SUPERSEDE_REDIRECT` (default
    /// ON, `=0` is the kill switch) as this VM read it at construction. On,
    /// [`Self::put`] makes the body it displaces not entrant, its stub
    /// forwarding to the new body, so a caller that BAKED the old entry
    /// reaches the current body (HotSpot's `make_not_entrant` on tier-up).
    /// See [`Self::redirect_superseded_locked`].
    pub(crate) supersede_redirect: bool,
    /// Bodies [`Self::put`] displaced while the cache was not yet armed
    /// ([`Self::arm_not_entrant`]), by key hash. Arming redirects them to
    /// their current published body; publication also serves its own key.
    /// Weak: keeps nothing mapped.
    pub(crate) supersede_pending: parking_lot::Mutex<FxHashMap<u64, Vec<std::sync::Weak<CompiledMethod>>>>,
    /// `(patched, refused)` by [`Self::redirect_superseded_locked`]; kept
    /// apart from [`Self::not_entrant_counts`], which counts the
    /// redefinition's and the evictions' passes.
    pub(crate) supersede_redirects: [std::sync::atomic::AtomicU64; 2],
    /// JIT round 19 wave 11, lane tail11 (TAIL9-20-2, default on
    /// `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`): the hit/miss words of every
    /// single-pass guarded nested splice a body this cache was handed
    /// carries, keyed by the guarded call's site ([`SpNestedMissKey`]: the
    /// ENCLOSING spliced callee, its pc, the guard class) and summed over every
    /// body that spliced it ([`Self::sp_nested_miss_counts`]). Fed by
    /// [`Self::put`] / [`Self::put_osr`] from
    /// [`CompiledMethod::sp_nested_miss_cells`], which only a compile under the
    /// switch fills; emptied by a whole-cache flush. Per cache, so per VM.
    pub(crate) sp_nested_miss_evidence:
        parking_lot::Mutex<FxHashMap<SpNestedMissKey, Vec<SpNestedMissCounters>>>,
}

/// JIT round 19 wave 11, lane tail11 (TAIL9-20-2): one guarded nested splice
/// site of the single-pass tier, as [`JitCache::sp_nested_miss_counts`] is
/// asked about it: the `"class/Name.method:desc"` of the ENCLOSING spliced
/// callee, the call's pc in its bytecode, and the exact class the hit arm is
/// guarded on.
pub type SpNestedMissKey = (Box<str>, usize, u32);

/// The two words a guarded nested splice's arms bump
/// (`x64::inlining::SpNestedSpliceCensusCell`): leaked, so `'static`, and
/// baked into machine code as immediates.
#[derive(Clone, Copy, Debug)]
pub struct SpNestedMissCounters {
    pub hits: &'static std::sync::atomic::AtomicU64,
    pub misses: &'static std::sync::atomic::AtomicU64,
}

/// One row of [`CompiledMethod::sp_nested_miss_cells`].
#[derive(Clone, Debug)]
pub struct SpNestedMissEvidence {
    pub key: SpNestedMissKey,
    pub counters: SpNestedMissCounters,
}

/// `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE` -- JIT round 19 wave 11, lane
/// tail11 (TAIL9-20-2 of `jit-r19-tail9-proposals-RETIRED-20261009.md`), default on. On, each
/// single-pass guarded nested splice counts its hit and miss arms (a plain
/// `INC QWORD [R11]` per arm, 13 bytes; the `LOCK` form under the census), the
/// body hands the counters to its [`JitCache`], and the VM's nested resolver
/// refuses a guarded row whose MEASURED miss share is at or above
/// [`SP_NESTED_MISS_REFUSE_SHARE_PCT`] with at least
/// [`SP_NESTED_MISS_REFUSE_MIN_MISSES`] misses -- which refuses the ENCLOSING
/// splice ("neither spliced nor direct-bound"), so the outer call reaches the
/// enclosing callee's compiled body and its own inline cache instead of the
/// blind dispatch helper the miss edge calls. Off: nothing is emitted, fed or
/// asked. Read once per compile and per resolution; not cached in a static.
pub fn sp_nested_miss_share_refuse_enabled() -> bool {
    true
}

/// TAIL9-20-2's bar: the census book's "above ~5 %" (wave 8's break-even, a
/// ~175 ns blind dispatch against a ~5-10 ns call, puts it at ~3-6 %).
pub const SP_NESTED_MISS_REFUSE_SHARE_PCT: u64 = 5;

/// TAIL9-20-2's floor: a share is evidence only past this many misses, so a
/// warm-up miss or two in a short run never refuses a splice.
pub const SP_NESTED_MISS_REFUSE_MIN_MISSES: u64 = 1024;

/// `CRATONVM_JIT_SP_NESTED_STUCK_WITHDRAW` -- JIT round 19 wave 12, lane
/// tail12 (TAIL11-2 of `jit-r19-tail11-proposals-RETIRED-20261009.md`), default on, meaningful only
/// with [`sp_nested_miss_share_refuse_enabled`] (whose counters it reads). A
/// published single-pass body whose OWN guarded nested splice crossed
/// [`sp_nested_miss_share_refuses`] keeps its guard and its blind-dispatch miss
/// edge forever (the miss is a call, not a trap, so nothing recompiles it);
/// with this on, the VM's compiled-entry drain withdraws such a body once
/// ([`JitCache::claim_sp_nested_stuck_bodies`]), and its recompile plans
/// without the splice (TAIL9-20-2's resolver refuses the row). Read per drain;
/// not cached in a static.
pub fn sp_nested_stuck_withdraw_enabled() -> bool {
    true
}

/// TAIL9-20-2's verdict on summed counts: refuse at or above the bar, past
/// the floor.
pub fn sp_nested_miss_share_refuses(hits: u64, misses: u64) -> bool {
    misses >= SP_NESTED_MISS_REFUSE_MIN_MISSES
        && misses.saturating_mul(100)
            >= hits
                .saturating_add(misses)
                .saturating_mul(SP_NESTED_MISS_REFUSE_SHARE_PCT)
}

/// One [`JitCache`]'s cold-body sweep bookkeeping.
#[derive(Default)]
pub(crate) struct CodeCacheSweepState {
    /// `artifact_id -> the artifact's entry count at the previous sweep`.
    ///
    /// Keyed on [`CompiledMethod::artifact_id`] and not on the entry ADDRESS,
    /// for the reason that field's own doc gives: an executable buffer is
    /// recycled to the next compile of a similar size, so an address-keyed
    /// snapshot is an ABA — a fresh body would inherit its predecessor's
    /// reading and could be swept on its first sweep, having never been given
    /// a chance to run.
    ///
    /// Rebuilt from the live set on every sweep rather than updated in place,
    /// so rows for artifacts that are gone do not accumulate.
    pub(crate) seen: rustc_hash::FxHashMap<u64, u64>,
    /// Entry counts summed over the live set at the previous sweep, or `None`
    /// before the first. Half of the rate limit: see [`Self::plan`].
    pub(crate) total_entries: Option<u64>,
    /// The caller's progress clock at the previous sweep, or `None` before the
    /// first. The other half of the rate limit: see [`Self::plan`].
    pub(crate) progress: Option<u64>,
    /// Sweeps this cache has performed, census included.
    pub(crate) sweeps: u64,
    /// JIT round 22 wave 11, lane runtime (reporting only, per cache so per
    /// VM): calls refused because nothing ran since the previous sweep.
    pub(crate) refused_nothing_ran: u64,
    /// Sweeps that looked and found no cold body.
    pub(crate) found_nothing_cold: u64,
    /// Cold candidates planned, and bodies the withdrawals took, over all sweeps.
    pub(crate) candidates_total: u64,
    pub(crate) swept_total: u64,
}

impl CodeCacheSweepState {
    /// Decide which of `live` is cold and advance this cache's sweep state.
    ///
    /// `live` is `(artifact_id, entry count, entry address)` for every
    /// published, not-already-retired method-entry body; `total_entries` is the
    /// sum of those counts. Split out from [`JitCache::sweep_cold_bodies`]
    /// because it is the whole policy and everything around it needs real
    /// compiled artifacts — a policy that can only be exercised end to end is
    /// one whose "it retired nothing" outcome cannot be told from "it was never
    /// allowed to look".
    ///
    /// Returns the entry addresses to withdraw and, when that set is empty for
    /// a reason other than "nothing was cold", which reason.
    ///
    /// `progress` is a monotonic clock of work the PROCESS did, supplied by the
    /// caller -- the VM passes its interpreter tier-up tick. It is what makes
    /// "no body moved" mean something.
    pub(crate) fn plan(
        &mut self,
        live: &[(u64, u64, usize)],
        total_entries: u64,
        progress: u64,
    ) -> (FxHashSet<usize>, Option<JitCodeCacheSweepRefusal>) {
        // The rate limit: refuse only when NOTHING happened -- no compiled body
        // entered AND no progress on the caller's clock. Then every reading is
        // the one the last sweep saw and a pass would find the entire cache
        // "cold" for no reason but having been asked twice.
        //
        // It used to refuse whenever no compiled body had been entered, and the
        // RT-8 soak (2026-09-18, `NOTES-abi8.md`) found that to be the exact
        // case the sweeper exists for. A program that moves on to a new phase
        // at the cap runs its new code in the INTERPRETER -- the cap refuses
        // every compile -- and stops entering the old phase's bodies. So no
        // counter moves, and the sweeper refused on every call for the rest of
        // the process while a cache full of dead code held the cap. The
        // interpreter doing thousands of invocations while no compiled body is
        // entered is not "nothing ran"; it is the strongest coldness evidence
        // the cache can get.
        if self.total_entries == Some(total_entries) && self.progress == Some(progress) {
            self.refused_nothing_ran += 1;
            return (
                FxHashSet::default(),
                Some(JitCodeCacheSweepRefusal::NothingRanSinceTheLastSweep),
            );
        }
        let census_only = self.sweeps == 0;

        let mut doomed = FxHashSet::default();
        if !census_only {
            for &(artifact_id, count, entry) in live {
                // `Some(&count)`, not `count <= previous`: an artifact ABSENT
                // from the previous snapshot is one published since it, and it
                // has not yet had a chance to run. Treating "no previous
                // reading" as "has not moved" would retire every body on the
                // first sweep after it was compiled, which is the precise
                // shape of sweeping away the method that just went hot.
                if self.seen.get(&artifact_id) == Some(&count) {
                    doomed.insert(entry);
                }
            }
        }

        // Rebuilt, not updated: rows for artifacts that have gone must not
        // accumulate, and `artifact_id` is monotonic so nothing ever reuses a
        // key that would let them be overwritten instead.
        self.seen.clear();
        self.seen.reserve(live.len());
        for &(artifact_id, count, _) in live {
            self.seen.insert(artifact_id, count);
        }
        self.total_entries = Some(total_entries);
        self.progress = Some(progress);
        self.sweeps += 1;

        if census_only {
            (
                FxHashSet::default(),
                Some(JitCodeCacheSweepRefusal::FirstSweepIsACensus),
            )
        } else {
            if doomed.is_empty() {
                self.found_nothing_cold += 1;
            }
            self.candidates_total += doomed.len() as u64; // Widening: usize -> u64
            (doomed, None)
        }
    }
}

/// CODECACHE21-6 (tag cc21): the artifact ids of the `live` rows (`(artifact_id,
/// entry count, entry address)`) whose entry address `plan` doomed. Artifact ids
/// are monotonic and never reused, so a withdrawal keyed on them cannot hit a
/// body that merely inherited a freed buffer's address.
pub(crate) fn doomed_artifact_ids(live: &[(u64, u64, usize)], doomed: &FxHashSet<usize>) -> FxHashSet<u64> {
    live.iter()
        .filter(|&&(_, _, entry)| doomed.contains(&entry))
        .map(|&(artifact_id, _, _)| artifact_id)
        .collect()
}

/// Why a [`JitCache::sweep_cold_bodies`] call retired nothing.
///
/// Every arm is a refusal to act, never a silent no-op: a sweeper that cannot
/// tell "there was nothing cold" from "I was never allowed to look" is one
/// whose nightly lane passes for the wrong reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitCodeCacheSweepRefusal {
    /// `CRATONVM_JIT_CODE_CACHE_SWEEP` is off, which is the default.
    Disabled,
    /// `CRATONVM_JIT_ENTRY_COUNTER` is off, so every body's entry count reads
    /// zero and "has not moved since the last sweep" is true of ALL of them.
    /// Sweeping on that signal would flush the whole cache on its second call.
    NoUsageSignal,
    /// The first sweep of this cache only records the counts. Nothing can be
    /// known to be cold until there are two readings to compare, and a body
    /// published between the cap being hit and this call has by construction
    /// not been entered yet.
    FirstSweepIsACensus,
    /// Nothing happened since the previous sweep: no compiled body in this
    /// cache was entered AND the caller's progress clock did not advance, so
    /// every reading is the one that sweep already saw and a pass would find
    /// the entire cache cold merely for having been asked twice.
    ///
    /// Compiled entries alone are NOT the test. A program that has moved on at
    /// the cap runs its new code interpreted and enters none of its old
    /// bodies; that is coldness, and refusing on it left the sweeper unable to
    /// act in exactly the case it exists for. See [`CodeCacheSweepState::plan`].
    NothingRanSinceTheLastSweep,
}

/// `(class, method, descriptor, class_id)` -- the identity of one compiled
/// method, in the row shape [`JitCache::snapshot_entry_counts`] produces, so a
/// caller can build a `tiered::MethodKey` from it exactly as it already does
/// there.
pub type WithdrawnMethodKey = (Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId);

/// Who hears that a [`JitCache`] withdrew bodies: its VM's tiered manager
/// (tier proposal W3-1, round 12 wave 4).
///
/// `current_tier` only advances when a body is published, and a method at C2
/// is SETTLED, so its call sites skip the manager until the settled
/// generation moves. A withdrawal the manager never hears of therefore leaves
/// the method at its old tier over an empty cache: interpreted, and never
/// offered again. The sweeper and the redefinition paths demoted by hand; the
/// class-hierarchy door (`invalidate_for_class_change`), the define doors
/// (`invalidate_for_class`, one of them in `native/jni.rs`), the unload door
/// (`invalidate_unloaded_classes`), a deopt eviction's reverse closure
/// (`remove`) and the test-side `clear_all`s did not
/// (`r12w3-tier-inlining-invalidation-leaves-withdrawn-methods-settled-patch`).
/// Reporting from the two functions every withdrawal goes through
/// (`JitCache::invalidate_matching_collecting` and
/// [`JitCache::clear_all_collecting`]) makes the demotion a property of the
/// cache rather than of each caller.
pub trait JitBodyWithdrawalSink: Send + Sync {
    /// `keys` lost their method-entry body in one withdrawal (a flush: any
    /// body), and none of them has had an optimizing method-entry body
    /// published again since. A key that lost only its OSR body is left out:
    /// `current_tier` never described it. Called with no cache lock held;
    /// deduplicated.
    fn bodies_withdrawn(&self, keys: &[WithdrawnMethodKey]);

    /// JIT round 21 wave 4 (lane codecache; rev-x64 item 8): `keys` lost an
    /// OPTIMIZING body (method-entry, or OSR under CODECACHE21-4) to a
    /// dependency invalidation or a trap eviction -- what HotSpot counts as a
    /// decompilation -- deduplicated. Charged to the method's recompile cutoff
    /// only; no tier changes. Called with no cache lock held. Nothing by
    /// default.
    fn optimizing_bodies_invalidated(&self, _keys: &[WithdrawnMethodKey]) {}

    /// STASH20-1 (JIT round 20 wave 19, lane yStash): must a statically bound
    /// call into `(class_name, method_name, descriptor)` of the loaded class
    /// `class_id` stay on the dispatch helper (a sticky recursion-cycle
    /// member, `JitVerdictRegistry::direct_call_requires_dispatch_of`)? The
    /// VM's not-entrant entry refuses such a method's forward, so the cache
    /// leaves its displaced body entrant
    /// (`not_entrant::supersede_skip_refused_forward_enabled`). Called with
    /// the cache's `mutation` lock held; must take no cache lock. `false` by
    /// default.
    fn forward_must_dispatch(
        &self,
        _class_name: &str,
        _method_name: &str,
        _descriptor: &str,
        _class_id: u32,
    ) -> bool {
        false
    }
}

/// What one [`JitCache::sweep_cold_bodies`] call did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JitCodeCacheSweepReport {
    /// Published method-entry bodies examined.
    pub observed: usize,
    /// Bodies the coldness predicate selected.
    pub candidates: usize,
    /// Cache entries actually withdrawn. Can EXCEED `candidates`: eviction
    /// goes through [`JitCache::invalidate_matching`], which withdraws the
    /// transitive reverse closure over baked direct calls and lambda adapters
    /// as well. It can also be smaller than the bytes a reader expects, since
    /// a body pinned by a caller's `_direct_callee_roots` is withdrawn from
    /// the cache without its mapping being released.
    pub swept: usize,
    /// Set when nothing was retired *and* the reason was not "nothing cold".
    pub refused: Option<JitCodeCacheSweepRefusal>,
    /// The identity of each COLD CANDIDATE that was withdrawn, as
    /// `(class, method, descriptor, class_id)` — the same row shape
    /// [`JitCache::snapshot_entry_counts`] produces, so the VM can build a
    /// `tiered::MethodKey` from it exactly as it already does there.
    ///
    /// The caller owes each of these a `TieredCompilationManager::note_body_withdrawn`
    /// unless the cache is armed with the VM's [`JitBodyWithdrawalSink`]
    /// ([`JitCache::reports_withdrawals`], the default since round 12 wave 4),
    /// which has then already demoted every one of them that lost its
    /// method-entry body. That is not optional bookkeeping: `current_tier`
    /// only advances, so a method whose body was swept while the manager still
    /// believes it is at C1 can never be offered for compilation again. See
    /// that function.
    ///
    /// **The closure is included.** This used to be candidates only: a body
    /// withdrawn by `invalidate_matching`'s transitive reverse closure — a
    /// caller that baked a direct call into a cold callee, or a lambda adapter
    /// reaching one — was absent, because the closure was computed inside that
    /// function and never handed back. Those methods lost their bodies WITHOUT
    /// being demoted, and since `current_tier` only advances they could never be
    /// offered for compilation again.
    ///
    /// `JitCache::invalidate_matching_collecting` now returns the identities it
    /// withdrew and this list is built from them, so `withdrawn.len()` accounts
    /// for `swept` rather than for `candidates`. `swept` can still exceed
    /// `withdrawn.len()`: a cache entry whose key names a method that another
    /// withdrawn entry also names — a method-entry body and an OSR body for the
    /// same method — is two entries and one demotion.
    pub withdrawn: Vec<WithdrawnMethodKey>,
    /// Bytes of executable buffer this sweep WITHDREW from the cache, measured
    /// the way `JitCodeReclamationStats::withdrawn_bytes` measures them
    /// (`_buffer.capacity()`), so the two cannot disagree.
    ///
    /// This is an UPPER BOUND on what the sweep can return to the OS, and never
    /// a statement that it did. A withdrawn body's mapping is released only when
    /// its last owner goes, and that is later: the retirement queue holds it
    /// until no thread can re-enter it, and a body another body still pins --
    /// through a caller's `_direct_callee_roots`, for as long as that caller
    /// itself survives -- stays mapped. What actually came back is
    /// `jit_code_reclamation_stats().reclaimed_bytes`, read after a drain.
    ///
    /// Why it exists: RT-8 / REVIEW-NOTE 8 -- "an eviction policy that counts
    /// bytes freed by number of methods evicted will be wrong by the inlining
    /// graph". Before this field the report offered a policy nothing BUT counts
    /// (`swept`, `withdrawn.len()`), so any policy sizing a sweep had to reason
    /// in methods. Bodies differ in size by orders of magnitude; counting them
    /// is not measuring space.
    pub withdrawn_bytes: u64,
}

/// `CRATONVM_JIT_CODE_CACHE_SWEEP=1` — arm the cold-body sweeper (RT-8).
///
/// **Default OFF.** It has two lanes now: the `code-cache-1mb-sweep` nightly
/// axis, which runs the corpus with the interpreter as the oracle, and
/// `vm/tests/code_cache_sweep_phases.rs`, which is the one that fills the cap
/// and sweeps. Fuzz programs are too small to reach 1 MiB. The first soak
/// (2026-09-18, `NOTES-abi8.md` RT-8) found four defects that together kept
/// the sweeper from freeing a byte. Flipping the default is a separate
/// decision from those fixes.
///
/// Read once and memoised, like every other compile-path knob here: a sweeper
/// that could be armed mid-run would have taken its census under one policy and
/// its decision under another.
pub(crate) fn code_cache_sweep_enabled() -> bool {
    use std::sync::OnceLock;
    static ARMED: OnceLock<bool> = OnceLock::new();
    *ARMED.get_or_init(|| false)
}

pub(crate) static JIT_ENTRY_OWNERS: std::sync::OnceLock<
    parking_lot::Mutex<FxHashMap<usize, std::sync::Weak<CompiledMethod>>>,
> = std::sync::OnceLock::new();
pub(crate) static JIT_CACHE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// Bit at which [`jit_cache_generation`] carries its RETIRE count (rt
/// proposal W8-1). The low half counts publications. The high half is bumped
/// whenever a body is withdrawn or an OSR body replaced, and whenever a
/// `CompiledMethod` that owns `JitInvokeInfo`s is freed. A `put` replacement
/// is publish-only: it marks the old body [`CompiledMethod::superseded`]
/// instead (round 11 wave 11). The thread-local site memos in
/// `vm/src/jit/helpers.rs` that hold only positive answers compare the high
/// half alone, so they survive publications that retire nothing. A carry out
/// of the low half reads as a retire: a spurious flush, never a missed one.
pub const JIT_CACHE_RETIRE_SHIFT: u32 = 32;
pub(crate) const JIT_CACHE_RETIRE_UNIT: u64 = 1 << JIT_CACHE_RETIRE_SHIFT;
/// How many compiled-frame activations are in flight process-wide.
///
/// Striped per thread: this is incremented and decremented on every
/// interpreter/JIT boundary crossing, and read only when an artifact is
/// retired. As one shared `AtomicUsize` it was a contended read-modify-write
/// on a single cache line twice per Java call — see
/// [`cratonvm_types::striped_counter`]. Folded (t1 wave 8, lane I25): its
/// readers also count the door entries [`jit_execution_enter_door`] puts in
/// `cratonvm_types::striped_counter::DOOR_DEPTH`.
pub(crate) static ACTIVE_JIT_EXECUTIONS: cratonvm_types::striped_counter::DoorFoldedCounter =
    cratonvm_types::striped_counter::DoorFoldedCounter::new();
/// One owner handed to the retirement queue.
pub(crate) struct DeferredOwner {
    /// Retirement generation stamped AFTER the owner's raw targets were
    /// unpublished. A thread that returns to JIT depth 0 after this generation
    /// cannot hold the body on its stack.
    pub(crate) retired_gen: u64,
    /// Start of the body's executable buffer: the identity a blocked thread's
    /// stack summary records.
    pub(crate) buffer_start: usize,
    /// Reserved bytes of that buffer, for the queue gauge.
    pub(crate) bytes: u64,
    /// Never read: the entry exists to be DROPPED, under `AuthorisedReclaim`,
    /// once the queue's proof covers `retired_gen` / `buffer_start`.
    #[allow(dead_code)]
    pub(crate) owner: RetiredCode,
}

/// What a retirement-queue entry keeps alive until it is safe to release.
pub(crate) enum RetiredCode {
    /// A compiled artifact whose raw targets its releaser already unpublished
    /// — the ordinary path, [`defer_jit_owner`].
    Owner(Arc<CompiledMethod>),
    /// The executable mapping of a PUBLISHED body whose last owner was dropped
    /// outside the queue. `ExecutableBuffer::drop` rescues it here instead of
    /// unmapping it; see [`defer_orphaned_mapping`].
    Mapping(crate::exec_memory::ExecutableBuffer),
    /// The side tables a rescued body's code addresses ([`RescuedSideTables`]),
    /// queued under the same buffer start as its [`Self::Mapping`] (JIT round
    /// 21 wave 4, lane codecache).
    #[allow(dead_code)]
    SideTables(Box<RescuedSideTables>),
}

/// What a body's machine code reads or calls through by baked address, moved
/// out of a [`CompiledMethod`] whose mapping its `ExecutableBuffer::drop` is
/// about to RESCUE into the retirement queue (JIT round 21 wave 4, lane
/// codecache; `CRATONVM_JIT_RESCUE_KEEPS_SIDE_TABLES`, default ON, `=0` frees
/// them with the artifact as before).
///
/// The rescue keeps the MAPPING because a thread may still be inside it: the
/// cascaded release of round 14 (`reclaim_is_cascaded`: a callee whose last
/// owner was another body freed on per-thread evidence, never checked against
/// the blocked threads' summaries itself) and a stray last-owner drop. Such a
/// thread executes the body's code, and that code dereferences these boxes
/// with no further check: an inline-cache call is `MOV R11,[slot+8]; CALL R11`
/// on a `JitMICSlot`, a dispatch passes a `JitInvokeInfo` address, a deopt stub
/// bakes a `DeoptimizationPoint`, the not-entrant stub bakes its record, a raw
/// `CALL rel32` reaches a callee only `_direct_callee_roots` keeps mapped. Until
/// this they were freed with the artifact while its mapping waited, so a parked
/// frame of a cascaded body resumed into code whose tables were heap garbage.
///
/// Queued under the body's own buffer start, so the drain releases it on the
/// proof that releases the mapping (no thread can be inside that buffer); the
/// two entries are independent and either may go first. Declaration order is
/// drop order: nothing here is read by another field's drop.
#[allow(dead_code)]
pub(crate) struct RescuedSideTables {
    pub(crate) strings: Vec<Box<str>>,
    pub(crate) invoke_infos: Vec<Box<JitInvokeInfo>>,
    pub(crate) vh_judged_sites: Vec<Box<VarHandleJudgedSite>>,
    pub(crate) entry_counter: Option<Box<entry_counter::JitEntryCounter>>,
    pub(crate) c1_branch_counters: Option<Arc<profile::BranchCounters>>,
    pub(crate) local_handler_sites: Vec<Box<JitLocalHandlerSite>>,
    pub(crate) mic_slots: Vec<Box<JitMICSlot>>,
    pub(crate) pic_slots: Vec<Box<JitPICSlot>>,
    pub(crate) cycle_cells: Vec<Arc<JitCycleEdgeCell>>,
    pub(crate) typecheck_seen_cells: Vec<Box<TypecheckSeenCell>>,
    pub(crate) direct_callee_roots: Vec<RetainedCode>,
    pub(crate) deopt_point_boxes: Vec<Box<deopt::DeoptimizationPoint>>,
    pub(crate) forced_chain_deopt_counters: Vec<Box<deopt::ForcedChainTripCell>>,
    /// CODECACHE21-5 (tier2 wave 3, tag cc21): the
    /// back-edge poll-stress countdown cells, baked as imm64s like the chain
    /// counters above (`CompiledMethod::_poll_stress_cells`). A rescued body
    /// under `CRATONVM_DBG_JIT_DEOPT_A_LOT` / `CRATONVM_DBG_GC_AT_BACK_EDGE`
    /// decremented a freed `AtomicU32` at its next back edge.
    pub(crate) poll_stress_cells: Vec<Box<std::sync::atomic::AtomicU32>>,
    pub(crate) class_init_cells: Vec<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    pub(crate) not_entrant: Option<Box<NotEntrantRecord>>,
}

// SAFETY: the contents are fields of a `CompiledMethod` (which is `Send`, see
// its impl) moved out of it unchanged; the raw pointers inside them address
// their own boxes, not anything tied to a thread.
unsafe impl Send for RescuedSideTables {}

impl RescuedSideTables {
    /// Move every baked side table out of `body`, leaving empty ones behind.
    pub(crate) fn take_from(body: &mut CompiledMethod) -> Self {
        use std::mem::take;
        Self {
            strings: take(&mut body._jit_strings),
            invoke_infos: take(&mut body._jit_invoke_infos),
            vh_judged_sites: take(&mut body._jit_vh_judged_sites),
            entry_counter: body._jit_entry_counter.take(),
            c1_branch_counters: body._jit_c1_branch_counters.take(),
            local_handler_sites: take(&mut body._jit_local_handler_sites),
            mic_slots: take(&mut body._jit_mic_slots),
            pic_slots: take(&mut body._jit_pic_slots),
            cycle_cells: take(&mut body._jit_cycle_cells),
            typecheck_seen_cells: take(&mut body._jit_typecheck_seen_cells),
            direct_callee_roots: take(&mut body._direct_callee_roots),
            deopt_point_boxes: take(&mut body._deopt_point_boxes),
            forced_chain_deopt_counters: take(&mut body._forced_chain_deopt_counters),
            poll_stress_cells: take(&mut body._poll_stress_cells),
            class_init_cells: take(&mut body._jit_class_init_cells),
            not_entrant: body.not_entrant.take(),
        }
    }
}

/// SYNC22-1 (JIT round 22, lane sync; `CRATONVM_JIT_RESCUED_TABLES_REWITHDRAW`,
/// default ON): the queue releases a rescued body's side tables only on a
/// proof about ITS buffer, but between the body's drop
/// ([`CompiledMethod::withdraw_inline_cache_owners`]) and that proof a thread
/// still inside the rescued code can take an inline-cache miss and publish a
/// NEW owner into one of these slots or cells. Dropped as a plain field here,
/// that owner would be released as a cascade of this release -- the shape
/// CODECACHE21-1 removed. So the same withdrawal runs again, and whatever it
/// finds leaves through the queue under its own proof.
///
/// Lock order (checked by reading): a `SideTables` entry is dropped only
/// outside the retirement-queue lock -- `drain_deferred_jit_owners` drops its
/// `released` batch after the guard's scope ends, `enqueue_retired_batch`'s
/// quiescent arm drops the batch it never queued, and no other path removes an
/// entry. The slot / PIC-writer / cell locks taken here belong to THIS dead
/// body; a thread holding one of them is inside that body's IC miss path, with
/// the body's return address on its stack, so no proof releases these tables
/// meanwhile. `CompiledMethod::drop` already runs the same withdrawal on a
/// drain's release path, so this adds no new lock pattern.
impl Drop for RescuedSideTables {
    fn drop(&mut self) {
        if self.mic_slots.is_empty() && self.pic_slots.is_empty() && self.cycle_cells.is_empty() {
            return;
        }
        let owners = withdraw_ic_owners_from(&self.mic_slots, &self.pic_slots, &self.cycle_cells);
        if !owners.is_empty() {
            release_withdrawn_ic_owners(owners);
        }
    }
}



/// Withdraw every owner these inline caches and cells hold: each slot's word
/// zeroed under its writer lock, then the owner taken (the body of
/// [`CompiledMethod::withdraw_inline_cache_owners`], shared with the `Drop` of
/// [`RescuedSideTables`]). The caller releases the result through
/// [`release_withdrawn_ic_owners`].
pub(crate) fn withdraw_ic_owners_from(
    mic_slots: &[Box<JitMICSlot>],
    pic_slots: &[Box<JitPICSlot>],
    cycle_cells: &[Arc<JitCycleEdgeCell>],
) -> Vec<Arc<CompiledMethod>> {
    use std::sync::atomic::Ordering;
    let mut owners: Vec<Arc<CompiledMethod>> = Vec::new();
    for slot in mic_slots {
        // JIT round 22, lane sync (review): under the slot's WRITER lock,
        // as every other writer of the word/owner pair takes it
        // (`JitMICSlot::compiled_owner`'s doc), and as the PIC and cycle
        // arms below do. This used `get_mut()`, which `&mut self` permits but
        // which is exactly the case it is wrong in: a RESCUED body may still be
        // running, and a thread inside it can take a MIC miss and publish into
        // this very slot (word, then owner, under the lock) concurrently -- an
        // unlocked `take()` then races that write on the non-atomic
        // `Option<Arc<_>>` (a lost or a doubly-released owner). Same answer
        // when nothing races: one uncontended lock per slot, at drop.
        let mut owner = slot.compiled_owner.lock();
        slot.cached_entry_word.store(0, Ordering::SeqCst);
        if let Some(previous) = owner.take() {
            owners.push(previous);
        }
    }
    for slot in pic_slots {
        slot.withdraw_owners_for_drop(&mut owners);
    }
    for cell in cycle_cells {
        // Review rev-x64 item 6 (wave 5): withdrawn whatever its strong
        // count (the first version tested `strong_count == 1` before the
        // lock, a check a registry upgrade could invalidate). Sound for
        // a cell another thread holds too: `entry_word` is read only by
        // the owning body's code, every writer takes `owner`'s lock, so a
        // concurrent install either precedes this (and its owner is taken
        // here) or follows it (word and owner set together, and the
        // owner then leaves through `JitCycleEdgeCell`'s own `Drop`,
        // through the queue); a concurrent `clear` finds nothing. A zero
        // word is a miss: the site dispatches.
        let mut owner = cell.owner.lock();
        cell.entry_word.store(0, Ordering::SeqCst);
        if let Some(previous) = owner.take() {
            owners.push(previous);
        }
    }
    owners
}



/// `CRATONVM_JIT_RETAINED_HOLDER_COUNT` (JIT round 21 wave 4, lane codecache,
/// CODECACHE21-2; default ON, `0|false|off|no` restores the `strong_count > 1`
/// shortcut): see [`CompiledMethod::retained_holders`]. Read once per artifact,
/// at construction.
pub(crate) fn retained_holder_count_enabled() -> bool {
    true
}



impl CompiledMethod {
    /// CODECACHE21-1 (JIT round 21 wave 4, lane codecache): withdraw every
    /// owner this dying body's inline caches and cells hold -- the slot's
    /// word zeroed FIRST, so no thread still inside the body (a rescued one)
    /// can reach the callee through it again, then the owner taken -- and
    /// release them through [`release_withdrawn_ic_owners`].
    ///
    /// These owners are bare `Arc`s. Dropped with the fields, a callee whose
    /// last owner one of them was is released as a CASCADE of this body's
    /// release: under per-thread evidence about THIS buffer only, so round 14
    /// re-queued its mapping and this wave its side tables, but its code-range
    /// registration and compile id went at once
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    /// Routed here, such a callee stays a whole, registered artifact in the
    /// queue until a proof about its own buffer.
    pub(crate) fn withdraw_inline_cache_owners(&mut self) {
        if self._jit_mic_slots.is_empty()
            && self._jit_pic_slots.is_empty()
            && self._jit_cycle_cells.is_empty()
        {
            return;
        }
        // The withdrawal itself (MIC slots under their writer lock since JIT
        // round 22 lane sync, PIC slots, cycle cells) is shared with the
        // `Drop` of `RescuedSideTables` (SYNC22-1).
        let owners = withdraw_ic_owners_from(
            &self._jit_mic_slots,
            &self._jit_pic_slots,
            &self._jit_cycle_cells,
        );
        release_withdrawn_ic_owners(owners);
    }
}

/// Release owners a dying body's inline caches and cells held, every one of
/// them already withdrawn from its slot (the slot's word zeroed first): ALL of
/// them to the retirement queue, in ONE batch, so each is released on a proof
/// about its OWN buffer and never as a cascade of this body's release.
///
/// Review rev-x64 item 5 (wave 5): the first version dropped an owner plainly
/// when `retained_holders > 0`. That read and the drop were two steps: a
/// `RetainedCode` dropping in between took the count to zero and queued its
/// own `Arc`, and if the queue released that one first, the plain drop here
/// was the final reference -- the cascade this exists to remove. The queue
/// drops a non-last `Arc` as a plain decrement, so queueing all of them costs
/// one batch trip per dying body and closes the window.
pub(crate) fn release_withdrawn_ic_owners(owners: Vec<Arc<CompiledMethod>>) {
    inline_cache_pic::defer_jit_owners(owners);
}

pub(crate) static DEFERRED_JIT_OWNERS: std::sync::OnceLock<parking_lot::Mutex<Vec<DeferredOwner>>> =
    std::sync::OnceLock::new();

/// Code-cache install, reclamation and refused-allocation accounting, read by
/// [`jit_code_reclamation_stats`] and [`jit_code_alloc_failure_stats`].
/// Relaxed counters: observability, not a transaction.
///
/// The three `alloc_failure_*` fields at the bottom are a separate ledger that
/// shares this struct rather than owning a static of its own; see the note
/// above [`note_jit_code_alloc_failure`] for why, and note that
/// [`jit_code_reclamation_stats`] deliberately does NOT carry them — widening
/// its public struct would change a shape the VM reads field by field, and the
/// two are read by different call sites for different questions.
///
/// **`typecheck_sites` is a THIRD guest ledger and has nothing to do with the
/// code cache**, which is worth saying plainly rather than leaving a reader to
/// wonder. It is here for exactly the reason the `alloc_failure_*` fields are:
/// `jit/tests/process_global_statics_ratchet.rs` freezes the number of `static`
/// declaration lines under `jit/src` and asserts EQUALITY, so a new census
/// static is a red build and the correction would have to be made in a file the
/// lane adding the census does not own. The `instanceof` site census asked for
/// by
/// `docs/feature-designs/jit-ir-optimizations.md`
/// is five counters; that page names this exact remedy ("hang the three
/// counters off an existing accounting struct the way
/// `note_jit_code_alloc_failure`'s counters were hung off `RECLAMATION` for
/// exactly this reason"). The cost is that this struct's NAME is now narrower
/// than its contents; the alternative was five statics or an unfed counter, and
/// both are worse.
pub(crate) struct ReclamationCounters {
    pub(crate) installed_bodies: std::sync::atomic::AtomicU64,
    pub(crate) installed_bytes: std::sync::atomic::AtomicU64,
    pub(crate) installed_code_bytes: std::sync::atomic::AtomicU64,
    pub(crate) withdrawn_bodies: std::sync::atomic::AtomicU64,
    pub(crate) withdrawn_bytes: std::sync::atomic::AtomicU64,
    /// Withdrawals per [`retire_reason`] code. Sums to `withdrawn_bodies`
    /// exactly, because [`retire_withdrawn_body`] is the only site that bumps
    /// either and it bumps both or neither.
    pub(crate) withdrawn_by_reason: [std::sync::atomic::AtomicU64; retire_reason::COUNT],
    /// High-water mark of `installed_bytes - reclaimed_bytes`, sampled at each
    /// publish. See [`note_jit_published_live_bytes_peak`] for the sampling
    /// rule and for why the sample is biased LOW rather than high.
    pub(crate) peak_live_bytes: std::sync::atomic::AtomicU64,
    pub(crate) queued_owners: std::sync::atomic::AtomicU64,
    pub(crate) queued_bytes: std::sync::atomic::AtomicU64,
    pub(crate) reclaimed_bodies: std::sync::atomic::AtomicU64,
    pub(crate) reclaimed_bytes: std::sync::atomic::AtomicU64,
    pub(crate) reclaimed_code_bytes: std::sync::atomic::AtomicU64,
    pub(crate) drains: std::sync::atomic::AtomicU64,
    pub(crate) drains_quiescent: std::sync::atomic::AtomicU64,
    pub(crate) drains_by_thread_evidence: std::sync::atomic::AtomicU64,
    pub(crate) drains_deferred: std::sync::atomic::AtomicU64,
    /// Consecutive drains that released nothing; reset by any release. Paces
    /// the depth-0 pump ([`pump_drain_paced`]).
    pub(crate) futile_drain_streak: std::sync::atomic::AtomicU64,
    /// Pump ticks skipped since the last paced drain.
    pub(crate) pump_ticks_skipped: std::sync::atomic::AtomicU64,
    /// JIT round 22 wave 11, lane runtime (reporting only): per-body verdicts of
    /// the per-thread proof that KEPT a body. See
    /// [`JitCodeReclamationStats::kept_by_running_thread`].
    pub(crate) kept_by_running_thread: std::sync::atomic::AtomicU64,
    pub(crate) kept_by_blocked_band: std::sync::atomic::AtomicU64,
    pub(crate) drains_untracked: std::sync::atomic::AtomicU64,
    /// Refused code-cache allocations, all reasons. See
    /// [`note_jit_code_alloc_failure`].
    pub(crate) alloc_failures: std::sync::atomic::AtomicU64,
    /// Bytes those refusals asked for; a lower bound (the cap gate refuses
    /// before a size is computed and contributes zero).
    pub(crate) alloc_failure_bytes: std::sync::atomic::AtomicU64,
    /// Refusals per [`code_alloc_failure`] code.
    pub(crate) alloc_failures_by_reason: [std::sync::atomic::AtomicU64; code_alloc_failure::COUNT],
    /// The `instanceof` site census, both tiers. A guest ledger; see this
    /// struct's doc comment for why it is a field here and not a static.
    pub(crate) typecheck_sites: TypecheckSiteCensus,
    /// The per-method publication census — a FOURTH guest ledger, here for the
    /// same reason as the others (see this struct's doc comment: a new
    /// `static` under `jit/src` is a red `process_global_statics_ratchet`).
    ///
    /// `OnceLock` rather than a `Mutex` built in [`Self::new`] because that
    /// constructor is `const` and `FxHashMap::default()` is not.
    pub(crate) compilation_census: std::sync::OnceLock<parking_lot::Mutex<CompilationCensusState>>,
    /// Lowest start and highest end of every executable buffer ever registered
    /// (round 14 wave 2, lane codecache): an address outside `[mapped_lo,
    /// mapped_hi)` was never JIT code, live or freed. A field here, not a
    /// `static`, for the ratchet reason above.
    pub(crate) mapped_lo: std::sync::atomic::AtomicUsize,
    pub(crate) mapped_hi: std::sync::atomic::AtomicUsize,
    /// Round 14 wave 3 (lane fixup, RV3-2): the CODE grace, the highest
    /// retirement generation after which every running thread has returned to
    /// JIT depth 0 (or every thread was outside compiled code). Unlike
    /// [`JIT_GRACED_GENERATION`] it is never advanced on a miss-handler stamp
    /// (M8-1) or at a cooperative stop: a thread there is still inside
    /// compiled bodies. Never above `JIT_GRACED_GENERATION`. Read by
    /// [`reserve_compile_id`]; a field here, not a `static`, for the ratchet
    /// reason above.
    pub(crate) code_graced_generation: std::sync::atomic::AtomicU64,
}

/// Publications per method, for the process report's compilation group.
///
/// # Why a table and not three counters
///
/// The three figures this feeds — `methods_compiled`, `recompilations` and
/// `max_versions_for_one_method` — are all functions of one quantity, "how many
/// bodies has this method had", and only that quantity can answer the third.
/// Deriving all three from the map at read time rather than maintaining three
/// atomics beside it means there is one source of truth and no drift to pin:
/// `methods_compiled` is `versions.len()`, `recompilations` is
/// `sum(v) - len()`, and the max is `values().max()`.
///
/// # What a "version" is
///
/// One PUBLICATION of a body for this method key, counted whether or not it
/// displaced a live one. That is the model's definition —
/// `CodeCacheLifecycle::install` bumps `recompilations` when its own per-method
/// count comes back above 1, and its table never forgets either — and it is
/// deliberately **not** the same population as
/// `withdrawn_by_reason[retire_reason::SUPERSEDED]`, which counts only the
/// publications that displaced a body still in the map. A method whose body was
/// withdrawn by a class unload and then recompiled is a recompilation here and
/// is not a supersede there. Both are true; they answer different questions, and
/// the process report's field is named for this one.
///
/// A method-entry body and an OSR body of the same method are two versions of
/// it. `installed_bodies` counts both, and `versions_per_method` is the ratio of
/// that figure to this one, so counting them separately here would make the
/// ratio mean nothing.
///
/// # The bound, and why exceeding it suppresses rather than truncates
///
/// One `(u64, u32)` entry per distinct method ever compiled in this process.
/// That is small against the executable memory each of those methods already
/// costs, and the insert is trivial against the full shard-map clone
/// [`JitCache::put`] performs on the same publication under the same lock — so
/// the census is unconditional rather than gated on the flag that prints it.
///
/// It is still unbounded in principle, so it is bounded in practice at
/// [`CENSUS_METHOD_LIMIT`]. Past that point a NEW method is not inserted and
/// `truncated` latches, and [`jit_compilation_census`] then answers `None` —
/// the group stays suppressed and the report prints `n/a` with the reason.
/// Truncating silently would have been the worse failure: `methods_compiled`
/// pinned at the limit while `installs` kept climbing drives
/// `versions_per_method` steadily upward, which reads exactly like the
/// compilation thrash the figure exists to detect.
pub(crate) struct CompilationCensusState {
    /// [`compute_jit_key_hash`] of the method, to its publication count.
    ///
    /// Keyed by the hash rather than by the `JitKey` so an entry costs twelve
    /// bytes instead of three `Arc<str>`s. A collision merges two methods'
    /// counts in a diagnostic; at this table's size that is a far smaller error
    /// than the one the retained strings would cost in memory.
    pub(crate) versions: FxHashMap<u64, u32>,
    /// A publication was dropped from the table because it was full. Latches;
    /// once set the census is no longer exact and stops being reported.
    pub(crate) truncated: bool,
}

/// The most distinct methods [`CompilationCensusState`] will track.
///
/// 2^17 methods is roughly 2 MiB of table. Chosen well above what a large
/// application compiles (tens of thousands of distinct methods) and far below
/// anything that would matter beside a code cache measured in tens of
/// megabytes.
pub(crate) const CENSUS_METHOD_LIMIT: usize = 1 << 17;

/// Compile-time site census for BOTH `instanceof` arms: the single-pass one
/// (`jit/src/x64/op_object.rs`, opcode `0xc1`) and the optimizing tier's
/// (`jit/src/ir_lower.rs`, `Op::InstanceOf`).
///
/// One struct rather than nine loose counters so it can be a single FIELD on
/// [`ReclamationCounters`] — see that struct's doc comment for why a census
/// under `jit/src` cannot be its own `static`.
///
/// The two tiers are counted APART, never summed, for the reason
/// `CHECKCAST_INLINE_SITES_IR`'s doc gives about its own pair: they are two
/// independent emitters and a hot method only ever runs the second one, so a
/// combined figure that moves cannot say which door a perf claim is about.
///
/// The five buckets are the `checkcast` arm's four split-by-cause counters
/// (`CHECKCAST_INLINE_SITES_PRIM_ARRAY`, `CHECKCAST_INLINE_SITES`,
/// `CHECKCAST_INLINE_NO_TARGET_ID`, `CHECKCAST_INLINE_UNTRUSTED`) applied to
/// the twin arm a hundred lines below it, plus one the `checkcast` arm has no
/// analogue for. They are counted at EMISSION, not in the predicate, which is
/// review #80's rule and the one the `checkcast` counters already follow: a
/// code-buffer retry that re-emits a method counts its sites again, so these
/// are upper bounds on distinct source sites, to be compared against zero and
/// against each other and never divided.
pub(crate) struct TypecheckSiteCensus {
    /// `instanceof` sites that got the 1-D primitive-array KIND_TAGS compare.
    pub(crate) instanceof_prim_array: std::sync::atomic::AtomicU64,
    /// `instanceof` sites that got the inline class-id compare.
    pub(crate) instanceof_class_id: std::sync::atomic::AtomicU64,
    /// Of [`Self::instanceof_class_id`], those that ALSO got the definite-miss
    /// inline `0` for a `final` target.
    ///
    /// A SUBSET, not a disjoint bucket, and the one counter with no `checkcast`
    /// analogue — because the substitution it measures has no `checkcast`
    /// analogue either. `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` is **default ON**
    /// (`instanceof_final_miss_enabled` uses `runtime_flag_default_on`) and
    /// whether it engages is decided by `instanceof_miss_is_final_boot_class`'s
    /// hard-coded nine-name list or by the compile door having interned the
    /// target as a proven-`final` user class. A performance feature that is on
    /// by default, whose engagement is decided by a name-matching predicate,
    /// and whose engagement nothing records, is one that cannot be shown to
    /// have ever run — which is the argument
    /// `r10-readers-instanceof-arm-has-no-site-census-FIXED-20260922.md` makes for the
    /// whole census and makes strongest here.
    pub(crate) instanceof_final_miss: std::sync::atomic::AtomicU64,
    /// Refused: the target class was not resolvable at compile time.
    pub(crate) instanceof_no_target_id: std::sync::atomic::AtomicU64,
    /// Refused: the operand was not a provably-trusted oop.
    pub(crate) instanceof_untrusted: std::sync::atomic::AtomicU64,

    // ---- the OPTIMIZING tier's own `instanceof` arm ------------------------
    //
    // `ir_lower.rs`'s `Op::InstanceOf` lowering (round 9 wave 9, `irl9`), which
    // is to the five buckets above what `CHECKCAST_INLINE_SITES_IR` is to
    // `CHECKCAST_INLINE_SITES`: a second, independent emitter that a HOT method
    // is the only one to run. Closes
    // `docs/feature-designs/jit-ir-optimizations.md`.
    //
    // FOUR buckets, not five, and that is the substance of this half rather
    // than an omission. The page that asked for it said outright "do not assume
    // the five buckets transfer", and one does not: the single-pass arm's
    // `refused-untrusted-operand` is fed by `operand_is_trusted_oop`, a test
    // over that backend's `stack_oop_marks` abstract-stack marks. The
    // optimizing tier has no such test and cannot have one — its operand is an
    // SSA value whose reference-ness is a property of the IR, so there is no
    // population for that bucket to count. A bucket with that NAME fed by a
    // different condition is worse than a missing bucket; see the `const _`
    // numbering assertions in `vm/src/jit/code_cache_lifecycle.rs` for the same
    // hazard one level down.
    /// Optimizing-tier `instanceof` sites that got the 1-D primitive-array
    /// KIND_TAGS compare.
    ///
    /// **Reads zero on every production run today, for a reason outside this
    /// census.** Measured 2026-09-22: `lib.rs` admits a site to
    /// `instanceof_info` only when `cp_new_resolver` answers
    /// `JitNewSite::Resolved`, and an ARRAY-typed `CONSTANT_Class` never does,
    /// so `ir.rs`'s `0xc1` arm takes its miss branch, `plant_uncommon_trap`
    /// refuses, and the WHOLE method bails to single-pass —
    /// `[ir] IrBuilder::build refused at ir.rs:11132` for both
    /// `instanceof int[]` and `instanceof String[]`, on a probe whose
    /// `instanceof String` method compiles IR and moves
    /// [`Self::instanceof_ir_class_id`] in the same run. The emitting arm is
    /// real and `jit/tests/r10_producers_ir_instanceof_site_census.rs` step 3
    /// exercises it; nothing in production reaches it. Kept rather than
    /// deleted, because fixing that refusal lights this up unchanged. See
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    pub(crate) instanceof_ir_prim_array: std::sync::atomic::AtomicU64,
    /// Optimizing-tier `instanceof` sites that got the inline class-id compare.
    pub(crate) instanceof_ir_class_id: std::sync::atomic::AtomicU64,
    /// Of [`Self::instanceof_ir_class_id`], those that ALSO got the
    /// definite-miss inline `0` for a `final` target. A SUBSET, exactly as
    /// [`Self::instanceof_final_miss`] is of [`Self::instanceof_class_id`].
    ///
    /// The optimizing tier's own flag read is `ir_instanceof_final_miss_enabled`
    /// over the same `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` key, so this is the
    /// engagement number for that default-ON substitution **on the door a hot
    /// method actually reaches** — the single-pass bucket measures the door it
    /// was superseded at.
    pub(crate) instanceof_ir_final_miss: std::sync::atomic::AtomicU64,
    /// Optimizing-tier `instanceof` sites that fell through to `jit_instanceof`
    /// with no inline arm at all: neither a primitive-array KIND_TAGS byte nor a
    /// usable target class id was available.
    ///
    /// The DENOMINATOR for the three buckets above — without it a zero on them
    /// cannot be told from a workload with no `instanceof` in it.
    ///
    /// **Its reachable population is not the one the name first suggests, and
    /// that is the substantive difference between the two doors.** Both arms
    /// derive the target as `typecheck_target_for_site(..).filter(|&id| id != 0)`,
    /// so the refusal has two sub-causes: the site had no id at all, or its id
    /// is `0`. On the single-pass door both fire. On THIS door only the second
    /// can: an `instanceof` whose target is not loaded at compile time is
    /// refused upstream by `instanceof_info`'s admission gate (the
    /// not-yet-loaded resolution path can run a user classloader, which this
    /// tier will not host inside a helper call), so it never becomes an
    /// `Op::InstanceOf` node — the IR builder's `0xc1` arm plants an
    /// `UnresolvedTypeCheck` uncommon trap and the body goes to single-pass.
    ///
    /// The second sub-cause is a live population rather than a tripwire:
    /// `ClassStore::next_id` is `self.classes.len()`, so the FIRST class the VM
    /// loads owns id `0` and every `x instanceof <that class>` lands here.
    /// `jit/tests/r10_producers_ir_instanceof_site_census.rs` pins both halves —
    /// that an id-`0` target reaches this bucket, and that an unresolved one
    /// does not reach this tier at all.
    pub(crate) instanceof_ir_no_target_id: std::sync::atomic::AtomicU64,
}

impl TypecheckSiteCensus {
    pub(crate) const fn new() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            instanceof_prim_array: AtomicU64::new(0),
            instanceof_class_id: AtomicU64::new(0),
            instanceof_final_miss: AtomicU64::new(0),
            instanceof_no_target_id: AtomicU64::new(0),
            instanceof_untrusted: AtomicU64::new(0),
            instanceof_ir_prim_array: AtomicU64::new(0),
            instanceof_ir_class_id: AtomicU64::new(0),
            instanceof_ir_final_miss: AtomicU64::new(0),
            instanceof_ir_no_target_id: AtomicU64::new(0),
        }
    }
}

impl ReclamationCounters {
    pub(crate) const fn new() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            installed_bodies: AtomicU64::new(0),
            installed_bytes: AtomicU64::new(0),
            installed_code_bytes: AtomicU64::new(0),
            withdrawn_bodies: AtomicU64::new(0),
            withdrawn_bytes: AtomicU64::new(0),
            // Inline `const` block for the same reason
            // `alloc_failures_by_reason` uses one: `AtomicU64` is not `Copy`,
            // so `[AtomicU64::new(0); N]` does not compile.
            withdrawn_by_reason: [const { AtomicU64::new(0) }; retire_reason::COUNT],
            peak_live_bytes: AtomicU64::new(0),
            queued_owners: AtomicU64::new(0),
            queued_bytes: AtomicU64::new(0),
            reclaimed_bodies: AtomicU64::new(0),
            reclaimed_bytes: AtomicU64::new(0),
            reclaimed_code_bytes: AtomicU64::new(0),
            drains: AtomicU64::new(0),
            drains_quiescent: AtomicU64::new(0),
            drains_by_thread_evidence: AtomicU64::new(0),
            drains_deferred: AtomicU64::new(0),
            futile_drain_streak: AtomicU64::new(0),
            pump_ticks_skipped: AtomicU64::new(0),
            kept_by_running_thread: AtomicU64::new(0),
            kept_by_blocked_band: AtomicU64::new(0),
            drains_untracked: AtomicU64::new(0),
            alloc_failures: AtomicU64::new(0),
            alloc_failure_bytes: AtomicU64::new(0),
            // Inline `const` block, not `[AtomicU64::new(0); N]`: `AtomicU64`
            // is not `Copy`, so the array-repeat form does not compile. Same
            // shape as `CodeCacheLifecycle::new`'s histogram on the VM side.
            alloc_failures_by_reason: [const { AtomicU64::new(0) }; code_alloc_failure::COUNT],
            typecheck_sites: TypecheckSiteCensus::new(),
            compilation_census: std::sync::OnceLock::new(),
            mapped_lo: std::sync::atomic::AtomicUsize::new(usize::MAX),
            mapped_hi: std::sync::atomic::AtomicUsize::new(0),
            code_graced_generation: AtomicU64::new(0),
        }
    }
}

pub(crate) static RECLAMATION: ReclamationCounters = ReclamationCounters::new();

/// One thread's part in the quiescence proof code reclamation needs.
///
/// The process-wide `ACTIVE_JIT_EXECUTIONS` count can prove "no thread is in
/// compiled code", and nothing else. A thread that never returns from compiled
/// code — a pool worker parked in `LinkedBlockingQueue.take()` — keeps it above
/// zero forever, and before these records existed no retired body was released
/// until the code-cache cap refused every compile. Per-thread evidence lets the
/// drain decide body by body instead. See [`drain_deferred_jit_owners`].
pub(crate) struct JitThreadQuiescence {
    /// JIT executions this thread holds. Written only by its own thread.
    pub(crate) depth: std::sync::atomic::AtomicUsize,
    /// The retirement generation read the last time this thread returned to
    /// depth 0, read with its depth already 0. Written only by its own thread.
    pub(crate) quiescent_gen: std::sync::atomic::AtomicU64,
    /// The retirement generation read the last time this thread entered a
    /// Rust inline-cache miss handler ([`jit_ic_quiescent_point`]): past the
    /// probe that missed and before any probe the handler makes, so outside
    /// every inline-cache window, though still inside compiled code. Evidence
    /// for the INLINE-CACHE grace only, never for code release (round 14
    /// wave 2, lane mic, proposal M8-1). Written only by its own thread.
    pub(crate) ic_quiescent_gen: std::sync::atomic::AtomicU64,
    /// Nesting of blocked windows ([`jit_thread_blocked_enter`]). Written only
    /// by its own thread.
    pub(crate) blocked_nesting: std::sync::atomic::AtomicU32,
    /// What this thread's stack can return into while it is blocked.
    pub(crate) blocked: parking_lot::Mutex<BlockedStackSummary>,
}

#[derive(Default)]
pub(crate) struct BlockedStackSummary {
    /// The scan completed, the thread is still inside the blocked window the
    /// scan was taken at, and it has run no compiled code since. Cleared under
    /// the lock before the thread can run compiled code again.
    pub(crate) valid: bool,
    /// Sorted, deduplicated starts of every executable buffer a word of the
    /// scanned stack band points into.
    pub(crate) buffers: Vec<usize>,
    /// nio21 wave 17 L3: the band the lazy summary has not scanned yet. While
    /// `Some`, `buffers` is meaningless and a reader resolves it first
    /// ([`BlockedStackSummary::resolve_pending`]). `None` under the eager
    /// summary (`CRATONVM_JIT_LAZY_BLOCKED_SUMMARY=0`).
    pub(crate) pending: Option<PendingBlockedBand>,
}

/// Every thread that has ever entered compiled code and has not exited.
pub(crate) static JIT_THREADS: std::sync::OnceLock<parking_lot::Mutex<Vec<Arc<JitThreadQuiescence>>>> =
    std::sync::OnceLock::new();

pub(crate) fn jit_threads() -> &'static parking_lot::Mutex<Vec<Arc<JitThreadQuiescence>>> {
    JIT_THREADS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

/// JIT executions entered on a thread whose quiescence record is already gone
/// (thread teardown). While non-zero the per-thread evidence is incomplete, so
/// only the process-wide fast path may release bodies.
pub(crate) static UNTRACKED_JIT_EXECUTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Returns to depth 0 between two drain attempts one thread makes while the
/// retirement queue is non-empty. Per-thread evidence walks every registered
/// thread, so it must not ride every return.
pub(crate) const DRAIN_PUMP_INTERVAL: u32 = 32;

/// Returns to depth 0 between two grace-only proofs one thread makes while a
/// retired inline-cache way waits for its grace and nothing is queued (round
/// 13 wave 2, lane mega). Sparser than [`DRAIN_PUMP_INTERVAL`]: that state can
/// last as long as some thread spins inside compiled code.
pub(crate) const GRACE_PUMP_INTERVAL: u32 = 1024;

/// Grace-lag install refusals between two inline-cache grace catch-up proofs
/// one thread makes ([`jit_ic_grace_catch_up`], round 14 wave 2, lane mic).
/// The first refusal proves at once; a site whose receivers keep being refused
/// (a peer spins in pure machine code, so the proof cannot succeed) pays the
/// O(registered threads) walk once per this many helper calls.
pub(crate) const IC_GRACE_CATCH_UP_INTERVAL: u32 = 64;

/// The calling thread's entry in [`JIT_THREADS`], removed when the thread exits.
pub(crate) struct ThreadQuiescenceHandle {
    pub(crate) record: Arc<JitThreadQuiescence>,
    pub(crate) drain_countdown: std::cell::Cell<u32>,
    /// `CRATONVM_JIT_IC_GRACE_THREAD_EVIDENCE`, read once per thread: `0`
    /// unread, `1` off, `2` on. Per thread rather than a process static (the
    /// statics ratchet), and read only while a grace lags.
    pub(crate) grace_evidence_flag: std::cell::Cell<u8>,
    /// `CRATONVM_JIT_IC_QUIESCENT_STAMP`, read once per thread: `0` unread,
    /// `1` off, `2` on (round 14 wave 2, lane mic, M8-1).
    pub(crate) ic_stamp_flag: std::cell::Cell<u8>,
    /// Grace-lag refusals left before this thread's next
    /// [`jit_ic_grace_catch_up`] proof (round 14 wave 2, lane mic).
    pub(crate) ic_catch_up_countdown: std::cell::Cell<u32>,
    /// `CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`, read once per thread: `0`
    /// unread, `1` off, `2` on (round 14 wave 7, lane mega).
    pub(crate) ic_catch_up_self_stamp_flag: std::cell::Cell<u8>,
    /// `CRATONVM_JIT_LAZY_BLOCKED_SUMMARY`, read once per thread: `0` unread,
    /// `1` off, `2` on (nio21 wave 17 L3; a field, not a static, for the
    /// statics ratchet).
    pub(crate) lazy_blocked_summary_flag: std::cell::Cell<u8>,
}

impl ThreadQuiescenceHandle {
    /// nio21 wave 17 L3: does [`jit_thread_blocked_enter`] defer its band
    /// scan to the drain (`CRATONVM_JIT_LAZY_BLOCKED_SUMMARY`, default on;
    /// `0|false|off|no` scans at every transition, as before)? Read once per
    /// thread.
    pub(crate) fn lazy_blocked_summary(&self) -> bool {
        match self.lazy_blocked_summary_flag.get() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                self.lazy_blocked_summary_flag.set(if on { 2 } else { 1 });
                on
            }
        }
    }

    pub(crate) fn register() -> Self {
        let record = Arc::new(JitThreadQuiescence {
            depth: std::sync::atomic::AtomicUsize::new(0),
            quiescent_gen: std::sync::atomic::AtomicU64::new(0),
            ic_quiescent_gen: std::sync::atomic::AtomicU64::new(0),
            blocked_nesting: std::sync::atomic::AtomicU32::new(0),
            blocked: parking_lot::Mutex::new(BlockedStackSummary::default()),
        });
        lock_jit_threads().push(Arc::clone(&record));
        THREAD_QUIESCENCE_REGISTERED.with(|registered| registered.set(true));
        Self {
            record,
            drain_countdown: std::cell::Cell::new(0),
            grace_evidence_flag: std::cell::Cell::new(0),
            ic_stamp_flag: std::cell::Cell::new(0),
            ic_catch_up_countdown: std::cell::Cell::new(0),
            ic_catch_up_self_stamp_flag: std::cell::Cell::new(0),
            lazy_blocked_summary_flag: std::cell::Cell::new(0),
        }
    }

    /// Round 14 wave 7 (lane mega): does [`jit_ic_grace_catch_up`] re-stamp
    /// the refusing thread's own `ic_quiescent_gen` before it collects the
    /// evidence (`CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`, default on)? Read once
    /// per thread, like [`Self::ic_quiescent_stamp`].
    pub(crate) fn ic_catch_up_self_stamp(&self) -> bool {
        match self.ic_catch_up_self_stamp_flag.get() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                self.ic_catch_up_self_stamp_flag.set(if on { 2 } else { 1 });
                on
            }
        }
    }

    /// Round 14 wave 2 (lane mic, proposal M8-1): may this thread stamp
    /// `ic_quiescent_gen` at the miss handlers' entry and run the inline-cache
    /// grace catch-up (`CRATONVM_JIT_IC_QUIESCENT_STAMP`, default on)? Read
    /// once per thread, like [`Self::grace_by_thread_evidence`]. Off, the stamp
    /// stays `0`, so `min_running_ic_gen` equals `min_running_gen` exactly and
    /// the catch-up never runs: the pre-M8-1 behaviour.
    pub(crate) fn ic_quiescent_stamp(&self) -> bool {
        match self.ic_stamp_flag.get() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                self.ic_stamp_flag.set(if on { 2 } else { 1 });
                on
            }
        }
    }

    /// Round 13 wave 2 (lane mega): may this thread's depth-0 returns pump the
    /// per-thread grace proof while nothing is queued
    /// (`CRATONVM_JIT_IC_GRACE_THREAD_EVIDENCE`, default on)? See
    /// [`jit_execution_leave`].
    pub(crate) fn grace_by_thread_evidence(&self) -> bool {
        match self.grace_evidence_flag.get() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                self.grace_evidence_flag.set(if on { 2 } else { 1 });
                on
            }
        }
    }
}

impl Drop for ThreadQuiescenceHandle {
    fn drop(&mut self) {
        // Thread exit: its thread function has returned, so no compiled frame
        // of it remains anywhere.
        self.record
            .depth
            .store(0, std::sync::atomic::Ordering::Release);
        self.record.blocked.lock().valid = false;
        jit_threads()
            .lock()
            .retain(|thread| !Arc::ptr_eq(thread, &self.record));
    }
}

thread_local! {
    pub(crate) static THREAD_QUIESCENCE: ThreadQuiescenceHandle = ThreadQuiescenceHandle::register();
    /// Whether this thread ever registered a quiescence record. Lets a blocked
    /// transition on a thread that never entered compiled code — most parks in
    /// the process — skip registering one.
    pub(crate) static THREAD_QUIESCENCE_REGISTERED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Retirement generation: advanced AFTER every withdrawal of a raw target from
/// a place generated code reads without a lock (an inline-cache way), and for
/// every owner handed to the retirement queue.
///
/// A withdrawal stamped `g` is graced — its way may be reused for a different
/// receiver — once `g <= JIT_GRACED_GENERATION`, i.e. once every thread has
/// provably been outside compiled code at some instant after the stamp, and so
/// cannot still be between that way's class compare and its entry load. Since
/// round 14 wave 2 (lane mic, M8-1) "outside compiled code" includes the entry
/// of a Rust miss handler ([`jit_ic_quiescent_point`]): outside every probe,
/// which is all this grace needs; no probe spans a call.
pub(crate) static JIT_RETIRE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Highest retirement generation that has been graced. See
/// [`JIT_RETIRE_GENERATION`].
pub(crate) static JIT_GRACED_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Advance the retirement generation and return the stamp for a withdrawal
/// whose writes have just completed.
#[inline]
pub(crate) fn bump_retire_generation() -> u64 {
    JIT_RETIRE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
}

/// Whether a withdrawal stamped `generation` has been graced.
#[inline]
pub(crate) fn retire_generation_is_graced(generation: u64) -> bool {
    generation <= JIT_GRACED_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Record that every thread was outside compiled code at some instant after the
/// retirement generation read as `observed`. The caller must read `observed`
/// BEFORE the quiescence observation it reports, so the observation postdates
/// every withdrawal stamped at or below it.
#[inline]
pub(crate) fn note_graced_generation(observed: u64) {
    JIT_GRACED_GENERATION.fetch_max(observed, std::sync::atomic::Ordering::AcqRel);
}

/// Round 14 wave 3 (lane fixup, RV3-2 of
/// `r14w3-review3-wave2-review-findings-FIXED-20260929.md`): record the CODE grace
/// (`ReclamationCounters::code_graced_generation`). Same reading rule as
/// [`note_graced_generation`]; `observed` must be bounded by
/// `min_running_gen` (depth-0 returns), never by the inline-cache-only
/// `min_running_ic_gen`. Every caller also notes the inline-cache grace with
/// a value at least this large, so the code grace never passes it.
#[inline]
pub(crate) fn note_code_graced_generation(observed: u64) {
    RECLAMATION
        .code_graced_generation
        .fetch_max(observed, std::sync::atomic::Ordering::AcqRel);
}

/// Whether a compile id released under `generation` may be reissued: every
/// running thread has returned to JIT depth 0 since (the code grace), not
/// merely left every inline-cache probe. `CRATONVM_JIT_COMPILE_ID_CODE_GRACE=0`
/// restores the wave-2 rule (the inline-cache grace). `code_grace` is that
/// switch, read by the caller once per reservation (once per compile), outside
/// the allocator lock.
pub(crate) fn compile_id_generation_is_graced(generation: u64, code_grace: bool) -> bool {
    if !code_grace {
        return retire_generation_is_graced(generation);
    }
    generation
        <= RECLAMATION
            .code_graced_generation
            .load(std::sync::atomic::Ordering::Acquire)
}

/// Round 13 wave 4 (lane mega3, `r13w2-mega-grace-at-cooperative-stop-patch`):
/// note that every mutator is stopped at a safepoint poll or inside a blocked
/// window, so none is between an inline-cache compare and its entry load.
/// The VM calls it on the stop's initiator, AFTER every counted mutator has
/// arrived cooperatively and BEFORE any is released, and only when the stop
/// froze nobody (a frozen peer may be anywhere). This is what graces a retired
/// way while some thread spins in a compiled loop that never returns to depth
/// 0. It advances only the INLINE-CACHE grace: compiled code is released by
/// the retirement queue's own proof, which this does not touch (a thread
/// parked at a poll is still inside its body).
/// `CRATONVM_JIT_IC_GRACE_AT_STOP=0` makes it a no-op. Read once per pause,
/// so no cached static is needed.
pub fn note_grace_at_cooperative_stop() {
    // Read while everyone is stopped: every withdrawal stamped at or below it
    // completed before an instant at which no thread was inside a probe.
    note_graced_generation(JIT_RETIRE_GENERATION.load(std::sync::atomic::Ordering::Acquire));
}

/// Round 14 wave 2 (lane mic, proposal M8-1 of `jit-r13-mega8-proposals-RETIRED-20260929.md`,
/// `r12w7-mega6-grace-starves-while-a-thread-stays-compiled`): record that the
/// calling thread stands outside every inline-cache window now.
///
/// The VM's Rust miss handlers (`jit_invoke_virtual_mic`,
/// `jit_invoke_dispatch`) call it FIRST, before any probe of their own: the
/// probe that sent the thread here has finished (a miss, or a hit on a zero
/// word), every compiled frame below is suspended at a CALL, and no probe
/// spans a call. The generation is read with `Acquire` at that instant, so any
/// probe the thread starts afterwards sees every withdrawal stamped at or
/// below it; the `Release` store orders every probe it finished before. That
/// is the same witness `quiescent_gen` gives at a depth-0 return, but for the
/// inline-cache grace ONLY: the thread is still inside compiled bodies, so
/// code release keeps using `quiescent_gen` alone (see
/// `ThreadQuiescenceEvidence::min_running_ic_gen`).
///
/// One TLS read, one load and one store. No-op on a thread that never
/// registered a quiescence record, or with
/// `CRATONVM_JIT_IC_QUIESCENT_STAMP=0`.
#[inline]
pub fn jit_ic_quiescent_point() {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
    {
        return;
    }
    let _ = THREAD_QUIESCENCE.try_with(|handle| {
        if handle.ic_quiescent_stamp() {
            handle.record.ic_quiescent_gen.store(
                JIT_RETIRE_GENERATION.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
    });
}

/// Round 14 wave 2 (lane mic, M8-1): an inline-cache install was just refused
/// only for a lagging grace ([`JitPICSlot::install`]); try to advance the
/// grace NOW on per-thread evidence, with each running thread vouching up to
/// the later of its depth-0 stamp and its miss-handler stamp
/// ([`jit_ic_quiescent_point`]). Returns whether the graced generation
/// advanced, i.e. whether the install is worth one retry.
///
/// This is the cheap proof that comes before the stop-based one: the
/// handshake request (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`,
/// `MegaDispatchTable::claim_grace_request`) is asked for only when the
/// install is still refused after it. It succeeds whenever every thread
/// inside compiled code has returned to depth 0, entered a miss handler, or
/// blocked since the way's retirement; a peer spinning in pure machine code
/// (no helper call) still bounds it, and is what the handshake is for.
///
/// Same rule as the drain and the depth-0 pump: `observed` is read before the
/// evidence. The caller must hold no PIC writer lock (lock order: nothing,
/// then [`JIT_THREADS`], then one summary at a time). Rate-limited per thread
/// to one proof per [`IC_GRACE_CATCH_UP_INTERVAL`] refusals; off with
/// `CRATONVM_JIT_IC_QUIESCENT_STAMP=0`.
pub(crate) fn jit_ic_grace_catch_up() -> bool {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
    {
        return false;
    }
    let due = THREAD_QUIESCENCE
        .try_with(|handle| {
            if !handle.ic_quiescent_stamp() {
                return false;
            }
            let left = handle.ic_catch_up_countdown.get();
            if left == 0 {
                handle.ic_catch_up_countdown.set(IC_GRACE_CATCH_UP_INTERVAL);
                // Round 14 wave 7 (lane mega): the refusing thread vouches for
                // itself first. Its miss-handler stamp was taken at the
                // handler's entry, BEFORE the handler's own compile probe,
                // whose tier-up supersede is the usual source of the retired
                // way that refused this install; left alone, that stale stamp
                // bounds `min_running_ic_gen` below the retirement and this
                // proof fails on the caller's own account, then waits
                // `IC_GRACE_CATCH_UP_INTERVAL` refusals. The thread is in Rust
                // here, past every probe it started (the same witness as
                // `jit_ic_quiescent_point`), and the store precedes the
                // `observed` read below, as the reading rule requires.
                if handle.ic_catch_up_self_stamp() {
                    handle.record.ic_quiescent_gen.store(
                        JIT_RETIRE_GENERATION.load(Ordering::Acquire),
                        Ordering::Release,
                    );
                }
                true
            } else {
                handle.ic_catch_up_countdown.set(left - 1);
                false
            }
        })
        .unwrap_or(false);
    if !due {
        return false;
    }
    let before = JIT_GRACED_GENERATION.load(Ordering::Acquire);
    let observed = JIT_RETIRE_GENERATION.load(Ordering::Acquire);
    if observed <= before {
        // Another thread graced everything since the refusal: retry.
        return true;
    }
    let Some(evidence) = collect_thread_quiescence_evidence() else {
        return false;
    };
    let graced = observed.min(evidence.min_running_ic_gen);
    note_graced_generation(graced);
    // RV3-2: the same evidence carries the code grace, bounded by depth-0
    // returns only.
    note_code_graced_generation(observed.min(evidence.min_running_gen));
    graced > before
}

pub(crate) fn jit_entry_owners(
) -> &'static parking_lot::Mutex<FxHashMap<usize, std::sync::Weak<CompiledMethod>>> {
    JIT_ENTRY_OWNERS.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Every registered entry owner still alive: `(entry, label, strong count)`.
/// A diagnostic for teardown tests, which drop a VM and look for compiled
/// bodies that outlived it.
pub fn live_jit_entry_owners() -> Vec<(usize, String, usize)> {
    let Some(owners) = JIT_ENTRY_OWNERS.get() else {
        return Vec::new();
    };
    let owners = owners.lock();
    owners
        .iter()
        .filter_map(|(entry, owner)| {
            let cm = owner.upgrade()?;
            Some((*entry, cm.method_label.clone(), Arc::strong_count(&cm) - 1))
        })
        .collect()
}

/// Register a lambda-adapter thunk as the owner of its own entry address.
///
/// `JitMICSlot::install` refuses to publish an entry whose owner it cannot
/// resolve (`jit_entry_publishable`), and the owner it holds is what keeps the
/// code alive while a slot points at it. A thunk is not a cached METHOD — it
/// has no `MethodKey` and never appears in `JitCache` — so it registers here
/// directly. See `lambda_adapter`.
pub(crate) fn register_jit_entry_owner_for_adapter(entry: usize, arc: &Arc<CompiledMethod>) {
    note_body_entered_unpublished(arc);
    jit_entry_owners().lock().insert(entry, Arc::downgrade(arc));
}

/// The identity of whatever owns `entry` right now, for recording beside a
/// baked direct-call address. `0` means "nothing owns it", which
/// `prepare_for_publication` treats as "no identity recorded".
/// `pub` and not `pub(crate)`: the interpreter's OSR tier
/// (`vm/src/runtime/interpreter/jit_bridge.rs`) bakes direct CALLs of its own
/// and must record the same bake-time identity this crate's method-entry tier
/// records, or `prepare_for_publication` has nothing to compare and falls back
/// to a liveness-only check that a recycled buffer satisfies.
pub fn jit_entry_artifact_id(entry: usize) -> u64 {
    let id = resolve_jit_entry_owner(entry).map_or(0, |o| o.artifact_id);
    // `CRATONVM_JIT_CALLEE_IDENTITY=0` keeps this lookup -- and the
    // `jit_entry_owners` lock it takes -- but records NO identity, so
    // `prepare_for_publication` falls back to its historical liveness-only
    // answer. That is the control the fix needs: a lock taken on a compile path
    // can serialise a race away by itself, and this session has already watched
    // a `Mutex` in a diagnostic make the defect vanish. If the defect stays away
    // with the check disabled, the check is not what fixed it.
    if callee_identity_check_enabled() {
        id
    } else {
        0
    }
}

/// `CRATONVM_JIT_CALLEE_IDENTITY=0` -- record no bake-time callee identity.
/// Default ON. See [`jit_entry_artifact_id`] for why the switch exists.
pub(crate) fn callee_identity_check_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

pub(crate) fn resolve_jit_entry_owner(entry: usize) -> Option<Arc<CompiledMethod>> {
    jit_entry_owners().lock().get(&entry)?.upgrade()
}

/// The `num_args` word a call site hands `jit_service_callee_deopt` (the
/// callee-deopt service, `vm/src/jit/helpers.rs`) when it kept NO copy of the
/// call's arguments: they belong to the callee (gcd d9/d, 2026-09-28). The
/// service then never re-runs the callee from entry nor runs its handler with
/// them -- the two arms that read arguments back after the call -- and hands
/// such a trap to the caller's own caller instead (a frameless deopt of the
/// caller). Every other emitter passes a plain count, which is never negative.
pub const SERVICE_ARGS_OWNED_BY_CALLEE: i32 = -1;
// No plain count can be the word: the service's decoder relies on it.
const _: () = assert!(SERVICE_ARGS_OWNED_BY_CALLEE < 0);

/// JIT round 19 wave 4, lane throws4 (THROWS3-3,
/// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`): a bit ORed into the plain
/// argument count a direct self-call's sentinel path hands
/// `jit_service_callee_deopt`. Set, the service first asks whether it has
/// anything to do at all -- no deopt frame and no exceptional frame stashed,
/// no not-entrant escape recorded, a throwable pending, a callee without an
/// exception table -- and answers `i64::MIN` (propagate) without taking and
/// restoring the signal record when it has not. The decoder masks it off
/// before reading the count; never set together with
/// [`SERVICE_ARGS_OWNED_BY_CALLEE`] (a negative whole word).
pub const SERVICE_ARGS_LEAN_PROPAGATE_BIT: u64 = 1 << 40;

/// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN=1` (JIT round 19 wave 4, lane
/// throws4, THROWS3-3 of `docs/feature-designs/jit-ir-optimizations.md`).
/// **Default ON** since the round-19 proposal round (`=0` turns it off). Read once per optimizing compile. See
/// `ir_lower::Lowerer::self_call_sentinel_lean_site`.
pub fn self_call_sentinel_lean_enabled() -> bool {
    true
}

/// JIT round 19 wave 5, lane throws5 (THROWS4-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`): ORed into
/// [`SERVICE_ARGS_LEAN_PROPAGATE_BIT`]'s word, never alone, by a single-pass
/// direct call that kept no argument copy (the callee owns them, gcd d9/d):
/// the word then stands for [`SERVICE_ARGS_OWNED_BY_CALLEE`] plus the lean
/// bit, which a negative whole word cannot carry. The service decodes it
/// back to the owned-arguments count.
pub const SERVICE_ARGS_LEAN_ARGS_OWNED_BIT: u64 = 1 << 41;
const _: () = assert!(SERVICE_ARGS_LEAN_ARGS_OWNED_BIT != SERVICE_ARGS_LEAN_PROPAGATE_BIT);

/// `CRATONVM_JIT_DIRECT_CALL_SENTINEL_LEAN=1` (JIT round 19 wave 5, lane
/// throws5, THROWS4-1). **Default ON** since the round-19 proposal round (`=0` turns it off). Read once per compile of either
/// tier: THROWS3-3's lean service bit on every statically bound direct-call
/// route -- the single-pass tier's callee-deopt service check
/// (`x64/deopt_stubs.rs` `emit_callee_deopt_service_check`: baked
/// `invokestatic` / `invokespecial` CALLs, the raw self-CALL, the inlined
/// callee checks) and the optimizing tier's direct cross call (merged,
/// deferred and split shapes) and cycle-edge cell call. Both tiers already
/// leave a narrow result's sentinel without the `dispatch_threw` peek on these
/// routes; the service's early out is the half that was missing.
pub fn direct_call_sentinel_lean_enabled() -> bool {
    true
}

/// May a site of this `JitInvokeInfo::invoke_kind` carry the lean service
/// bit? The statically bound kinds (1 special, 3 static, 4 the IR's
/// self-recursive marker): the service's lean predicate answers without the
/// receiver, which kinds 0 / 2 need for their callee-table probe (it refuses
/// them anyway; this saves it the question).
pub fn service_lean_kind_admits(invoke_kind: u8) -> bool {
    matches!(invoke_kind, 1 | 3 | 4)
}

/// The `num_args` word a site hands `jit_service_callee_deopt` for a plain
/// count `n` (`Some`) or for arguments owned by the callee (`None`), with the
/// lean bit when `lean`. Without it: exactly the historical word.
pub fn service_args_word(n: Option<usize>, lean: bool) -> i64 {
    match (n, lean) {
        // Cast: an argument count is far below `i64::MAX`.
        (Some(n), false) => n as i64,
        (None, false) => i64::from(SERVICE_ARGS_OWNED_BY_CALLEE),
        // Cast: the count plus bit 40 is far below `i64::MAX`.
        (Some(n), true) => (n as u64 | SERVICE_ARGS_LEAN_PROPAGATE_BIT) as i64,
        // Cast: bits 40 and 41 only.
        (None, true) => (SERVICE_ARGS_LEAN_PROPAGATE_BIT | SERVICE_ARGS_LEAN_ARGS_OWNED_BIT) as i64,
    }
}

/// The service's decoder for [`service_args_word`]: `(count word, lean)`,
/// where the count word is the plain count or
/// [`SERVICE_ARGS_OWNED_BY_CALLEE`].
pub fn decode_service_args_word(word: i64) -> (i64, bool) {
    // Cast: a bit test on the word's bits.
    let bits = word as u64;
    if word <= 0 || bits & SERVICE_ARGS_LEAN_PROPAGATE_BIT == 0 {
        return (word, false);
    }
    if bits & SERVICE_ARGS_LEAN_ARGS_OWNED_BIT != 0 {
        return (i64::from(SERVICE_ARGS_OWNED_BY_CALLEE), true);
    }
    // Cast: the masked word is the plain count, far below `i64::MAX`.
    ((bits & !SERVICE_ARGS_LEAN_PROPAGATE_BIT) as i64, true)
}


/// gcd d9/d (2026-09-28): may a baked single-pass direct `CALL` to `entry`
/// leave the call's reference arguments to the callee, keeping no copy of
/// them in the caller's frame for the callee-deopt service?
///
/// That copy (`reserve_direct_call_service_slots`, `x64/op_invoke.rs`) is
/// named in the call's oop map and read by the conservative band scan, so it
/// kept every argument alive for the whole call however early the callee
/// dropped it (`Gcd1ArgPinProbe`'s warm case: `caller` off=104
/// `region=operand-spill`). HotSpot's outgoing arguments belong to the
/// callee's frame. The service reads the copy on only three arms: a re-run of
/// a declined stash, a frameless trap's re-run, and a run of the callee's own
/// handler. The owner must make the third impossible and the second
/// unexpected ([`callee_body_owns_its_arguments`]); the first, and whatever of
/// the second still happens, the service hands to the caller's caller.
///
/// `false` for an entry nothing owns (an intrinsic, a thin helper, a body not
/// published through `JitCache`), which keeps today's copy.
pub fn direct_callee_owns_its_arguments(entry: usize) -> bool {
    resolve_jit_entry_owner(entry).is_some_and(|cm| callee_body_owns_its_arguments(&cm))
}

/// `CRATONVM_JIT_SERVICE_COPY_WINDOW` -- default ON, `=0` off (gkr, OOM21-2:
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// A direct call the owned-arguments hand-off refuses keeps a copy of its
/// reference arguments for the callee-deopt service, whose only readers re-run
/// the callee from entry. Such a re-run happens only where the replay rule
/// calls it exact (`replay_from_entry_commits_nothing_with_handlers`); every
/// other one is refused with `InternalError` before it reads an argument. So
/// once the callee stands at a safepoint from which no exact re-run point is
/// reachable ([`CompiledMethod::rerun_window_closed_sp_ids`]), the caller's
/// copies ([`CompiledMethod::service_arg_copies`]) have no reader, and the
/// collector clears them instead of rooting them for the rest of the call
/// (`conservative_roots::clear_closed_service_copies`). HotSpot's outgoing
/// arguments belong to the callee, whose map is liveness-exact. Read per
/// compile and per pause, no static.
pub fn service_copy_window_enabled() -> bool {
    true
}

impl CompiledMethod {
    /// gkr: the service-copy slots of the direct call returning to
    /// `ret_off` (bytes past [`Self::entry_ptr`]), if it recorded any.
    pub fn service_arg_copies_at(&self, ret_off: u32) -> Option<&[i16]> {
        self.service_arg_copies
            .binary_search_by_key(&ret_off, |(off, _)| *off)
            .ok()
            .map(|i| &*self.service_arg_copies[i].1)
    }

    /// gkr: is no exact re-run point reachable from safepoint `sp_id`?
    pub fn rerun_window_closed_at_sp_id(&self, sp_id: u32) -> bool {
        self.rerun_window_closed_sp_ids.binary_search(&sp_id).is_ok()
    }
}

/// `CRATONVM_JIT_ZERO_INCOMING_STACK_ARGS` -- default ON, `=0` off (ARGWIN21-3,
/// page `jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`):
/// both tiers' prologues zero every incoming stack argument word once they have
/// copied it to its local slot (`ir_lower` `emit_prologue`, `x64/frames.rs`).
/// The words lie in the caller's outgoing block, inside the caller's frame,
/// whose band scan otherwise kept a reference argument the callee had dropped
/// alive for the whole call. Read per compile, no static.
pub fn zero_incoming_stack_args_enabled() -> bool {
    true
}

/// The artifact half of [`direct_callee_owns_its_arguments`], split out so it
/// can be tested without a published body.
///
/// * [`CompiledMethod::method_declares_no_handlers`]: the service's handler
///   arm (`callee_has_exception_table`) answers `false`, and the method is not
///   synchronized (a synchronized stash is always declined, i.e. re-run);
/// * no frameless trap stub and no indy trap: such a trap stashes nothing, so
///   the service's only answer to it is a re-run from entry, and that arm
///   would fire on every trap rather than on a rare decline;
/// * not a wrapped or self-locking entry: those are the synchronized method's
///   two published shapes, excluded for the reason above.
pub fn callee_body_owns_its_arguments(cm: &CompiledMethod) -> bool {
    cm.method_declares_no_handlers
        && !cm.has_frameless_trap_stub
        && !cm.has_indy_trap
        && !cm.requires_wrapped_entry
        && !cm.self_locks_monitor
}


/// DIAG (2026-07-27): `CRATONVM_DBG_JIT_STALE_IC=1` reports, at each artifact
/// retirement, any inline-cache slot still pointing into the body being
/// unmapped. Read-once; default-off.
pub(crate) fn dbg_stale_ic_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

thread_local! {
    /// Re-entrancy guard: the scan holds `Arc`s, and dropping them can re-enter
    /// `CompiledMethod::drop`.
    pub(crate) static STALE_IC_SCANNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Report every live inline-cache slot whose entry pointer equals `dying`.
///
/// Such a slot hands generated code (`jit/src/x64.rs`'s `MOV R11,[slot]; CALL
/// R11` cascade, and `runtime_lowering::emit_hashed_vtable_stub`) a raw pointer
/// into a mapping that is being released right now.
pub(crate) fn report_stale_ic_holders(dying: usize) {
    if dying == 0 {
        return;
    }
    if STALE_IC_SCANNING.with(|f| f.replace(true)) {
        return;
    }
    let candidates: Vec<Arc<CompiledMethod>> = {
        let owners = jit_entry_owners().lock();
        owners.values().filter_map(|w| w.upgrade()).collect()
    };
    let dying_name = lookup_jit_method_name(dying).unwrap_or_else(|| "<unnamed>".to_string());
    for cm in &candidates {
        if cm.entry as usize == dying {
            continue;
        }
        let mut kind: Option<&'static str> = None;
        for slot in &cm._jit_mic_slots {
            if slot.cached_entry().0 as usize == dying {
                kind = Some("MIC");
            }
        }
        for slot in &cm._jit_pic_slots {
            for i in 0..JIT_PIC_ENTRIES {
                if slot
                    .way(i)
                    .is_some_and(|(_, entry, _)| entry as usize == dying)
                {
                    kind = Some("PIC");
                }
            }
            for i in 0..JIT_MEGA_ENTRIES {
                let word = slot.mega_entry_words[i].load(std::sync::atomic::Ordering::Acquire);
                if jit_ic_entry_decode(word).0 as usize == dying {
                    kind = Some("MEGA");
                }
            }
        }
        if let Some(kind) = kind {
            eprintln!(
                "[jit-stale-ic] retiring {dying:#x} ({dying_name}) still referenced by a {kind} \
slot of {:#x} ({})",
                cm.entry as usize,
                lookup_jit_method_name(cm.entry as usize)
                    .unwrap_or_else(|| "<unnamed>".to_string()),
            );
        }
    }
    drop(candidates);
    STALE_IC_SCANNING.with(|f| f.set(false));
}

pub(crate) fn deferred_jit_owners() -> &'static parking_lot::Mutex<Vec<DeferredOwner>> {
    DEFERRED_JIT_OWNERS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// The retirement queue's one lock-ordering rule, checked rather than argued
// ---------------------------------------------------------------------------
//
// `drain_deferred_jit_owners` takes the retirement queue and, WHILE HOLDING IT,
// `collect_thread_quiescence_evidence` takes `jit_threads()`. That order is not
// a style choice. Observing quiescence before taking the queue is a
// use-after-free of executable memory -- the argument is written out at the
// top of `drain_deferred_jit_owners` -- so the queue must always come first,
// and a path that took `jit_threads()` and then the queue would both invert
// the order and deadlock against a concurrent drain.
//
// Until now that rule existed only as prose. `NOTES-runtime.md` RT-3 item 4
// asked for it to be registered in `types/src/lock_order.rs` so it is checked.
//
// # Why this is a pairwise check and not two new `LockLevel`s
//
// `LockLevel` is ONE global order over eleven levels, and both of these are
// LEAF locks: nothing is acquired while either is held except the other one,
// and either can be reached with any VM lock already held (a redefinition
// holds `ClassManager`, L10, all the way down to `defer_jit_owner`). A leaf
// must sit below every level its callers might hold, so a faithful global
// registration needs two new levels BELOW `Scratch` -- which is `L0`, so it
// means renumbering all eleven, the tracker's bit-array and `level_from_u8`,
// and then checking these two against every VM lock on every path that reaches
// them. Paths that are correct today would start reporting.
//
// That is a large, VM-wide change to assert ONE nesting between two locks that
// have exactly four acquisition sites between them, all in this file. So this
// checks the one rule directly, and it is still the framework's rule in the
// ways that matter: it fires under `lock_order::enforcement_active()` -- debug
// builds, plus any test binary that calls `force_enable_for_testing` -- and it
// panics with the framework's `"lock order violation"` wording, so a
// `#[should_panic(expected = "lock order violation")]` written against the
// framework catches this too. If these locks ever gain a third acquirer, or
// start nesting with a VM lock, that is the point to promote them into
// `LockLevel` properly.

std::thread_local! {
    /// How many `jit_threads()` guards THIS thread holds. Non-zero means the
    /// retirement queue must not be taken -- see [`lock_retire_queue`].
    ///
    /// A count and not a flag: the rule is about the lock being held at all,
    /// and a count stays correct if a future path ever holds two guards.
    static JIT_THREADS_HELD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A `jit_threads()` guard that records, for its lifetime, that this thread
/// holds the lock.
pub(crate) struct JitThreadsGuard(pub(crate) parking_lot::MutexGuard<'static, Vec<Arc<JitThreadQuiescence>>>);

impl std::ops::Deref for JitThreadsGuard {
    type Target = Vec<Arc<JitThreadQuiescence>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for JitThreadsGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for JitThreadsGuard {
    fn drop(&mut self) {
        // Runs before the inner guard releases the lock, so there is a moment
        // where the count reads zero and the lock is still held. That window is
        // on THIS thread and inside `drop`, where it cannot reach
        // `lock_retire_queue`, so it cannot hide an inversion.
        JIT_THREADS_HELD.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

/// Take `jit_threads()`. Every acquisition goes through here, so the queue's
/// ordering check sees every one of them.
pub(crate) fn lock_jit_threads() -> JitThreadsGuard {
    let guard = jit_threads().lock();
    JIT_THREADS_HELD.with(|held| held.set(held.get() + 1));
    JitThreadsGuard(guard)
}

/// Take the retirement queue, asserting it is not being taken under
/// `jit_threads()`.
///
/// Every acquisition of the queue goes through here. The check is one
/// thread-local read and fires only when lock-order enforcement is active, so
/// the release build pays nothing for it.
pub(crate) fn lock_retire_queue() -> parking_lot::MutexGuard<'static, Vec<DeferredOwner>> {
    if cratonvm_types::lock_order::enforcement_active() {
        let held = JIT_THREADS_HELD.with(|held| held.get());
        if held != 0 {
            panic!(
                "lock order violation: attempted to acquire the JIT retirement queue while \
                 holding jit_threads() ({held} guard(s)); the queue must be taken FIRST -- \
                 observing quiescence before taking it is a use-after-free of executable \
                 memory (see drain_deferred_jit_owners)"
            );
        }
    }
    deferred_jit_owners().lock()
}


/// Inline-cache installs refused because the compiled target could not be
/// retained. See [`jit_entry_publishable`]. Diagnostic counter only.
pub(crate) static UNOWNED_IC_ENTRY_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of inline-cache installs refused for lack of a retainable owner.
pub fn unowned_ic_entry_refusals() -> u64 {
    UNOWNED_IC_ENTRY_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether `entry` may be published into an inline-cache slot that generated
/// code calls without further validation.
///
/// The emitted MIC/PIC cascade in `jit/src/x64.rs` loads the slot's entry
/// pointer into R11 and `CALL R11`s it, so the slot must keep the pointed-to
/// body mapped for as long as the pointer is readable. That keep-alive is the
/// slot's `compiled_owner` / `compiled_owners[i]` `Arc<CompiledMethod>`.
///
/// A missing owner is legitimate for a *native* target (a builtin trampoline,
/// never unmapped) and for the synthetic sentinels unit tests install. It is
/// NOT legitimate for an address inside a live JIT code region: that is a
/// compiled body whose `Arc` we failed to resolve, so nothing would stop the
/// next tier-up `put` from dropping the last clone and `munmap`ping it under a
/// slot still holding the raw pointer. Refuse those; the site falls back to the
/// dispatch helper, which re-resolves authoritatively.
///
/// A callee that can DEOPT is publishable: the emitted sequence checks the
/// `i64::MIN` sentinel after the inline `CALL` and routes it through
/// `jit_service_callee_deopt`, which performs the same precise resume every
/// helper-mediated dispatch arm does. Before that check existed, the sentinel
/// escaped to the caller as if it were the caller's own deopt and the callee's
/// stashed frame was mis-attributed by an unrelated sink — see
/// `handle_compiled_callee_deopt_sentinel` in `vm/src/jit/helpers.rs`.
///
/// # `jdk_only` is an argument, not a latch read
///
/// This function took its policy from the process-global
/// `JIT_COMPATIBILITY_MODE` until 2026-08-10, which was the last
/// dispatch-affecting reader of that static and the remaining half of
/// `JDK-ONLY-WAVE2` §2. It is now passed in by whoever is publishing, so a
/// `Compatible` VM sharing a process with a strict one publishes under its own
/// policy. Every production caller has a `SharedVm` two frames up and passes
/// `dispatch_policy(vm).is_jdk_only()`; unit tests that install synthetic
/// sentinels pass `false`.
///
/// The strict branch is measured unreachable in both modes — every MIC/PIC
/// publication takes its entry from `try_jit_compile_callee`, which is owned,
/// so the `owner.is_some()` early return above fires first. Threading the
/// policy does not change that; it changes *whose* policy would decide if a
/// native trampoline ever did reach here, which is what §2 is about. The
/// invariant, per the `JitMICSlot` note, is the refusal itself.
pub(crate) fn jit_entry_publishable(entry: u64, owner: &Option<Arc<CompiledMethod>>, jdk_only: bool) -> bool {
    if entry == 0 {
        return true;
    }
    // Bit 0 of an inline-cache entry word is the needs-context tag
    // (`JIT_IC_NEEDS_CONTEXT_TAG`), so an address with it set cannot be encoded
    // at all. A compiled body starts page-aligned and never has it; anything
    // else that does stays on the resolving helper.
    if entry & JIT_IC_NEEDS_CONTEXT_TAG != 0 {
        return false;
    }
    if owner.is_some() {
        return true;
    }
    // No live owner. Two shapes make that fatal rather than merely unowned:
    //
    //  * the address is still a `jit_entry_owners` key. Keys are removed only
    //    by `CompiledMethod::drop`, and only once the `Weak` is already dead,
    //    so a present key with a non-upgradable `Weak` means "this WAS a
    //    JitCache artifact and it is gone" — its buffer is unmapped, or worse
    //    already recycled by a later allocation.
    //  * the address lies inside a live JIT code region: a compiled body we
    //    simply failed to resolve, which the next tier-up `put` can drop.
    //
    // Anything else was never JIT-managed — a native/builtin target, or a unit
    // test's synthetic sentinel. Nothing owns those and nothing unmaps them,
    // so they stay publishable (that is what `compiled_owner`'s doc comment
    // means by "native targets have no owner").
    let registered = jit_entry_owners().lock().contains_key(&(entry as usize));
    let in_jit_region = {
        let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
        regions.contains(entry as usize as *const u8)
    };
    if registered || in_jit_region {
        UNOWNED_IC_ENTRY_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    // Real-JDK (`docs/book/src/user-guide/real-jdk-mode.md` §1.3, §7): an unowned
    // entry is by definition a native/builtin trampoline. Publishing it here
    // means generated code will `CALL R11` it on every guard hit with NO
    // dispatch helper on the path — so `resolve_dispatch` never runs, the
    // registry's `record_invocation` never fires, and a `SyntheticStub`
    // callback bound before the policy was consulted would execute unnoticed
    // for the life of the artifact. That is precisely the "stale JIT or
    // alternate dispatch path" the acceptance criterion forbids, and this crate
    // cannot re-check the kind here (it holds a raw address, not a
    // `NativeMethodId`, and does not depend on `native-api`).
    //
    // Refuse instead. The slot stays "class cached, target unresolved" — the
    // exact shape `prepopulate` and the two refusals above already produce — so
    // the site falls back to `jit_invoke_virtual_mic` / `jit_invoke_dispatch`,
    // which re-resolves authoritatively under the policy. No silent stub call,
    // no wild jump, and Compatible mode is untouched: this branch is reached
    // only on an inline-cache MISS (which already takes two mutexes), and only
    // after the `owner.is_some()` early return has let every JIT-compiled Java
    // callee through.
    if jdk_only {
        record_jdk_only_ic_native_refusal();
        return false;
    }
    // Allowed, but with no keep-alive. Expected only for native/builtin targets
    // and unit-test sentinels; a *production* hit here means an artifact was
    // freed between the compile probe handing out its entry and this
    // publication, which is exactly the stale-entry crash.
    let n = UNOWNED_IC_ENTRY_PUBLISHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if dbg_stale_ic_enabled() && (n <= 20 || n % 1000 == 0) {
        eprintln!("[jit-stale-ic] published UNOWNED entry {entry:#x} (n={n})");
    }
    true
}

/// Inline-cache publications of a non-zero entry with no retainable owner.
pub(crate) static UNOWNED_IC_ENTRY_PUBLISHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of inline-cache publications that carried no owner.
pub fn unowned_ic_entry_publishes() -> u64 {
    UNOWNED_IC_ENTRY_PUBLISHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// DIAG (2026-07-27): `CRATONVM_JIT_LEAK_CODE=1` makes retired JIT code
/// immortal. Used to test whether a SIGSEGV is a jump into a freed code page:
/// if the crash vanishes with this set, the fault is a use-after-free of
/// compiled code rather than a miscompile. Leaks by construction — diagnosis
/// only, never a shipping mode.
pub(crate) fn jit_leak_code_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

