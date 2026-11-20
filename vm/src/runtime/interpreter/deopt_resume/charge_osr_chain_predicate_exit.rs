#[allow(unused_imports)]
use super::*;

/// JIT round 23 wave 4 (lane L1): charge an optimizing OSR body's CHAIN exit
/// through a spliced loop's entry predicate ([`chain_predicate_charge_id`]),
/// as `jit_bridge::osr_predicate_exit_charge` charges a flat one: against the
/// OSR'd method at the outermost call site under `UncommonTrap`, and after
/// [`PER_BCI_DESPEC_LIMIT`] such charges the predicate's own chain id there,
/// which the next compile's planter reads. The splice is not withdrawn (no
/// wildcard). Returns whether the exit was charged; a charged one is bounded
/// by that withdrawal, so the caller leaves the rejection budget alone, as for
/// the flat predicate charge. Under `CRATONVM_JIT_OSR_PREDICATE_EXIT_CHARGE`
/// (the caller's switch).
///
/// JIT round 25 (item 4 residual 1): a spliced field site's compactness guard
/// ([`chain_compact_charge_id`]) is charged the same way, under its own reason
/// (`SpeculationFailed`) and chain id.
pub(crate) fn charge_osr_chain_predicate_exit(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    let Some(id) = chain_named_charge_id(rframe, cause) else {
        return false;
    };
    let Some(outer) = rframe.caller_frames.last() else {
        return false;
    };
    if outer.bci == u32::MAX {
        return false;
    }
    // `UncommonTrap` for a predicate, `SpeculationFailed` for a compactness
    // guard: the cause's own (`chain_named_charge_id` matched it).
    let Some(reason) = cause.map(|c| c.reason) else {
        return false;
    };
    let method_key = format!("{class_name}.{method_name}:{descriptor}");
    let is_unstable_if = id
        == cratonvm_jit::deopt::chain_predicate_speculation_id(
            &rframe.method_key,
            rframe.bci,
            cratonvm_jit::deopt::PredicateKind::UnstableIf,
        );
    if is_unstable_if {
        if !shared
            .jit
            .despec_registry
            .insert_if_absent(&method_key, outer.bci, id)
        {
            return true;
        }
    } else if shared
        .jit
        .despec_registry
        .contains_speculation(&method_key, outer.bci, id)
    {
        return true;
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR chain predicate exit charged: {class_name}.{method_name}{descriptor} \
             call-site bci={} (innermost {} bci={}) id={id:#x}",
            outer.bci, rframe.method_key, rframe.bci
        );
    }
    let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
        shared,
        class_id,
        class_name,
        method_name,
        descriptor,
        reason,
        outer.bci,
    );
    if !is_unstable_if {
        let _ = despec_site_after_trap_limit(shared, &method_key, outer.bci, Some(reason), id);
    }
    true
}

/// W3L4-4: the wildcard at `bci` once the call site has been charged twice
/// [`PER_BCI_DESPEC_LIMIT`] times at `(reason, bci)` under a named nested-trap
/// id `id` -- the backstop for a named withdrawal no planner row matched.
pub(in crate::runtime::interpreter::deopt_resume) fn nested_trap_wildcard_backstop(
    shared: &SharedVm,
    method_key: &str,
    bci: u32,
    reason: cratonvm_jit::deopt::DeoptReason,
    id: u32,
) {
    if bci == u32::MAX {
        return;
    }
    let charged = {
        let log = shared.jit.deopt_log.lock();
        log.deopt_count_at_site(method_key, reason, bci)
    };
    if charged >= PER_BCI_DESPEC_LIMIT.saturating_mul(2)
        && shared
            .jit
            .despec_registry
            .contains_speculation(method_key, bci, id)
    {
        shared
            .jit
            .despec_registry
            .insert(method_key, bci, cratonvm_jit::deopt::SPECULATION_ID_ANY);
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] nested-trap backstop: {method_key} bci={bci} \
                 ({charged} charges) — call site withdrawn (wildcard)"
            );
        }
    }
}

/// Round nio21 wave 7 (lane L2,
/// `r21-charat-trap-despec-skips-the-rerun-sinks-20261008`): the one
/// attribution a sink holding its OWN stashed frame charges and de-specs a
/// trap under. Written out once for the resuming arm
/// ([`real_frame_deopt_resume_or_throw_and_despeculate`]) and the doors'
/// re-run and IR-resume arms (`jit_bridge::charge_own_stashed_trap`), which
/// charged under the same `(reason, bci)` ([`stash_charge_site`]) but never
/// recorded the per-bci de-spec, so a speculation failing on those arms was
/// withdrawn only by the whole-method escalation.
///
/// * `named`: the stash's cause when this artifact recognises it, else the
///   bci derivation ([`trap_cause_at_frame`]); for an inlined chain the
///   stash's own reason under the wildcard id, or a nested trap guard's
///   named id ([`nested_trap_charge_id`]).
/// * `charge_bci`: the site in the sink method's own bytecode (the
///   outermost scope's bci for a chain, [`stash_identity_scope`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StashTrapAttribution {
    pub(crate) named: Option<cratonvm_jit::deopt::DeoptCause>,
    pub(crate) charge_bci: u32,
    /// A named chain id whose withdrawal a planner row must honour, else
    /// twice the limit records the wildcard: a nested trap guard's
    /// ([`nested_trap_charge_id`]) or a spliced compactness guard's
    /// ([`chain_compact_charge_id`]).
    pub(crate) backstop_id: Option<u32>,
}

impl StashTrapAttribution {
    /// The reason the trap is charged under: `named`'s, else `UncommonTrap`
    /// (equal to [`stash_charge_site`]'s reason in both shapes).
    pub(crate) fn charge_reason(&self) -> cratonvm_jit::deopt::DeoptReason {
        self.named
            .map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason)
    }
}

/// See [`StashTrapAttribution`].
pub(crate) fn stash_trap_attribution(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> StashTrapAttribution {
    let chain = !rframe.caller_frames.is_empty();
    let nested_trap_id = if chain {
        nested_trap_charge_id(compiled, rframe, cause)    } else {
        None
    };
    // JIT round 25 (item 4 residual 1): a withdrawn compactness guard's id
    // no planner row honours (its pc moved) takes the same backstop.
    let backstop_id = if chain {
        nested_trap_id.or_else(|| chain_compact_charge_id(rframe, cause))    } else {
        None
    };
    let named = if chain {
        // JIT round 23 wave 4 (lane L1): a spliced loop's entry predicate is
        // named by its chain id, so its withdrawal leaves the splice standing.
        // JIT round 25 (item 4 residual 1): and a spliced field site's
        // compactness guard by its own (`chain_compact_charge_id`).
        let chain_predicate_id = chain_named_charge_id(rframe, cause);
        cause.map(|c| cratonvm_jit::deopt::DeoptCause {
            reason: c.reason,
            speculation_id: nested_trap_id
                .or(chain_predicate_id)
                .unwrap_or(cratonvm_jit::deopt::SPECULATION_ID_ANY),
        })
    } else {
        trap_cause_at_frame(compiled, code, rframe, cause)
    };
    StashTrapAttribution {
        named,
        charge_bci: stash_identity_scope(rframe).bci,
        backstop_id,
    }
}

/// The per-bci de-spec every own-stash sink records AFTER its charge (the
/// log's lock must be released), at the grain of [`StashTrapAttribution`]:
/// [`despec_site_after_trap_limit`] counted at `(reason, bci)` for a named
/// speculation and at the whole bci for the wildcard, plus the W3L4-4
/// nested-trap backstop. Nothing for the superseded-guard sentinel. The
/// callers skip it exactly where they skip the charge (superseded artifact,
/// exit left for the interpreter, uncharged forced chain trip, own IR site
/// trap, which `despeculate_trapped_method` de-specs itself). Returns the
/// count when it withdrew.
pub(crate) fn despec_own_stash_site_after_charge(
    shared: &SharedVm,
    method_key: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    att: &StashTrapAttribution,
) -> Option<usize> {
    if rframe.bci == u32::MAX {
        return None;
    }
    // JIT round 19 wave 3 (SPLICE2-2): the chain form; a single-scope frame
    // or the switch off is exactly `despec_site_after_trap_limit`.
    let withdrawn = despec_chain_site_after_trap_limit(
        shared,
        method_key,
        rframe,
        att.charge_bci,
        att.named.map(|c| c.reason),
        att.named
            .map_or(cratonvm_jit::deopt::SPECULATION_ID_ANY, |c| {
                c.speculation_id
            }),
    );
    if let Some(bci_deopts) = withdrawn {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] per-bci de-spec: {} bci={} ({} deopts ≥ {}) — \
                 speculation suppressed on next compile (method stays compilable)",
                method_key, att.charge_bci, bci_deopts, PER_BCI_DESPEC_LIMIT
            );
        }
    }
    // W3L4-4 backstop: a named nested-trap withdrawal the planner did not
    // honour (no row matched) must not leave the call site trapping forever.
    // Twice the limit at `(reason, call site)` records the wildcard.
    if let (Some(id), Some(c)) = (att.backstop_id, att.named) {
        nested_trap_wildcard_backstop(shared, method_key, att.charge_bci, c.reason, id);
    }
    withdrawn
}

/// `CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC`, **default ON** (round nio21
/// wave 7, lane L2): whether the doors' re-run and IR-resume arms
/// (`jit_bridge::charge_own_stashed_trap`) and the OSR door's rejected-bail
/// charge record the per-bci de-spec the resuming arm records
/// ([`despec_own_stash_site_after_charge`]). `=0` restores the charge alone
/// (the whole-method escalation as the only brake). Read only on a charged
/// trap.
pub(crate) fn rerun_sink_per_bci_despec_enabled() -> bool {
    true
}

/// Did the stash entry whose point address is `point_addr`
/// (`take_last_deopt_with_point`) come out of `ran`, the body this sink just
/// executed? `true` for an unknown point (0: the re-run sentinel of a null
/// point, a plain restash), which leaves the decision to the name check.
///
/// Interpreter round i1 wave 10, lane L4
/// (`docs/feature-designs/deopt-osr.md`):
/// the name check alone accepts a frame fired by ANOTHER body with the same
/// `"<class>.<method>:<descriptor>"` key — another loader's copy of the class,
/// or a nested activation of this method under a different artifact — and a
/// sink would then rebuild this activation from that body's locals and bci.
/// The point address is the stash's identity token: every point a body's
/// stubs can fire, inlined scopes included, is one of that body's own
/// (`CompiledMethod::owns_deopt_point`), and `ran` is alive for the whole
/// sink, so its storage cannot have been reused for another body's points.
#[inline]
pub(crate) fn deopt_stash_is_from_artifact(
    ran: &crate::jit::CompiledMethod,
    point_addr: usize,
) -> bool {
    point_addr == 0 || ran.owns_deopt_point(point_addr)
}

/// [`deopt_frame_matches_method`] for a sink that holds the body it ran:
/// the frame must name the method AND have been fired by that very body
/// ([`deopt_stash_is_from_artifact`]).
pub(crate) fn deopt_frame_matches_artifact(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ran: &crate::jit::CompiledMethod,
    point_addr: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    deopt_frame_matches_method(rframe, class_name, method_name, descriptor)
        && deopt_stash_is_from_artifact(ran, point_addr)
}

/// jit-invokedynamic-groovy-regression fix — de-speculate the method a
/// MISMATCHED stashed frame actually belongs to (parsed from its baked
/// `method_key`), so the truly-trapping method gets evicted/blacklisted and
/// stops re-trapping, instead of the consumer's own (innocent) method eating
/// the deopt accounting. No-op for an unparseable/empty key.
///
/// `point_addr` is the address of the deopt point that stashed `rframe`
/// (`take_last_deopt_with_point`; 0 when the taker did not keep it). A trap
/// out of a body the owner's published one superseded is not charged: see
/// [`foreign_trap_came_from_a_superseded_body`].
///
/// `pub(crate)` since round 11 wave 12 (lane tier): the dispatch helpers'
/// declined-callee re-run
/// (`r11w12-tier-helpers-declined-callee-rerun-patch-FIXED-20260924.md`) charges
/// the stash it takes through this function, as the OSR sink's
/// identity-mismatch arm did when the stash reached it.
///
/// `cause` is the stash's own (`take_last_deopt_with_point`); an `OsrExit`
/// the interpreter-only mode asked for is not charged (wave 15,
/// [`super::stashed_exit_left_for_the_interpreter`]).
pub(crate) fn despeculate_stashed_frame_method_with_cause(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) {
    // Round 15 wave 4 (lane chain5): a forced chain exit that reached a sink
    // as a foreign frame is claimed here (census: forced, not resumed).
    let forced = claim_foreign_forced_chain_trip(shared, rframe, point_addr);
    // Interpreter round i1 wave 15, lane L3: the frame's method left at a
    // back-edge poll because it must now run interpreted, which says nothing
    // about its code. Charged as `UncommonTrap` below, the `OsrExit` rule of
    // `DeoptimizationController::deoptimize_in` would not see it.
    if super::stashed_exit_left_for_the_interpreter(shared, rframe, cause, point_addr) {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] foreign frame {} bci={}: left for the interpreter-only \
                 mode, not charged",
                rframe.method_key, rframe.bci
            );
        }
        return;
    }
    despeculate_claimed_stashed_frame_method(shared, rframe, point_addr, forced);
}

/// [`despeculate_stashed_frame_method_with_cause`] for a sink that did not keep
/// the stash's cause; see there.
pub(crate) fn despeculate_stashed_frame_method(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) {
    let forced = claim_foreign_forced_chain_trip(shared, rframe, point_addr);
    despeculate_claimed_stashed_frame_method(shared, rframe, point_addr, forced);
}

/// Round 15 wave 4 (lane chain5; F2 of `r15w4-chain4-review-findings`): a
/// forced chain exit whose stash reached a sink that does not hold the
/// trapping body -- the call-site service refused it before building the
/// chain and an outer door took it as a foreign frame, or the service's
/// declined-callee re-run took it -- was never claimed: it stayed in flight
/// and the next trip at that point was counted forced in its place, and it
/// was charged (evicting its owner by name) like a failed speculation. Claimed
/// here.
///
/// Round 16 lane chain (CH5W4-1): by the stash's own forced mark
/// ([`claim_forced_chain_trip_at`]), not through the owner's published body
/// found by name: a superseded body's forced trip (G1 of
/// `r15w4-chain5-review-findings`), a class name two loaders define, and a
/// trip the call-site service claimed and restashed (G2: still forced, so not
/// charged, and counted once) are all exact now. Only a chain stash can be a
/// forced exit: a single-scope frame returns before the claim.
pub(in crate::runtime::interpreter::deopt_resume) fn claim_foreign_forced_chain_trip(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) -> bool {
    if point_addr == 0 || rframe.caller_frames.is_empty() {
        return false;
    }
    claim_forced_chain_trip_at(shared, point_addr)
}

/// [`despeculate_stashed_frame_method`] once the forced-trip claim was made:
/// a claimed forced exit is left uncharged under
/// `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED` ([`leave_forced_trip_uncharged`]).
pub(in crate::runtime::interpreter::deopt_resume) fn despeculate_claimed_stashed_frame_method(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
    forced: bool,
) {
    if leave_forced_trip_uncharged(shared, forced) {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] foreign frame {} bci={}: forced chain exit \
                 (CRATONVM_DEOPT_EAGER_CHAINS), not charged",
                rframe.method_key, rframe.bci
            );
        }
        return;
    }
    // The frame's OWNER is the method its artifact belongs to: an inlined
    // chain's outermost scope, charged at that scope's own bci (the call site
    // of the splice), not the spliced callee that trapped, whose own body is
    // innocent (wave 26, lane L2; `stash_identity_scope`). The same frame for
    // a single scope.
    let owner = stash_identity_scope(rframe);
    let Some((rest, key_desc)) = owner.method_key.rsplit_once(':') else {
        return;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        return;
    };
    if foreign_trap_came_from_a_superseded_body(shared, key_class, key_method, key_desc, point_addr)
    {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] foreign frame {} bci={}: trap from a superseded body, \
                 not charged, fresh body kept",
                rframe.method_key, rframe.bci
            );
        }
        return;
    }
    // N3(b). This used to charge `UnreachedCode`, the one reason
    // `recommend_action` answers `MakeNotCompilable` on the FIRST occurrence —
    // so method `B` was permanently retired because method `A`'s sink saw
    // `B`'s frame. Read what that says: "a frame arrived at the wrong sink" is
    // evidence about the STASH ROUTING, not about `B`'s code, and `B` may
    // never have mis-speculated at all. All this site actually knows is that
    // `B` trapped; charging it as if `B` were unreachable is a verdict nobody
    // took.
    //
    // `UncommonTrap` is the honest reason. It is CHARGED, so `B`'s trap rate
    // is still visible and `B` still backs off if it really is re-trapping —
    // which is the whole point of de-speculating the frame's real owner rather
    // than the innocent consumer — but it escalates on the ordinary
    // count-based schedule, so ONE mis-routed frame cannot blacklist a method.
    // Reserve `UnreachedCode` for genuinely unreachable code.
    //
    // The precise reason is NOT recoverable here: this sink holds the frame and
    // its bci but not `B`'s artifact, and looking `B`'s artifact up would mean
    // trusting a `method_key` we have just established does not name the method
    // we are in. `trap_reason_for_frame` is the disambiguating form and it needs
    // the artifact the frame actually came from.
    crate::jit::helpers::DeoptimizationController::deoptimize(
        shared,
        key_class,
        key_method,
        key_desc,
        cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        owner.bci,
    );
}

/// Did the point at `point_addr` fire in a body that is no longer published
/// for `class.method desc`? The foreign-frame form of
/// [`trapped_artifact_is_superseded`], for a sink that holds the frame's
/// owner only by name (r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
/// the last by-name sink).
///
/// `DeoptimizationController::deoptimize` evicts by name, so charging a trap
/// out of a retired body would evict the FRESH body its first trap's
/// recompile published, and count the one speculation failure again.
///
/// The class is resolved exactly as `deoptimize` resolves it (by name, id 0
/// when no single loader answers), so this asks about the very slot the charge
/// would evict. `true` only on a positive mismatch: a known point (non-zero),
/// a published method-entry body that does not own it, and no published OSR
/// body that owns it. An empty slot or an unknown point keeps the charge, as
/// `helpers::try_resume_trapped_callee`'s identical rule does, so the address
/// only ever suppresses a charge.
///
/// Limit, shared with that rule: an address cannot tell a retired body from
/// one that was never published under this name
/// (`helpers::trapping_artifact_is_superseded`, which holds the artifact,
/// charges such a body). Both answer "superseded" here. The charge lost for the second kind
/// would have evicted the published body, which is not the one trapping; what
/// it did buy is the ledger count toward `MakeNotCompilable`.
pub(in crate::runtime::interpreter::deopt_resume) fn foreign_trap_came_from_a_superseded_body(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    point_addr: usize,
) -> bool {
    if point_addr == 0 {
        return false;
    }
    let class_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let cache = shared.jit.jit_cache.read();
    let Some(live) = cache.get(class_name, method_name, descriptor, class_id) else {
        return false;
    };
    let osr = cache.get_osr(class_name, method_name, descriptor, class_id);
    let by_address = !live.owns_deopt_point(point_addr)
        && !osr
            .as_ref()
            .is_some_and(|osr| osr.owns_deopt_point(point_addr));
    // Interpreter round i2 wave 61, lane L2 (the last sink of
    // `i59-L2-proposal-every-stash-names-its-trapping-body`): the body the
    // trampoline named at the trap, compared with the published bodies by
    // POINTER. An address can name a freed body's storage reused by the
    // published one; a pointer to a live `Arc` cannot.
    if !cratonvm_jit::deopt::stash_names_trapping_body_enabled() {
        return by_address;
    }
    // j27 wave 7 (lane T1LAM3): held as a `RetainedCode` (the stash's `Weak`
    // upgrade may be the body's last owner).
    let Some(named) =
        crate::jit::helpers::stale_trap_not_entrant::taken_trapping_body_retained(point_addr)
    else {
        return by_address;
    };
    let superseded = !Arc::ptr_eq(&live, named.arc())
        && !osr.as_ref().is_some_and(|osr| Arc::ptr_eq(osr, named.arc()));
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] foreign trap body named by the stash: {class_name}.{method_name}{descriptor} \
             superseded={superseded} by-address={by_address} agrees={}",
            superseded == by_address
        );
    }
    superseded
}

/// Was the class of any INNER scope of `rframe`'s inlined chain (a spliced
/// callee, resolved as [`materialise_inner_scopes`] resolves it) redefined
/// after `compiled`'s compilation began? Such a scope would be rebuilt from its
/// method's current bytecode, which is not the bytecode its bci and locals
/// describe (round 13 wave 5, lane resume). `false` at once while no class was
/// ever redefined, for a scope that does not resolve (the chain builder
/// refuses it anyway), and with `CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES=0`
/// (default ON; read only once a spliced callee's class was redefined).
/// `false` for a single frame. The doors' re-run answer asks it too
/// (`jit_bridge::door_rerun_or_refuse`): such a chain re-runs as a redefined
/// class's frame does, instead of raising.
pub(in crate::runtime::interpreter) fn chain_inner_scope_redefined_since_compile(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if rframe.caller_frames.is_empty() || !crate::classloading::any_class_redefined() {
        return false;
    }
    let mut enclosing = outermost_class;
    // Outermost-inward, skipping the outermost scope (the compiled method).
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe))
    {
        // By the class the compile spliced (round 13 wave 6, lane chain2): the
        // name rule could answer another class of the same name, whose
        // redefinition says nothing about this scope.
        let Ok(callee) = resolve_inlined_callee_scoped(
            shared,
            enclosing,
            &scope.method_key,
            scope.class_id,
            &compiled.splice_scope_class_ids,
        ) else {
            return false;
        };
        let class_id = callee.declaring_class_id;
        if class_was_redefined(shared, class_id)
            && !shared
                .jit
                .jit_cache
                .compiled_since_redefinition_of(class_id, compiled.install_epoch)
        {
            return true;
        }
        enclosing = class_id;
    }
    false
}

/// Round 14 wave 3 (lane chain; proposal R14DP-5, page
/// `r13w5-resume-chain-inner-scope-of-a-redefined-class`): the templates the
/// inner scopes of `rframe`'s inlined chain whose class was redefined since
/// `compiled`'s compilation began ([`chain_inner_scope_redefined_since_compile`])
/// must be rebuilt from -- the bytecode the compile spliced
/// (`CompiledMethod::splice_scope_sources`), the obsolete method a JVMTI
/// redefinition leaves such an activation running.
///
/// `Some(rows)`, one `(method key, template)` per such scope, when EVERY such
/// scope has a retained body that names the same static-ness as the current
/// method and whose frame the class's history can move onto the current pool
/// (`obsolete_frames::rebuilt_body_translates`, from the body's
/// `compile_cp_stamp`). The template keeps the current method's identity,
/// modifiers and source file (a redefinition changes none of the first two),
/// carries the row's exception table (the body's original rows, catch types
/// translated by `rebuilt_body_translates` below; a masked body's row is its
/// original bytes: `cratonvm_jit` `splice_scope_sources`, round rx27 lane
/// matcher),
/// and bounds its operand stack by two slots per code byte (no instruction
/// pushes more than two). The caller restamps each frame built from one
/// ([`restamp_inner_own_source_frames`]).
///
/// `None` (keep the refusal) for a single frame, when no inner scope was
/// redefined, when a scope does not resolve or has no row, and with
/// `CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE=0`. Asked by the doors and, since
/// round 14 wave 4 (lane resume2, CH3W-3), by every other chain sink
/// ([`build_stale_chain_from_own_sources`], the OSR-exit chain transfer).
pub(in crate::runtime::interpreter) fn chain_inner_scope_own_sources(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<Vec<(String, Arc<CachedBytecodeMethod>)>> {
    if rframe.caller_frames.is_empty()
        || compiled.splice_scope_sources.is_empty()
        || !crate::classloading::any_class_redefined()
        || !cratonvm_jit::chain_inner_own_source_enabled()
    {
        return None;
    }
    let stamp = compiled.compile_cp_stamp()?;
    let mut rows: Vec<(String, Arc<CachedBytecodeMethod>)> = Vec::new();
    let mut enclosing = outermost_class;
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe))
    {
        let current = resolve_inlined_callee_scoped(
            shared,
            enclosing,
            &scope.method_key,
            scope.class_id,
            &compiled.splice_scope_class_ids,
        )
        .ok()?;
        let class_id = current.declaring_class_id;
        enclosing = class_id;
        if !class_was_redefined(shared, class_id)
            || shared
                .jit
                .jit_cache
                .compiled_since_redefinition_of(class_id, compiled.install_epoch)
            || rows.iter().any(|(key, _)| *key == scope.method_key)
        {
            continue;
        }
        let source = compiled
            .splice_scope_sources
            .iter()
            .find(|row| row.method_key == scope.method_key)?;
        if source.is_static != current.is_static {
            return None;
        }
        let code_len = source.code.len().saturating_sub(2);
        let template = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: class_id,
            class_name: current.class_name.clone(),
            method_name: current.method_name.clone(),
            method_descriptor: current.method_descriptor.clone(),
            source_file: current.source_file.clone(),
            code: Arc::clone(&source.code),
            exception_table: source
                .exception_table
                .iter()
                .map(|&(start_pc, end_pc, handler_pc, catch_type)| {
                    cratonvm_reader::attribute::ExceptionTableEntry {
                        start_pc,
                        end_pc,
                        handler_pc,
                        catch_type,
                    }
                })
                .collect(),
            max_stack: u16::try_from(code_len.saturating_mul(2)).unwrap_or(u16::MAX),
            max_locals: source.max_locals,
            num_params: current.num_params,
            is_synchronized: current.is_synchronized,
            is_static: current.is_static,
        });
        if !super::obsolete_frames::rebuilt_body_translates(
            shared,
            class_id,
            &template.code,
            &template.exception_table,
            stamp,
        ) {
            return None;
        }
        rows.push((scope.method_key.clone(), Arc::new(template)));
    }
    (!rows.is_empty()).then_some(rows)
}

/// Restamp, with the body's constant-pool generation, every frame of the
/// inlined chain just pushed for `rframe` that was built from an own-source
/// template ([`chain_inner_scope_own_sources`]), so it is moved onto its
/// class's current pool before it runs, exactly as the outermost frame is
/// (`obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code`). The chain
/// occupies the top `1 + caller_frames.len()` frames, outermost first.
pub(in crate::runtime::interpreter::deopt_resume) fn restamp_inner_own_source_frames(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
    compile_cp_stamp: Option<u64>,
) {
    if own_sources.is_empty() {
        return;
    }
    let Some(outermost) = thread
        .frames
        .len()
        .checked_sub(1 + rframe.caller_frames.len())
    else {
        return;
    };
    let inner = rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe));
    for (k, scope) in inner.enumerate() {
        if own_sources.iter().any(|(key, _)| *key == scope.method_key) {
            super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                shared,
                thread,
                outermost + 1 + k,
                compile_cp_stamp,
            );
        }
    }
}

/// Was the OUTERMOST scope's class of `rframe`'s inlined chain -- the
/// compiled method's own, `outermost_class` -- redefined after `compiled`'s
/// compilation began? Round 13 wave 8, lane chain3.
///
/// [`chain_inner_scope_redefined_since_compile`] asks the spliced callees;
/// this asks the method the body belongs to, for the sinks that rebuild its
/// frame from the CURRENT bytecode (the door's `cached`, the tier-up sink's
/// current `Code` attribute). A single frame has the own-source resume and
/// the constant-pool restamp for that case; a chain has neither (both are
/// single-frame, and `obsolete_frames::stamp_frame_rebuilt_from_compiled_code`
/// stamps only the TOP frame, which for a chain is the innermost), so the
/// frame would be parked at the old bci in the new code. `false` for a single
/// frame, while no class was ever redefined, and for a body compiled after
/// the class's last redefinition (its bytecode IS the current one). Kill
/// switch `CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES` (default ON; read
/// only once such a chain traps). Not asked by the OSR-exit chain transfer:
/// that one writes the live frame, whose own bytecode it reads.
pub(in crate::runtime::interpreter) fn chain_outer_scope_redefined_since_compile(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if rframe.caller_frames.is_empty() || !crate::classloading::any_class_redefined() {
        return false;
    }
    class_was_redefined(shared, outermost_class)
        && !shared
            .jit
            .jit_cache
            .compiled_since_redefinition_of(outermost_class, compiled.install_epoch)
}

/// Round 14 wave 2 (lane deopt; `r13w5-resume-chain-inner-scope-of-a-redefined-class`,
/// the outermost-scope half of the per-scope own-source resume): the bytecode
/// the OUTERMOST frame of `rframe`'s inlined chain must be rebuilt from when
/// the compiled method's own class was redefined after `compiled`'s
/// compilation began ([`chain_outer_scope_redefined_since_compile`]) and no
/// spliced callee's class was ([`chain_inner_scope_redefined_since_compile`]).
///
/// The outermost scope IS the compiled method, and its template is on the
/// artifact (`CompiledMethod::compiled_source`, [`obsolete_activation_source`]
/// with its translation check); the inner scopes are then current, so the
/// chain is exact once the outermost frame is built from that template and
/// restamped with the body's pool generation -- which every chain sink
/// already does to its outermost frame (`push_inlined_chain`'s
/// `outermost_cp_stamp`, the doors' `stamp_frame_at_rebuilt_from_compiled_code`).
/// Before, such a chain was refused, and the refusal re-ran the method from
/// entry (the doors), raised `InternalError` (the tier-up sink) or restashed
/// (the call-site service). A chain whose spliced callee's class was
/// redefined still needs the per-scope templates and stays refused.
///
/// `None` for a single frame, when the outermost class was not redefined since
/// the compile, when an inner scope's was, when the body kept no source or its
/// frame would not translate, or with `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE=0`
/// (default ON; read only on such a trap).
pub(crate) fn chain_outermost_own_source(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<Arc<CachedBytecodeMethod>> {
    let outermost = cached.declaring_class_id;
    if !chain_outer_scope_redefined_since_compile(shared, compiled, outermost, rframe)
        || chain_inner_scope_redefined_since_compile(shared, compiled, outermost, rframe)
    {
        return None;
    }
    obsolete_activation_source(shared, compiled, cached)
}

/// Round 14 wave 4 (lane resume2; proposal CH3W-3 of `jit-r14-chain3-proposals-RETIRED-20260930.md`,
/// patch `r14w3-chain-tierup-sink-inner-own-source-patch`): the chain of
/// `rframe`, trapped out of `compiled`, for a sink that runs it to completion
/// itself (the first-call tier-up sink, the call-site service by point) when a
/// scope's class was redefined since the compile
/// ([`chain_inner_scope_redefined_since_compile`],
/// [`chain_outer_scope_redefined_since_compile`]). Each such scope is built from
/// the bytecode the body ran: the redefined spliced callees from the compile's
/// retained bodies ([`chain_inner_scope_own_sources`]), the outermost from the
/// body's own template ([`obsolete_activation_source`], under
/// `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE`); every frame so built, and the
/// outermost, is restamped with the body's pool generation at the push
/// (`DeoptFrameChain::inner_own_source_frames`, `outermost_cp_stamp`). `None`
/// (the sink's refusal) when any stale scope has no usable template, and for a
/// redefined spliced callee with `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS=0`
/// ([`chain_own_source_sinks_enabled`]; the outermost-only case is round 14
/// wave 2's and keeps its own switch). The doors ask the same two helpers
/// inline and restamp positionally.
pub(in crate::runtime::interpreter) fn build_stale_chain_from_own_sources(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
) -> Option<DeoptFrameChain> {
    let outermost = cached.declaring_class_id;
    let inner_sources =
        if chain_inner_scope_redefined_since_compile(shared, compiled, outermost, rframe) {
            if !chain_own_source_sinks_enabled() {
                return None;
            }
            chain_inner_scope_own_sources(shared, compiled, outermost, rframe)?
        } else {
            Vec::new()
        };
    let resume_from =
        if chain_outer_scope_redefined_since_compile(shared, compiled, outermost, rframe) {
            obsolete_activation_source(shared, compiled, cached)?
        } else {
            Arc::clone(cached)
        };
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {}.{}{} bci={}: a stale chain resumes in the bytecode the body ran \
             ({} inner scope template(s), outermost from {})",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            rframe.bci,
            inner_sources.len(),
            if Arc::ptr_eq(&resume_from, cached) {
                "the current code"
            } else {
                "the body's own"
            },
        );
    }
    build_deopt_frame_chain_sourced(shared, &resume_from, rframe, hints, &inner_sources)
        .map(|chain| chain.with_outermost_cp_stamp(compiled.compile_cp_stamp()))
}

/// `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS`, **default ON** (round 14 wave 4,
/// lane resume2, CH3W-3): the chain sinks other than the doors -- the
/// first-call tier-up sink, the call-site service by point and both OSR-exit
/// chain transfers -- rebuild a redefined spliced callee's scope from the
/// bytecode the compile spliced, as the doors have since R14DP-5. `=0`
/// restores their refusal (and the call-site service's refusal of a chain
/// whose outermost class alone was redefined). Read only on such a trap.
pub(in crate::runtime::interpreter::deopt_resume) fn chain_own_source_sinks_enabled() -> bool {
    true
}



/// deopt-osr Step 9 — resume a real-frame deopt under the epoch staleness
/// guard, then drive de-speculation. Only reached under `CRATONVM_DEOPT_REAL`
/// with `compiled.can_deopt_resume`, so it is inert in production.
///
/// 1. **Staleness guard.** The running artifact (`compiled`) carries the
///    compilation epoch live when it was installed; the method's *live* epoch
///    advances on every invalidation (`SharedVm::bump_compilation_epoch`). An
///    artifact whose epoch is behind is superseded, but its frame is still
///    resumed: the trapping frame owns its artifact, so the baked
///    `DeoptimizationPoint` is valid and self-consistent with the code that
///    trapped (the epochs version the speculation, not the frame layout).
///    Only a redefinition of the declaring class, which invalidates the
///    bytecode itself, falls back to the whole-method re-run (`None`).
/// 2. **Resume.** Build + push the interpreter frame and resume at the
///    trapping bci (`resume_real_ir_deopt`).
/// 3. **De-speculation.** Record the deopt and drive the escalation policy
///    (`DeoptimizationController::deoptimize`: log the event so the deopt rate
///    is observable, evict so the next call recompiles, blacklist on repeated
///    deopts), which also advances the live epoch. Run AFTER the resume
///    decision so it only affects FUTURE invocations — the current frame,
///    already resumed, is unaffected. The reason is recovered from the matching
///    deopt point so OSR-exit events stay countable separately from guards.
///
/// `cause` is the [`cratonvm_jit::deopt::DeoptCause`] the stash kept beside
/// `rframe` (`take_last_deopt_with_point`), `None` when the taker had none;
/// see [`trap_cause_at_frame`] for how it is used. `point_addr` is the
/// address of the point that fired (0 when unknown); only a FOREIGN frame's
/// charge reads it (`despeculate_stashed_frame_method`), because this
/// method's own is judged by `compiled`'s identity.
///
/// Returns `Ok(Some)` when the frame was resumed (caller returns it), `Ok(None)`
/// to fall through to the whole-method re-run, and `Err` with the
/// `OutOfMemoryError` [`resume_real_ir_deopt_or_throw`] raises when the heap
/// could not re-materialise the frame's scalar-replaced objects (round 12 wave
/// 5): the trapped method's outcome, never a re-run. The charge below is made
/// on every path.
pub(in crate::runtime::interpreter) fn real_frame_deopt_resume_or_throw_and_despeculate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // Identity gate (jit-invokedynamic-groovy-regression): never resume a
    // frame that was stashed by a DIFFERENT method (a nested compiled callee's
    // trap whose sentinel propagated up to this outer sink). Resuming it here
    // would materialize THIS method's frame with the callee's locals/stack/bci
    // — arbitrary misexecution (the root cause of the Groovy "duplicate main
    // method" compiler-internal failures that forced the 5ceb880f revert).
    // De-speculate the frame's real owner so it stops re-trapping, then fall
    // back to the whole-method re-run (the pre-existing conservative path).
    // The frame must also come out of `compiled` itself, not out of another
    // body with the same name (`deopt_stash_is_from_artifact`).
    //
    // An inlined chain is named by its OUTERMOST scope (wave 26, lane L2;
    // `stash_identity_scope`): `resume_real_ir_deopt` materialises the whole
    // chain and checks that scope against `cached` again itself.
    let own_body = deopt_stash_is_from_artifact(compiled, point_addr);
    // Round 15 wave 4 (lane chain4): a forced chain exit of this body, claimed
    // before anything can refuse (census only; `false` for another body's
    // point). Its outcome is counted once `resumed` is known, below.
    let forced = claim_forced_chain_trip(shared, compiled, point_addr);
    if !own_body
        || !deopt_frame_matches_method(
            stash_identity_scope(rframe),
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] stashed frame identity mismatch: frame={} bci={} \
                 own_body={own_body} vs sink method {}.{}:{} — despeculating frame \
                 owner, safe re-run",
                rframe.method_key,
                rframe.bci,
                cached.class_name,
                cached.method_name,
                cached.method_descriptor
            );
        }
        // A key-less sentinel is this body's only when its point is (or is
        // unknown); another body's sentinel names no owner to charge.
        if rframe.method_key.is_empty() && own_body {
            // r11-tier: the identity-less re-run sentinel names no owner, so
            // `despeculate_stashed_frame_method` returns without charging
            // anything and the body that stashed it — the one this sink just
            // ran, since the stash is popped innermost-first — kept trapping
            // uncounted. Charge it here, the way the sinks' non-resume arm
            // charges an ordinary frame; `UncommonTrap` escalates on the
            // count-based schedule, so one sentinel blacklists nothing.
            crate::jit::helpers::DeoptimizationController::deoptimize_in(
                shared,
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cratonvm_jit::deopt::DeoptReason::UncommonTrap,
                rframe.bci,
            );
            return Ok(None);
        }
        despeculate_stashed_frame_method_with_cause(shared, rframe, cause, point_addr);
        return Ok(None);
    }
    let method_key = format!(
        "{}.{}:{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    );
    let live = shared.compilation_epoch_for(&method_key);
    // A superseded artifact still resumes. The trapping frame owns its
    // artifact until it returns, so its code and deopt boxes are live, and the
    // snapshot is SELF-CONSISTENT with the (stale, still executing) code that
    // trapped — the epochs version the speculation, not the frame layout — so
    // resuming is sound once the identity check above passed
    // (jit-invokedynamic-groovy-regression fix: skipping here forced the
    // imprecise whole-method re-run for every trap arriving through a stale
    // cached entry right after the first de-speculation, re-duplicating side
    // effects). Class redefinition invalidates the bytecode itself, so it keeps
    // the conservative skip — except for the mode exit the safepoint slow path
    // granted a body compiled after every redefinition of this VM, whose
    // bytecode is the class's current one (interpreter round i1 wave 18, lane
    // L3; `granted_exit_of_a_current_body`), and for ANY trap out of a body
    // whose compilation began after the class's last redefinition
    // ([`compiled_from_the_current_bytecode`]; round 12 wave 6, lane jni).
    let redefined = class_was_redefined(shared, cached.declaring_class_id);
    // Round 13 wave 5 (lane resume): an inlined chain whose SPLICED callee's
    // class was redefined after this body's compilation began would be rebuilt
    // from that callee's NEW bytecode (inner scopes are resolved now) at the
    // old bci with the old locals. Not resumed; see
    // `r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`.
    let chain_inner_stale = chain_inner_scope_redefined_since_compile(
        shared,
        compiled,
        cached.declaring_class_id,
        rframe,
    );
    if chain_inner_stale && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: a spliced callee's class was redefined \
             since the compile; the inlined chain is not resumed",
            rframe.bci
        );
    }
    // Round 13 wave 8 (lane chain3): the same for the OUTERMOST scope of a
    // chain. A single frame of a redefined class resumes in the bytecode it
    // was compiled from (`own_source` below, restamped); a chain has no
    // own-source path and would push the outermost frame from `cached`, which
    // is not that bytecode once the class changed after the compile.
    let chain_outer_stale = chain_outer_scope_redefined_since_compile(
        shared,
        compiled,
        cached.declaring_class_id,
        rframe,
    );
    if chain_outer_stale && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: the class was redefined since the compile; \
             the inlined chain is not resumed",
            rframe.bci
        );
    }
    // Round 14 wave 3 (lane chain, R14DP-5): such an inner scope is rebuilt
    // from the bytecode the compile spliced when the artifact kept it for
    // every such scope (`chain_inner_scope_own_sources`); only a scope with no
    // usable template still refuses the chain.
    let inner_own_sources = if chain_inner_stale {
        chain_inner_scope_own_sources(shared, compiled, cached.declaring_class_id, rframe)    } else {
        None
    };
    let chain_inner_refused = chain_inner_stale && inner_own_sources.is_none();
    if inner_own_sources.is_some() && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: the redefined spliced callees resume in \
             the bytecode the compile spliced",
            rframe.bci
        );
    }
    let fresh = !chain_inner_refused
        && !chain_outer_stale
        && (compiled.compilation_epoch >= live
            || !redefined
            || super::granted_exit_of_a_current_body(shared, cause, point_addr)
            || compiled_from_the_current_bytecode(shared, compiled, cached.declaring_class_id));
    // Round 13 wave 3 (lane replay2; proposal R13-1): a body of a redefined
    // class resumes in the bytecode it was compiled from when its publish
    // site kept it, whatever `cached` holds, and a superseded one resumes
    // too instead of re-running from entry. Nothing changes for a class
    // never redefined, nor (until round 14 wave 2, below) for an inlined
    // chain.
    //
    // Interpreter round i1 wave 46, lane L2: and the exit the safepoint
    // verdict granted a method-entry body forced to leave as its class's
    // obsolete activation after a renumbering redefinition
    // (`jit_bridge::granted_obsolete_activation_source`), also when the door
    // holds that very bytecode (`obsolete_activation_source` answers `None`
    // for `cached` itself, and such a body is neither fresh nor current).
    let own_source = if redefined && rframe.caller_frames.is_empty() {
        obsolete_activation_source(shared, compiled, cached).or_else(|| {
            super::granted_obsolete_activation_source(
                shared,
                cause,
                point_addr,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .map(|(source, _)| source)
            .filter(|source| {
                compiled
                    .compiled_source()
                    .is_some_and(|own| Arc::ptr_eq(own, source))
            })
        })
    } else if chain_outer_stale && !chain_inner_stale {
        // Round 14 wave 2 (lane deopt): a chain whose only redefined scope is
        // the outermost resumes with that frame rebuilt from the body's own
        // template (restamped below, as every resumed outermost frame is).
        chain_outermost_own_source(shared, compiled, cached, rframe)
    } else if chain_outer_stale && !chain_inner_refused {
        // Round 14 wave 3 (lane chain, R14DP-5): both halves -- the outermost
        // frame from the body's own template (as above; that helper asks the
        // inner scopes itself and would refuse), the inner ones from theirs.
        true
            .then(|| obsolete_activation_source(shared, compiled, cached))
            .flatten()    } else {
        None
    };
    let resume_from = own_source.as_ref().unwrap_or(cached);
    let inner_own_sources: &[(String, Arc<CachedBytecodeMethod>)] =
        inner_own_sources.as_deref().unwrap_or(&[]);
    let resumed = if fresh || own_source.is_some() {
        if own_source.is_some() && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: resuming in the bytecode the body \
                 was compiled from (class redefined since; fresh={fresh})",
                rframe.bci
            );
        }
        let resumed = resume_real_ir_deopt_or_throw(
            shared,
            thread,
            resume_from,
            rframe,
            &compiled.splice_scope_class_ids,
            inner_own_sources,
        );
        // The rebuilt frame runs the code the body was compiled from: stamped
        // with the body's constant-pool generation, so a frame of a body its
        // class's redefinition replaced reads its own constants (interpreter
        // round i1 wave 28, lane L3). For an inlined chain, its OUTERMOST
        // frame, which the chain pushed `caller_frames.len()` frames below the
        // top (wave 37; the inner frames are current, see
        // `DeoptFrameChain::with_outermost_cp_stamp`).
        if matches!(resumed, Ok(Some(CachedCallResult::FramePushed))) {
            if let Some(outermost) = thread
                .frames
                .len()
                .checked_sub(1 + rframe.caller_frames.len())
            {
                super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                    shared,
                    thread,
                    outermost,
                    compiled.compile_cp_stamp(),
                );
            }
            // Round 14 wave 3 (lane chain, R14DP-5): and every inner frame
            // built from the bytecode the compile spliced.
            restamp_inner_own_source_frames(
                shared,
                thread,
                rframe,
                inner_own_sources,
                compiled.compile_cp_stamp(),
            );
        }
        resumed
    } else {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] skip resume — artifact epoch {} < live {} for {} (superseded)",
                compiled.compilation_epoch, live, method_key
            );
        }
        // Round 15 wave 4 (lane chain4): a chain this door will not resume (a
        // redefinition it cannot vouch for) is a census refusal, as the other
        // sinks count theirs; it was uncounted here.
        if !rframe.caller_frames.is_empty() {
            note_inlined_chain_census(shared, ChainCensusEvent::Refused);
        }
        Ok(None)
    };
    // Round 15 wave 4 (lane chain4): the claimed forced exit's outcome.
    note_forced_chain_resumed(
        shared,
        forced,
        !rframe.caller_frames.is_empty()
            && matches!(resumed, Ok(Some(CachedCallResult::FramePushed))),
    );
    // De-speculate (record + evict + escalate + bump live epoch). The reason is
    // recovered from the deopt point matching the trapping bci so OSR-exit
    // events are tallied separately from guard deopts.
    //
    // Not for a trap out of a SUPERSEDED artifact (a newer body is already
    // published): `deoptimize` evicts by name, so it would remove the fresh
    // body, and the speculation failure was already charged when the old body
    // was retired. See `trapped_artifact_is_superseded`.
    let superseded = trapped_artifact_is_superseded(shared, cached, compiled);
    // One attribution for the charge and the per-bci de-spec below: the
    // stash's cause when this artifact recognises it, else the bci derivation.
    //
    // An inlined chain (wave 26, lane L2) trapped at a bci of a spliced
    // CALLEE's code, which neither derivation may read against `cached`'s
    // bytecode: it is charged at the call site in `cached`
    // (`stash_charge_site`), under the stash's own reason with the wildcard
    // speculation id, so the per-bci de-spec below withdraws what the
    // artifact speculated at that call site.
    //
    // Round nio20 wave 4 (lane L3, W3L4-4): a NESTED trap guard (the
    // innermost scope's own `invokevirtual` / `invokeinterface`) is charged
    // at the call site under a NAMED id derived from that scope, so its
    // withdrawal drops that nested row only (`ir_splice_strip_withdrawn_nested_traps`)
    // instead of the whole outer splice. See [`nested_trap_charge_id`].
    //
    // Round nio21 wave 7 (lane L2): in one copy with the doors' re-run arms,
    // [`stash_trap_attribution`].
    let attribution = stash_trap_attribution(compiled, &cached.code, rframe, cause);
    let named = attribution.named;
    let charge_bci = attribution.charge_bci;
    // Interpreter round i1 wave 15, lane L3: a method-entry body that left at
    // a back-edge poll because its method must now run interpreted
    // (`jvmti_events::exit_left_for_the_interpreter`) is resumed above like
    // any trap, and neither charged nor de-spec'd below. The stash's own cause
    // first, as `named` prefers it.
    let left_for_the_interpreter = cause.or(named).is_some_and(|c| {
        super::exit_left_for_the_interpreter(
            shared,
            c.reason,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        )
    });
    // Round 15 wave 4 (lane chain5): a claimed forced chain exit is a passing
    // guard, neither charged nor de-spec'd below under
    // `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED` (`leave_forced_trip_uncharged`).
    // Asked only where the charge would otherwise be made.
    let uncharged_forced =
        !superseded && !left_for_the_interpreter && leave_forced_trip_uncharged(shared, forced);
    // JIT round 16 (lane indy, IB-3): this artifact's own IR site trap.
    let own_site_trap = !superseded
        && !left_for_the_interpreter
        && !uncharged_forced
        && stash_is_own_ir_site_trap(compiled, rframe);
    // JIT round 19 wave 3 (lane splice3, SPLICE2-2): a marked expected throw
    // is resumed above and neither charged nor de-spec'd below.
    // Wave 4 (lane splice4, SPLICE3-3): past its rate the trip is charged as
    // an unmarked one (`expected_throw_trip_uncharged`; the rate-limit switch
    // off, exactly `splice_expected_throw_trip`).
    let expected_throw = !superseded
        && !left_for_the_interpreter
        && !uncharged_forced
        && !own_site_trap
        && expected_throw_trip_uncharged(compiled, rframe);
    // JIT round 19 wave 3, lane eager3 (EAGER2-4,
    // `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`): the cause the optimizing builder
    // recorded beside the point that fired, when it recorded one (empty table
    // with the switch off). An uninitialized-`new` trap or failed class-init
    // guard of this body's own code is charged as itself
    // (`jit_bridge::charge_uninit_new_trap`), named by its point rather than
    // by its `(method, bci)`.
    let chain = !rframe.caller_frames.is_empty();
    let point_cause = if chain || superseded || left_for_the_interpreter || uncharged_forced {
        None
    } else {
        compiled.trap_cause_of_point(point_addr)
    };
    if let Some(cause) = point_cause {
        super::jit_bridge::eager_chain_census_note_point_cause(shared, cause);
    }
    // Interpreter round i4 wave 94, lane L2: an unresolved-field trap
    // (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`) is a one-shot admitted site served
    // exactly as the uninitialized-`new` one (`jit_bridge::field_site_trap_admitted`).
    let uninit_new_by_point = matches!(
        point_cause,
        Some(
            cratonvm_jit::ir::TrapCause::UninitNew | cratonvm_jit::ir::TrapCause::UnresolvedField
        )
    );
    if superseded {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] trap from a superseded artifact of {method_key} bci={} \
                 — not charged, fresh body kept",
                rframe.bci
            );
        }
    } else if left_for_the_interpreter {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: left for the interpreter-only mode, \
                 not charged",
                rframe.bci
            );
        }
    } else if uncharged_forced {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: forced chain exit \
                 (CRATONVM_DEOPT_EAGER_CHAINS), not charged, body kept",
                rframe.bci
            );
        }
    } else if expected_throw {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: expected throw in a spliced {} \
                 bci={} (CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW), not charged, body kept",
                charge_bci, rframe.method_key, rframe.bci
            );
        }
    } else if uninit_new_by_point {
        super::jit_bridge::charge_uninit_new_trap(
            shared,
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            charge_bci,
            true,
        );
    } else if own_site_trap {
        // JIT round 16 (lane indy, IB-3): the site-trap policy (memo, IR ban,
        // recompile, its own per-bci de-spec) -- see
        // `stash_is_own_ir_site_trap`. The block below is skipped for it.
        crate::jit::helpers::despeculate_trapped_method(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
            cratonvm_jit::deopt::DeoptReason::UncommonTrap,
            charge_bci,
        );
    } else {
        let reason = named
            .map(|c| c.reason)
            .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap);
        crate::jit::helpers::DeoptimizationController::deoptimize_in(
            shared,
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            reason,
            charge_bci,
        );
    }

    // JIT round 19 wave 14 (TAIL12-1, opt-in
    // `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS`): a zero divisor that keeps
    // exiting an optimizing body entered through this door re-tiers it, as
    // the call-site service's does. A single frame's opcode is its own at the
    // charge bci; a chain's is the trapping scope's, the resumed top frame's
    // (its reason the stash's own cause; the charge bci is the outer invoke).
    if !superseded && !left_for_the_interpreter && !uncharged_forced {
        let (retier_reason, retier_opcode) = if chain {
            let top = matches!(resumed, Ok(Some(CachedCallResult::FramePushed)))
                .then(|| thread.frames.last())
                .flatten()
                .and_then(|f| f.code.get(f.pc).copied());
            (
                cause
                    .map(|c| c.reason)
                    .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap),
                top,
            )
        } else {
            (
                named
                    .map(|c| c.reason)
                    .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap),
                cached.code.get(charge_bci as usize).copied(),
            )
        };
        crate::jit::helpers::retier_division_guard_trap_at_sink(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
            retier_reason,
            retier_opcode,
            charge_bci,
        );
    }

    // deopt-osr Step 9 follow-up (c): per-bci de-spec. `deoptimize` above evicts
    // the whole artifact and (on enough *aggregate* deopts) escalates to a
    // whole-method blacklist. Before that escalation can fire, give the SINGLE
    // speculation site that keeps failing a chance to be dropped on its own: once
    // THIS bci has deopted `PER_BCI_DESPEC_LIMIT` times, record `(method, bci)` in
    // THIS VM's de-spec registry (`shared.jit.despec_registry`), which every
    // compile this VM requests consults, so the next compilation suppresses just that
    // speculative guard (falling back to per-access bounds checks) and the method
    // stays compiled. A method whose deopts are spread across many bcis still
    // hits the per-method backstop; a method with one pathological site gets
    // de-spec'd there and never reaches whole-method give-up. The limit mirrors
    // HotSpot's `PerBytecodeTrapLimit`. Skipped for the superseded-artifact
    // sentinel (`bci == u32::MAX`), whose failing site was already counted on the
    // pre-supersession deopts.
    //
    // NO LONGER inert in production. This sink was `deopt-real`-only when that
    // sentence was written; since 2026-09-07 the three `jit_bridge` sinks reach
    // it through `sink_precise_resume_allowed` too, so per-bci de-spec now fires
    // on ordinary runs. That is the intended direction — one pathological
    // speculation site gets suppressed on the next compile and the method stays
    // compiled, instead of the whole method being given up.
    if !superseded
        && !left_for_the_interpreter
        && !uncharged_forced
        && !own_site_trap
        && !expected_throw
        && rframe.bci != u32::MAX
    {
        // M4 — record WHICH speculation at this bci failed, not just that
        // one did. A protected invoke publishes two points at one bytecode
        // index (a `ReceiverTypeChanged` guard and a `PendingException`
        // frame — `jit/src/osr_exit.rs` calls that the ordinary shape), and
        // under the old `(method, bci)` key four traps on either withdrew
        // BOTH. Asked through `trap_reason_at_frame` (round 11 wave 2: it
        // names the array-access NullCheck / BoundsCheck pair from the frame
        // and skips the loop-header `OsrExit` map), NOT `trap_reason_for_frame`,
        // which folds "cannot name it" into `UncommonTrap` — a real reason,
        // and one whose derived id would withdraw a speculation nothing had
        // said anything about. An undecidable bci records
        // `SPECULATION_ID_ANY`, the wildcard, the coarse behaviour this site
        // had before. Round 11 wave 5: `named` (above) prefers the stash's
        // own cause, so two guards at one non-array bci no longer fall to the
        // wildcard either.
        //
        // The LIMIT is asked at the same grain as the verdict (round 11 wave 4).
        // A named speculation is withdrawn after four traps of ITS OWN, at
        // `(reason, bci)`, which is what the charge above recorded and what
        // HotSpot's `PerBytecodeTrapLimit` counts. Summing every reason at the
        // bci let three `NullCheck` traps and one `BoundsCheck` trap withdraw
        // the bounds-check speculation, which had failed once. Only the
        // wildcard, which withdraws everything at the bci, keeps the
        // whole-bci count. The rule is `despec_site_after_trap_limit`'s (RS-3),
        // and the W3L4-4 nested-trap backstop rides with it; both in one copy
        // with the doors' re-run arms since nio21 wave 7
        // ([`despec_own_stash_site_after_charge`]). JIT round 19 wave 3 (lane
        // splice3, SPLICE2-2): a chain's implicit exception escalates to a
        // named innermost entry first, under its switch
        // (`despec_chain_site_after_trap_limit`, called by that helper; off:
        // exactly `despec_site_after_trap_limit`).
        let _ = despec_own_stash_site_after_charge(shared, &method_key, rframe, &attribution);
    }
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // close the `Deoptimizing` window on the RESUMED path only. A `None` here
    // means no frame was materialised and the caller falls back to the
    // whole-method re-run — that path leaves compiled code through the JIT
    // entry pop, which resolves the window itself
    // (`conservative_roots::leaving_compiled_state`).
    if matches!(resumed, Ok(Some(_))) {
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "interpreter::real_frame_deopt_resume_and_despeculate",
        );
    }
    resumed
}

