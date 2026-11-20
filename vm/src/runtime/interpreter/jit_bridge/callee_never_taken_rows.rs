#[allow(unused_imports)]
use super::*;

/// MEGAOSR19-2 (JIT round 19 proposal wave 4, lane megaosr4): the rows of
/// `InlineSite::callee_receiver_profile` for a body declared by
/// `declaring_class_id`: the receiver counts the profile store holds for that
/// method at each virtual (kind 0) / interface (kind 2) call pc of
/// `invoke_targets`. The profile is keyed the way the interpreter records it
/// (the executing frame's class, which is the declaring class). Empty without
/// a profile or without a profiled virtual site.
/// JIT round 23 wave 4 (lane L1, CF23-2): the callee's never-taken branch pcs
/// from its OWN method profile (`cratonvm_jit::splice_never_taken_pcs`), or
/// nothing when the class id is unknown or the callee was never profiled.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_never_taken_rows(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    code: &[u8],
    code_len: usize,
    frozen: bool,
    always_taken: bool,
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
        if false {
            eprintln!("[splice-prune] {callee_method}{callee_desc}: no profile");
        }
        return Vec::new();
    };
    let rows = if always_taken {
        cratonvm_jit::splice_always_taken_pcs_frozen(&profile, code, code_len, frozen)
    } else {
        cratonvm_jit::splice_never_taken_pcs_frozen(&profile, code, code_len, frozen)
    };
    // JIT r24 (CF residual page): the counts the prune rows were read from,
    // one line per resolved body, so a run whose arm was not pruned shows why.
    if false {
        let mut counts: Vec<(usize, u32, u32)> =
            profile.branches.iter().map(|(&pc, c)| (pc, c.taken, c.not_taken)).collect();
        counts.sort_unstable();
        eprintln!(
            "[splice-prune] {callee_method}{callee_desc}: rows {rows:?} frozen={frozen} counts (pc, taken, not) {counts:?}"
        );
    }
    rows
}

/// CF-W10-2: [`callee_never_taken_rows`]'s twin for
/// `cratonvm_jit::splice_hot_path_pcs` (the callee's profiled dominant path).
pub(in crate::runtime::interpreter::jit_bridge) fn callee_hot_path_rows(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    code: &[u8],
    code_len: usize,
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
    cratonvm_jit::splice_hot_path_pcs(&profile, code, code_len)
}

/// CF-W8-3: [`callee_never_taken_rows`]'s twin for
/// `cratonvm_jit::splice_cold_arm_ranges` (the callee's rarely taken arms).
pub(in crate::runtime::interpreter::jit_bridge) fn callee_cold_arm_rows(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    code: &[u8],
    code_len: usize,
) -> Vec<(usize, usize, u16)> {
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
    cratonvm_jit::splice_cold_arm_ranges(&profile, code, code_len)
}

pub(in crate::runtime::interpreter::jit_bridge) fn callee_receiver_profile_rows(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    invoke_targets: &[(usize, cratonvm_jit::InlineInvokeTarget)],
) -> Vec<(usize, cratonvm_jit::profile::ReceiverCounts)> {
    if declaring_class_id == 0
        || !invoke_targets
            .iter()
            .any(|(_, t)| matches!(t.invoke_kind, 0 | 2))
    {
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
    let mut rows: Vec<(usize, cratonvm_jit::profile::ReceiverCounts)> = invoke_targets
        .iter()
        .filter(|(_, t)| matches!(t.invoke_kind, 0 | 2))
        .filter_map(|&(pc, _)| {
            profile
                .receivers
                .get(&pc)
                .filter(|counts| !counts.is_empty())
                .map(|counts| (pc, counts.clone()))
        })
        .collect();
    rows.sort_unstable_by_key(|&(pc, _)| pc);
    rows.dedup_by_key(|&mut (pc, _)| pc);
    rows
}

/// j27 wave 4 (lane SPLICEFFM; `j27-splice-ffm-element-hangs-seglongloops-FIXED-20261029.md`):
/// will the optimizing planner expand the `MemorySegment` element call
/// `target` at callee pc `ipc` inside this splice? The one admission both
/// sides ask (`cratonvm_jit::ffm_element_inline::spliced_ffm_element_admission`),
/// over the receiver row [`callee_receiver_profile_rows`] hands the planner
/// for that pc, with this class store answering whether the row's class is a
/// segment class the expansion serves (`ffm_segment_class_served`).
/// `callee` is `(callee method, callee descriptor, ipc)` of the body being
/// resolved, declared by `declaring_id`, and `code` is that body.
///
/// j27 wave 4 (lane SPLICEFFM2): with the builder declines the resolver can
/// see as inputs -- the callee body for the layout operand's `getstatic`
/// (`ffm_layout_operand_getstatic`), and the JIT-readable arena table for a
/// native class ([`spliced_ffm_arena_table_published`]). Whether that
/// `getstatic`'s field has filed facts is the planner's half: its
/// `static_field_info` row is resolved only after this class-manager read
/// ends (`jit_resolve_field_site` takes its own), so every proved
/// `getstatic` counts as filed here, and a planner that finds none leaves the
/// call to the re-plan (`spliced_ffm_declined_sites`).
pub(in crate::runtime::interpreter::jit_bridge) fn spliced_ffm_element_site_admitted(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    declaring_id: ClassId,
    (callee_method, callee_desc, ipc): (&str, &str, usize),
    target: (&str, &str, &str),
    code: &[u8],
) -> bool {
    use cratonvm_jit::ffm_element_inline as fei;
    let profile = shared
        .jit
        .profile_store
        .get_profile(&crate::jit::profile::MethodKey {
            class_id: declaring_id.as_u32(),
            method_name: callee_method.into(),
            descriptor: callee_desc.into(),
        });
    let counts = profile.as_ref().and_then(|p| p.receivers.get(&ipc));
    let call = fei::SplicedFfmCall {
        target,
        code,
        pc: ipc,
    };
    let segment_class = |class_id: u32| {
        let cid = ClassId::new(class_id);
        let found = cm.get_class(cid)?;
        let bootstrap = matches!(
            cm.get_loader_id(cid),
            Some(cratonvm_types::ClassLoaderId::Bootstrap)
        );
        fei::ffm_segment_class_served(&found.name, bootstrap, found.origin.has_real_bytes())
            .then(|| ((), fei::ffm_native_segment_class(&found.name)))
    };
    fei::spliced_ffm_element_admission(
        call,
        counts,
        spliced_ffm_arena_table_published(),
        segment_class,
        |_| true,
    )
    .is_some()
}

/// j27 wave 4 (lane SPLICEFFM2): does this process publish the JIT-readable
/// `Unsafe` arena table a spliced NATIVE segment access needs? The address
/// `jit_well_known_class_id` answers for
/// `cratonvm_jit::ir_lower::WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE` (x86-64 only),
/// judged by `ffm_arena_table_published` as the planner's
/// `scoped_arena_jit_table` judges it.
pub(in crate::runtime::interpreter::jit_bridge) fn spliced_ffm_arena_table_published() -> bool {
    let addr = if cfg!(target_arch = "x86_64") {
        cratonvm_native_builtins::unsafe_arena_jit_table_addr()
    } else {
        0
    };
    cratonvm_jit::ffm_element_inline::ffm_arena_table_published(u64::try_from(addr).unwrap_or(0))
}

// Whether a method reached by walking up from the RUNTIME RECEIVER is the
/// method real dispatch would select at a site declared against `cp_class_id`.
///
/// `find_method_recursive` implements JVMS selection, but selection is only
/// defined relative to a resolved method: a candidate overrides the resolved
/// method only if it is accessible to it (JVMS §5.4.5). A package-private
/// method in a *different* runtime package has the same name and descriptor and
/// is NOT an override — dispatch runs the resolved method, the walk finds the
/// impostor. Everything below is a fail-closed check for that class of
/// disagreement; a `false` answer means "do not inline", never "inline
/// something else".
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn receiver_resolution_is_dispatch_faithful(
    store: &crate::classloading::ClassStore,
    receiver_id: ClassId,
    cp_class_id: ClassId,
    declaring_id: ClassId,
    method: &cratonvm_reader::method::ClassFileMethod,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    // A virtual/interface site never dispatches to a static method, and a
    // private method is never inherited — a walk that reached one from a
    // subclass receiver found something dispatch could not.
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if method.is_static()
        || method.access_flags.contains(MethodAccessFlags::PRIVATE)
        || method.is_abstract()
    {
        return false;
    }
    // `<init>`/`<clinit>` are not virtually dispatched at all.
    if callee_method.starts_with('<') {
        return false;
    }
    // The receiver must actually be a subtype of the declared class, or the
    // profile handed us a class id from a different site entirely.
    if !class_is_assignable_to(store, receiver_id, cp_class_id) {
        return false;
    }
    // Is the selected method genuinely an OVERRIDE of what the constant-pool
    // reference resolves to? Two sufficient conditions, both answerable from
    // what is already in hand — deliberately NOT by resolving the CP reference
    // as well. `vm/src/runtime/resolve/guard.rs` ratchets the interpreter's
    // metadata-table bypass budget downward and nothing raises it; a second
    // `find_method_recursive` here would, and the cheaper rules below cost
    // only reach, never correctness.
    //
    //  1. `public`/`protected` overrides anything with the same name and
    //     descriptor it inherits, in any package (JVMS §5.4.5).
    //  2. Otherwise, the method must be declared on the CONSTANT-POOL CLASS
    //     itself — in which case CP resolution stops there and the selected
    //     method IS the resolved method, so there is no override question to
    //     answer.
    //
    // What this refuses is the remaining shape: a PACKAGE-PRIVATE method found
    // by walking up from the receiver, declared somewhere other than the CP
    // class. It may or may not override — a package-private method in a
    // different runtime package does NOT (dispatch runs the resolved method,
    // while the walk found the impostor) — and telling those apart needs the
    // CP-side resolution this deliberately does without. Refusing costs a
    // package-private virtual site its inline; guessing costs a wrong body.
    if method
        .access_flags
        .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
    {
        return true;
    }
    if declaring_id == cp_class_id {
        return true;
    }
    // Round 16 wave 13 (lane mega, MG16-3 of `jit-r16-mega-proposals-RETIRED-20261006.md`):
    // the refused shape asked the CP-side resolution instead of guessing. A
    // package-private method the walk found below the CP class is the body
    // dispatch runs exactly when JVMS §5.4.6 selection from this receiver,
    // against the method the CP class resolves to, selects that same
    // declaration -- selection applies §5.4.5's runtime-package rule (same
    // defining loader and package name) and the store's cross-loader policy.
    // Any other answer (another class, an error, `Lenient`) keeps the refusal.
    // `R16MegaOneSite` virt2 is the shape: a package-private `eval` overridden
    // in the same package, whose bimorphic splice was refused as
    // `first-body-refused`. Default ON since JIT round 20 (`=0` kills; A/B on
    // R16MegaOneSite: ent-virt / osr-virt 5.9 -> 1.9 ns, no row lost), read per
    // resolution.
    {
        use crate::runtime::resolve::selection::{self, Selection};
        let Some(resolved) =
            selection::resolved_ref_for_owner(store, cp_class_id, callee_method, callee_desc)
        else {
            return false;
        };
        match selection::select(store, receiver_id, resolved, callee_method, callee_desc) {
            Selection::Selected(id) => id == declaring_id,
            // JIT round 21 wave 3 (lane splice): the strict walk declines to
            // judge (`Lenient`: a class or interface of the receiver's
            // hierarchy without real bytes, or a resolved method it cannot
            // read); the superclass chain alone may still decide it.
            Selection::Lenient if guarded_splice_pp_class_chain_enabled() => {
                package_private_override_by_class_chain(
                    store,
                    receiver_id,
                    cp_class_id,
                    declaring_id,
                    method,
                    callee_method,
                    callee_desc,
                )
            }
            _ => false,
        }
    }
}

/// `CRATONVM_JIT_GUARDED_SPLICE_PP_CLASS_CHAIN` (JIT round 21 wave 3, lane
/// splice; perf, opt-in): when JVMS selection answers `Lenient` for a guarded
/// splice of a package-private override, decide it by
/// [`package_private_override_by_class_chain`]. Read per resolution (compile
/// time only).
pub(in crate::runtime::interpreter::jit_bridge) fn guarded_splice_pp_class_chain_enabled() -> bool {
    false
}

/// Is the package-private `method`, declared by `declaring_id` and found by
/// the lenient walk from `receiver_id`, the method JVMS 5.4.6 selects for a
/// `Methodref` the constant-pool class `cp_class_id` resolves -- decided on
/// the receiver's SUPERCLASS chain alone?
///
/// For a `Methodref` resolved to a class method `mA` of `A`, selection step 2
/// walks the receiver's superclasses and takes the first method that can
/// override `mA` (5.4.5); interfaces only matter when no class method is
/// found (step 3), which the lenient walk has already excluded here. So the
/// strict walk's whole-hierarchy `has_real_bytes` screen (which an interface
/// such as a stand-in `Future` fails, the suspected `tryFire` refusal of the
/// R21CfChain census) is stronger than this question needs. This asks, every
/// class from the receiver up to `A` having real bytes:
///
/// * `A` is on the receiver's superclass chain and `mA` is neither static nor
///   private;
/// * the FIRST non-static, non-private declaration of the name and descriptor
///   on that chain, from the receiver, is `declaring_id`'s (a static or
///   private one never overrides and selection walks past it, as here);
/// * that declaration can override `mA` (`method_override::can_override`, the
///   predicate selection itself uses, run-time package and transitive clause
///   included).
///
/// Any other shape (a nearer declaration that does not override, which
/// selection would walk past, an abstract one, a class without real bytes)
/// answers `false`: conservative only.
pub(in crate::runtime::interpreter::jit_bridge) fn package_private_override_by_class_chain(
    store: &crate::classloading::ClassStore,
    receiver_id: ClassId,
    cp_class_id: ClassId,
    declaring_id: ClassId,
    method: &cratonvm_reader::method::ClassFileMethod,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let private = |m: &cratonvm_reader::method::ClassFileMethod| {
        m.access_flags.contains(MethodAccessFlags::PRIVATE)
    };
    if method.is_static() || method.is_abstract() || private(method) {
        return false;
    }
    let Some(a_id) = crate::runtime::resolve::selection::resolve_declaring(
        store,
        cp_class_id,
        callee_method,
        callee_desc,
    ) else {
        return false;
    };
    let Some(a) = store.get(a_id) else {
        return false;
    };
    if a.is_interface() || !a.origin.has_real_bytes() {
        return false;
    }
    let Some(ma) = a.find_method(callee_method, callee_desc) else {
        return false;
    };
    if ma.is_static() || private(ma) {
        return false;
    }
    let mut first: Option<ClassId> = None;
    let mut cur = Some(receiver_id);
    let mut steps = 0usize;
    let mut reached_a = false;
    while let Some(id) = cur {
        steps += 1;
        if steps > 512 {
            return false;
        }
        let Some(class) = store.get(id) else {
            return false;
        };
        if !class.origin.has_real_bytes() {
            return false;
        }
        if first.is_none() {
            if let Some(m) = class.find_method(callee_method, callee_desc) {
                if !m.is_static() && !private(m) {
                    first = Some(id);
                }
            }
        }
        if id == a_id {
            reached_a = true;
            break;
        }
        cur = class.superclass;
    }
    if !reached_a || first != Some(declaring_id) {
        return false;
    }
    // The first declaration IS the resolved method: selected as resolved.
    if declaring_id == a_id {
        return true;
    }
    let mut budget = 512usize;
    cratonvm_classloading::method_override::can_override(
        store,
        method,
        declaring_id,
        ma,
        a_id,
        &mut budget,
    )
}

/// JIT round 21 wave 3 (lane splice): WHICH clause of
/// [`receiver_resolution_is_dispatch_faithful`] refused, for the
/// `CRATONVM_DBG_JITC` refusal line only (built inside the `no!` macro's
/// debug branch, never on a quiet run). The R21CfChain census read 32
/// `CompletableFuture$Completion.tryFire` refusals as one reason; the
/// package-private `tryFire` overrides make the selection answer the
/// interesting half (`Lenient` = a hierarchy the strict walk will not judge).
/// Mirrors the predicate's order; answers "faithful" if none fires, which
/// would mean the two have drifted.
pub(in crate::runtime::interpreter::jit_bridge) fn receiver_resolution_unfaithful_clause(
    store: &crate::classloading::ClassStore,
    receiver_id: ClassId,
    cp_class_id: ClassId,
    declaring_id: ClassId,
    method: &cratonvm_reader::method::ClassFileMethod,
    callee_method: &str,
    callee_desc: &str,
) -> String {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if method.is_static() {
        return "selected method is static".to_string();
    }
    if method.access_flags.contains(MethodAccessFlags::PRIVATE) {
        return "selected method is private".to_string();
    }
    if method.is_abstract() {
        return "selected method is abstract".to_string();
    }
    if callee_method.starts_with('<') {
        return "initializer".to_string();
    }
    if !class_is_assignable_to(store, receiver_id, cp_class_id) {
        return format!(
            "receiver {} is not a subtype of the constant-pool class {}",
            receiver_id.as_u32(),
            cp_class_id.as_u32()
        );
    }
    if method
        .access_flags
        .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
        || declaring_id == cp_class_id
    {
        return "faithful (clauses drifted)".to_string();
    }
    use crate::runtime::resolve::selection;
    match selection::resolved_ref_for_owner(store, cp_class_id, callee_method, callee_desc) {
        None => "package-private override, the constant-pool class resolves no method".to_string(),
        Some(resolved) => format!(
            "package-private override, JVMS selection from {} answers {:?} for {:?}",
            receiver_id.as_u32(),
            selection::select(store, receiver_id, resolved, callee_method, callee_desc),
            resolved,
        ),
    }
}

/// Whether `sub` is `sup` or inherits/implements it.
pub(in crate::runtime::interpreter::jit_bridge) fn class_is_assignable_to(
    store: &crate::classloading::ClassStore,
    sub: ClassId,
    sup: ClassId,
) -> bool {
    if sub == sup {
        return true;
    }
    let mut stack = vec![sub];
    let mut seen: std::collections::HashSet<ClassId> = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if id == sup {
            return true;
        }
        if !seen.insert(id) {
            continue;
        }
        let Some(class) = store.get(id) else {
            continue;
        };
        stack.extend(class.interfaces.iter().copied());
        if let Some(sc) = class.superclass {
            stack.push(sc);
        }
    }
    false
}

/// Instruction length for the inline-eligibility byte walk: the JIT's shared
/// decoder, `wide` forms included. The eligibility scan refuses
/// `tableswitch`/`lookupswitch` before it ever asks for a length.
#[inline]
pub(in crate::runtime::interpreter) fn inline_instr_length(code: &[u8], pc: usize) -> usize {
    cratonvm_jit::bytecode_insn_len(code, pc)
}

/// Extract the array index from an AIOOBE panic message.
pub fn parse_aioobe_index(msg: &str) -> i32 {
    // Format: "ArrayIndexOutOfBoundsException: index N out of bounds for length M"
    if let Some(rest) = msg.strip_prefix("ArrayIndexOutOfBoundsException: index ") {
        if let Some(idx_str) = rest.split_whitespace().next() {
            if let Ok(idx) = idx_str.parse::<i32>() {
                return idx;
            }
        }
    }
    -1
}

/// Reconstruct a JIT'd method's incoming locals (`this` + declared params) as
/// `Value`s from the bit-exact `(CompactValue, kind)` arg slots that
/// `execute_jit_call` saved before dispatch. Used only on the cold
/// exception-routing path to repopulate the handler frame's locals (see
/// `route_jit_exception_through_method`). Mirrors the descriptor-aware decode
/// of the dispatch pop loop: receiver slot is `L`, the rest decode by the
/// method descriptor's parameter tags.
pub(in crate::runtime::interpreter) fn jit_saved_args_to_values(
    cached: &CachedBytecodeMethod,
    saved_args: &[(CompactValue, u8)],
    np: usize,
) -> Vec<Value> {
    let is_static = cached.is_static;
    let mut out = Vec::with_capacity(np);
    // ONE forward scan, hoisted out of this per-argument loop. Always the scan
    // (no `SharedVm` here): it answers exactly what the kill switch's rescan
    // answers (`param_tags_match_nth_param_tag_byte`), on a cold path.
    let param_tags =
        ParamTags::for_method(cached, true, !cratonvm_jit_api::descriptor_facts_disabled());
    for i in 0..np {
        let (cv, kind) = saved_args[i];
        let desc_byte = if is_static {
            param_tags.get(&cached.method_descriptor, i)
        } else {
            param_tags.get_with_receiver(&cached.method_descriptor, i)
        };
        out.push(decode_arg_kind_aware(cv, kind, desc_byte));
    }
    out
}

/// Roots the reference arguments `execute_jit_call` popped off the caller's
/// operand stack, for the whole native activation.
///
/// Once popped they are no longer interpreter roots, yet the saved copies are
/// used AFTER the compiled call returns: re-pushed for a whole-method re-run,
/// and decoded into handler locals after an exception object was allocated.
/// The call itself can collect, so a moving young collection handed those
/// paths pre-move addresses. Each reference is pushed into `native_pin_roots`
/// as it is popped (its index in `arg_pins`, `usize::MAX` for a
/// non-reference) and re-read through [`pinned_saved_arg`]; this guard
/// releases the window on every return path. It is declared before
/// `JitSynchronizedMonitorGuard`, whose pin sits above this window, so that
/// guard drops first.
pub(in crate::runtime::interpreter) struct JitArgPinGuard {
    pub(in crate::runtime::interpreter) thread: *mut JvmThread,
    pub(in crate::runtime::interpreter) base: usize,
}

impl Drop for JitArgPinGuard {
    fn drop(&mut self) {
        // SAFETY: `thread` came from the exclusive `&mut JvmThread` of the
        // enclosing activation, which outlives this guard — the same aliasing
        // discipline `JitSynchronizedMonitorGuard` relies on.
        let thread = unsafe { &mut *self.thread };
        if thread.native_pin_roots.len() > self.base {
            thread.native_pin_roots.truncate(self.base);
        }
    }
}

/// gch h2/fj (2026-10-08; `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`
/// P3, the door `fr1_argsp_census_*` named: the judged `drop*` callee is a
/// compiled body entered from an interpreted caller, its argument held only by
/// this door's `native_pin_roots` window; `gch-h1fr-proposal-owned-body-argument-pins-remap-but-do-not-root`):
/// the reference arguments of a door's call held as WEAK global handles of
/// this VM's JNI table instead of strong pins, for a callee body that owns its
/// arguments (`cratonvm_jit::callee_body_owns_its_arguments`). Every collector
/// already remaps those handles after a move and clears them when the referent
/// died (`jni::sweep_weak_global_refs_with` in every pause, the concurrent
/// remarks and sweeps), which is exactly the remap-only list the proposal
/// asked for, with no new collector hook. The callee's own frame roots each
/// argument while it uses it; one it dropped can now be collected, as on
/// HotSpot. Opt-in (`CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK`, per VM).
///
/// [`Self::take`] converts the door's pin window (it must be the top of the
/// pin stack, and the thread must run no native: nothing can collect between
/// the release and the callee's prologue, the window
/// `vm_exec::PinnedDispatchArgs::release_before_rooting_tail` also relies on);
/// [`Self::repin`] puts the survivors back into `native_pin_roots` for the
/// door's post-call readers and names the cleared ones; `Drop` deletes the
/// handles on every path.
pub(in crate::runtime::interpreter) struct WeakDoorArgs<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) shared: &'a SharedVm,
    /// `(argument index, weak handle)`.
    pub(in crate::runtime::interpreter::jit_bridge) handles: smallvec::SmallVec<[(usize, crate::native::jni::JObject); 8]>,
    /// Exact duplicate pins held by the enclosing by-name dispatch guard.
    pub(in crate::runtime::interpreter::jit_bridge) weak_dispatch_pin_slots: smallvec::SmallVec<[usize; 8]>,
}

impl<'a> WeakDoorArgs<'a> {
    /// Swap the pin window `native_pin_roots[base..]` that holds the door's
    /// pinned arguments (`arg_pins[i]`, `usize::MAX` for a non-reference) for
    /// weak handles, and release the window. `None`, changing nothing, when
    /// no argument is pinned, the window is not exactly the top of the pin
    /// stack, or the thread runs a native.
    pub(in crate::runtime::interpreter) fn take(
        shared: &'a SharedVm,
        thread: &mut JvmThread,
        base: usize,
        arg_pins: &[usize],
    ) -> Option<Self> {
        let pinned = arg_pins.iter().filter(|&&pin| pin != usize::MAX).count();
        if pinned == 0
            || thread.native_pin_roots.len() != base + pinned
            || arg_pins.iter().any(|&pin| pin != usize::MAX && pin < base)
            || thread.running_native != 0
            || thread.native_excuse_raised != 0
        {
            return None;
        }
        let mut handles = smallvec::SmallVec::new();
        let mut weak_dispatch_pin_slots = smallvec::SmallVec::new();
        {
            let mut refs = shared.natives.jni_global_refs.lock();
            for (i, &pin) in arg_pins.iter().enumerate() {
                if pin == usize::MAX {
                    continue;
                }
                if let Some(obj) = thread.native_pin_roots.get(pin).copied() {
                    handles.push((i, refs.add_weak(obj)));
                }
            }
        }
        if let Some((dispatch_base, count)) = thread.by_name_dispatch_pin_window {
            if let Some(end) = dispatch_base.checked_add(count) {
                let mut arg_objects = arg_pins.iter().filter_map(|&pin| {
                    (pin != usize::MAX)
                        .then(|| thread.native_pin_roots.get(pin).map(|obj| obj.as_ptr() as usize))
                        .flatten()
                });
                let matches = end <= base
                    && count <= handles.len()
                    && (0..count).all(|offset| {
                        thread
                            .native_pin_roots
                            .get(dispatch_base + offset)
                            .is_some_and(|pin| Some(pin.as_ptr() as usize) == arg_objects.next())
                    })
                    && arg_objects.next().is_none();
                if matches {
                    weak_dispatch_pin_slots.extend(dispatch_base..end);
                }
            }
        }
        thread.native_pin_roots.truncate(base);
        Some(Self {
            shared,
            handles,
            weak_dispatch_pin_slots,
        })
    }

    /// Re-pin every argument whose referent is still alive (pushed onto
    /// `native_pin_roots`, its index written to `arg_pins[i]`, so the door's
    /// existing readers -- `pinned_saved_arg`, `pinned_decoded_args`, the
    /// write-backs -- read it at its current address) and answer the indexes
    /// whose referent was collected (`arg_pins[i] = usize::MAX`: the caller
    /// must null its own copy of those before any reader runs).
    ///
    /// gch h3/fd: resolving a weak handle back into a strong pin is a
    /// KEEP-ALIVE, as every other weak-global resolve in the VM is
    /// (`jni::jobject_to_obj`, gc-common w8-c): a concurrent mark (the
    /// Generational concurrent old cycle, G1, ZGC) that started while the
    /// argument was held only weakly may not have reached it, and a pin made
    /// from the bare `resolve` would then name an object that cycle frees. One
    /// SATB enqueue per survivor (inert when no cycle marks), outside the
    /// table lock (the barrier may flush into the collector's queue).
    pub(in crate::runtime::interpreter) fn repin(
        &self,
        thread: &mut JvmThread,
        arg_pins: &mut [usize],
    ) -> smallvec::SmallVec<[usize; 8]> {
        let mut cleared = smallvec::SmallVec::new();
        let mut survivors: smallvec::SmallVec<[ObjectRef; 8]> = smallvec::SmallVec::new();
        {
            let refs = self.shared.natives.jni_global_refs.lock();
            for &(i, handle) in &self.handles {
                let Some(slot) = arg_pins.get_mut(i) else {
                    continue;
                };
                match refs.resolve(handle) {
                    Some(obj) => {
                        *slot = thread.native_pin_roots.len();
                        thread.native_pin_roots.push(obj);
                        survivors.push(obj);
                    }
                    None => {
                        *slot = usize::MAX;
                        cleared.push(i);
                    }
                }
            }
        }
        for obj in survivors {
            self.shared.mem.heap.referent_keep_alive(Value::Object(Some(obj)));
        }
        cleared
    }
}

impl Drop for WeakDoorArgs<'_> {
    fn drop(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        let mut refs = self.shared.natives.jni_global_refs.lock();
        for &(_, handle) in &self.handles {
            let _ = refs.remove_weak(handle);
        }
    }
}

/// gch h3/fd (2026-10-08; `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`
/// P2): [`WeakDoorArgs`] for `interpreter::execute`'s first-call door, whose
/// arguments are held by an `InvokeArgsRootGuard` (one pin per non-null
/// reference argument, read back BY POSITION: `pin_base + ordinal`) rather
/// than by an `arg_pins` index table. Opt-in
/// (`CRATONVM_JIT_FIRST_CALL_DOOR_ARGS_WEAK`, per VM), for a body that owns
/// its arguments, never for a synchronized method.
///
/// [`Self::take`] releases the guard's window under the same conditions as
/// [`WeakDoorArgs::take`]; [`Self::restore`] puts the survivors back so the
/// guard's positional reads stay exact: a collected argument is NULLED in the
/// argument buffer (the guard then skips it) and the guard's count shrinks.
pub(in crate::runtime::interpreter) struct WeakExecuteArgs<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) weak: WeakDoorArgs<'a>,
    /// Per argument: its pin index in the guard's window, `usize::MAX` for a
    /// non-reference or null argument.
    pub(in crate::runtime::interpreter::jit_bridge) arg_pins: smallvec::SmallVec<[usize; 8]>,
}

impl<'a> WeakExecuteArgs<'a> {
    /// `None`, changing nothing, unless `args` is exactly what `guard` pinned
    /// (one pin per non-null reference, in order) and [`WeakDoorArgs::take`]
    /// accepts the window.
    pub(in crate::runtime::interpreter) fn take(
        shared: &'a SharedVm,
        thread: &mut JvmThread,
        guard: &super::field_access::InvokeArgsRootGuard,
        args: &[Value],
    ) -> Option<Self> {
        let mut arg_pins = smallvec::SmallVec::with_capacity(args.len());
        let mut ordinal = 0usize;
        for value in args {
            if matches!(value, Value::Object(Some(_))) {
                arg_pins.push(guard.pin_base + ordinal);
                ordinal += 1;
            } else {
                arg_pins.push(usize::MAX);
            }
        }
        if ordinal != guard.object_count {
            return None;
        }
        let weak = WeakDoorArgs::take(shared, thread, guard.pin_base, &arg_pins)?;
        Some(Self { weak, arg_pins })
    }

    /// Per argument, in order: not `usize::MAX` iff it is a non-null
    /// reference this door handed over weakly (gci i14/od: the promising
    /// shim's record, `run_jit_body_raw_promised`). Only the marker is
    /// meaningful here; the indexes name the released window.
    pub(in crate::runtime::interpreter) fn handed_arg_pins(&self) -> &[usize] {
        &self.arg_pins[..]
    }

    pub(in crate::runtime::interpreter) fn weak_dispatch_pin_slots(&self) -> &[usize] {
        &self.weak.weak_dispatch_pin_slots
    }

    /// Re-pin the surviving arguments at the guard's window, in argument
    /// order, null each collected one in `args`, and shrink the guard's count
    /// to match; answer whether any argument was collected (a re-run from
    /// entry is then inexact: the caller must refuse it). Pins a callee left
    /// above the window (none are expected: every door restores its own
    /// watermark) are kept above it.
    pub(in crate::runtime::interpreter) fn restore(
        mut self,
        thread: &mut JvmThread,
        guard: &mut super::field_access::InvokeArgsRootGuard,
        args: &mut [Value],
    ) -> bool {
        let base = guard.pin_base;
        if thread.native_pin_roots.len() < base {
            // Someone truncated below this door's watermark: no positional
            // read of the window can be trusted any more. Every reference
            // argument is answered as lost (the strong guard would have read
            // past the end here).
            debug_assert!(
                false,
                "pin stack truncated below a first-call door's window"
            );
            for value in args.iter_mut() {
                if matches!(value, Value::Object(Some(_))) {
                    *value = Value::Object(None);
                }
            }
            guard.object_count = 0;
            return true;
        }
        let above = thread.native_pin_roots.split_off(base);
        let cleared = self.weak.repin(thread, &mut self.arg_pins);
        for &i in &cleared {
            if let Some(value) = args.get_mut(i) {
                *value = Value::Object(None);
            }
        }
        guard.object_count = thread.native_pin_roots.len() - base;
        thread.native_pin_roots.extend(above);
        !cleared.is_empty()
    }
}

/// The InternalError `execute`'s first-call door raises instead of re-running
/// from entry a body whose owned argument was collected under
/// [`WeakExecuteArgs`] (gch h3/fd, d9d P2): the twin of
/// [`refuse_rerun_with_a_collected_owned_arg`] for a door with no cached
/// method.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn refuse_first_call_rerun_with_a_collected_owned_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallFailed {
    let message = format!(
        "precise deoptimization unavailable for {class_name}.{method_name}{descriptor} \
         (door execute-first-call): an argument its body owned was collected before the \
         re-run from entry; refusing an inexact replay",
    );
    crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::InternalError { message },
    )
}





/// The InternalError a door raises instead of re-running from entry a body
/// whose owned argument was collected under [`WeakDoorArgs`] (the callee
/// dropped it, then its stash was declined or it took a frameless trap): the
/// entry state no longer exists, so a re-run cannot be exact. Expected never
/// to fire for an owning body (no frameless stub; a declined stash is rare).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn refuse_rerun_with_a_collected_owned_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    door: &str,
) -> MethodCallFailed {
    let message = format!(
        "precise deoptimization unavailable for {}.{}{} (door {door}): an argument its body \
         owned was collected before the re-run from entry; refusing an inexact replay",
        cached.class_name, cached.method_name, cached.method_descriptor,
    );
    crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::InternalError { message },
    )
}

/// Saved argument `i`, with a pinned reference re-read at its current
/// (possibly relocated) address. See [`JitArgPinGuard`].
pub(in crate::runtime::interpreter) fn pinned_saved_arg(
    thread: &JvmThread,
    saved_args: &[(CompactValue, u8)],
    arg_pins: &[usize],
    i: usize,
) -> (CompactValue, u8) {
    let (cv, kind) = saved_args[i];
    match arg_pins
        .get(i)
        .and_then(|&pin| thread.native_pin_roots.get(pin))
    {
        // Cast: heap address to the compact reference encoding.
        Some(obj) => (CompactValue::object(obj.as_ptr() as usize as u64), kind),
        None => (cv, kind),
    }
}

/// [`jit_saved_args_to_values`] over the saved arguments with every pinned
/// reference re-read first.
pub(in crate::runtime::interpreter) fn jit_saved_args_to_values_pinned(
    cached: &CachedBytecodeMethod,
    saved_args: &[(CompactValue, u8)],
    np: usize,
    thread: &JvmThread,
    arg_pins: &[usize],
) -> Vec<Value> {
    let mut current = saved_args.to_vec();
    for (i, slot) in current.iter_mut().enumerate().take(np) {
        *slot = pinned_saved_arg(thread, saved_args, arg_pins, i);
    }
    jit_saved_args_to_values(cached, &current, np)
}

/// The [`JitArgPinGuard`] window for the two doors that receive their
/// arguments already decoded (`execute_jit_call_decoded`,
/// `execute_jit_call_oneshot`).
///
/// Pushes every non-null reference in `args` onto `native_pin_roots`, records
/// its pin index in `arg_pins[i]` (`usize::MAX` for anything else), and
/// returns the guard that releases the window. Read the arguments back through
/// [`pinned_decoded_args`].
///
/// Those doors used to hand the pre-call `args` straight to the exception
/// routers as the handler frame's locals. `args` is a Rust-side copy the
/// collector cannot see, and the compiled call between the decode and that
/// read can run a moving collection, so the handler frame was built with
/// pre-move addresses — the defect `JitArgPinGuard` already closed for
/// `execute_jit_call`, which is the one door that popped its own arguments.
pub(in crate::runtime::interpreter) fn pin_decoded_jit_args(
    thread: &mut JvmThread,
    args: &[Value],
    arg_pins: &mut [usize],
) -> JitArgPinGuard {
    let base = thread.native_pin_roots.len();
    for (i, v) in args.iter().enumerate() {
        if let Value::Object(Some(obj)) = v {
            if let Some(pin) = arg_pins.get_mut(i) {
                *pin = thread.native_pin_roots.len();
                thread.native_pin_roots.push(*obj);
            }
        }
    }
    JitArgPinGuard {
        thread: thread as *mut JvmThread,
        base,
    }
}

/// `args` with every reference [`pin_decoded_jit_args`] pinned re-read at its
/// current (possibly relocated) address. Non-references, nulls, and any slot
/// without a live pin come back unchanged.
pub(in crate::runtime::interpreter) fn pinned_decoded_args(
    thread: &JvmThread,
    args: &[Value],
    arg_pins: &[usize],
) -> Vec<Value> {
    args.iter()
        .enumerate()
        .map(|(i, v)| {
            let pinned = arg_pins
                .get(i)
                .and_then(|&pin| thread.native_pin_roots.get(pin));
            match (v, pinned) {
                (Value::Object(Some(_)), Some(obj)) => Value::Object(Some(*obj)),
                _ => *v,
            }
        })
        .collect()
}

/// Write the pinned (possibly relocated) reference arguments back into the
/// CALLER's decoded argument buffer — the in-place sibling of
/// [`pinned_decoded_args`].
///
/// r9w2-vm2: the decoded doors return `Ok(None)` ("declined") on two paths
/// that follow the compiled call — a deopt with no resumable frame, and the
/// `deopt_signaled` sentinel — and every caller then builds the interpreted
/// frame from the SAME buffer it passed in. That buffer is a Rust-side copy
/// the collector cannot see, so a moving collection inside the compiled body
/// left it naming from-space addresses. Called immediately before each such
/// post-call `return Ok(None)`, while the [`JitArgPinGuard`] window is still
/// open. Non-references, nulls and slots without a live pin are untouched.
pub(in crate::runtime::interpreter) fn write_back_pinned_decoded_args(
    thread: &JvmThread,
    dst: &mut [Value],
    arg_pins: &[usize],
) {
    for (i, v) in dst.iter_mut().enumerate() {
        let pinned = arg_pins
            .get(i)
            .and_then(|&pin| thread.native_pin_roots.get(pin));
        if let (Value::Object(Some(_)), Some(obj)) = (*v, pinned) {
            *v = Value::Object(Some(*obj));
        }
    }
}

/// Dispatch-free entries that came back with a throwable stashed.
///
/// A body with `has_dispatch == false` is entered through a door that skips
/// `catch_unwind`, and that door used to ignore `sig.exception` altogether.
/// `take_all_jit_signals` TAKES it, so a throwable stashed by a helper the
/// compiler's `has_dispatch` list did not anticipate was dropped on the floor:
/// the method returned its sentinel or its last value as though it had
/// completed normally. That list (`jit/src/x64/driver.rs`, and the IR tier's
/// own arms in `jit/src/lib.rs`) has been wrong at least six times — the
/// `Character.getType`, `<clinit>`, indy, athrow, checkcast and IR `Op::Throw`
/// entries each record one — so every door now routes the exception whatever
/// the flag says, and counts the event, because a nonzero count names a
/// `has_dispatch` omission for the JIT crate to fix.
pub(in crate::runtime::interpreter::jit_bridge) static FAST_ENTRY_STRANDED_EXCEPTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Record that a dispatch-free entry returned with a pending throwable (which
/// the caller then routes). Warns once per process, naming the method, so the
/// missing `has_dispatch` trigger can be found without a debugger.
pub(in crate::runtime::interpreter::jit_bridge) fn note_fast_entry_stranded_exception(name: JitBodyName<'_>) {
    let prior = FAST_ENTRY_STRANDED_EXCEPTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if prior == 0 {
        tracing::warn!(
            "JIT: dispatch-free compiled method {}.{}{} returned with a pending \
             exception; routed it, but the compiler's has_dispatch list is missing \
             whatever stashed it",
            name.class_name,
            name.method_name,
            name.descriptor,
        );
    }
}

/// The identity of the method a compiled body belongs to, as the post-return
/// drain needs it: only for the once-per-process stranded-exception warning.
/// Borrowed, so a door that holds no [`CachedBytecodeMethod`] (`execute`'s
/// first-call door) need not build one per call to share [`run_jit_body_raw`].
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter) struct JitBodyName<'a> {
    pub(in crate::runtime::interpreter) class_name: &'a str,
    pub(in crate::runtime::interpreter) method_name: &'a str,
    pub(in crate::runtime::interpreter) descriptor: &'a str,
}

impl<'a> JitBodyName<'a> {
    #[inline]
    pub(in crate::runtime::interpreter::jit_bridge) fn of(cached: &'a CachedBytecodeMethod) -> Self {
        Self {
            class_name: &cached.class_name,
            method_name: &cached.method_name,
            descriptor: &cached.method_descriptor,
        }
    }
}

/// Owns the implicit monitor of a JIT-entered `ACC_SYNCHRONIZED` method.
///
/// Compiled code has no interpreter frame on which to keep `monitor_on_exit`,
/// so the monitor must instead be rooted explicitly for the complete native
/// activation and released on every Rust return path. The native-pin slot is
/// retained after acquisition because a moving collection may update it before
/// `Drop` performs the matching implicit `monitorexit`.
///
/// A thin JIT-facing wrapper: the pin, the release and the frame handoff are
/// `vm_exec::SynchronizedMethodGuard`, the one guard every synchronized-method
/// door shares.
pub(in crate::runtime::interpreter) struct JitSynchronizedMonitorGuard(pub(in crate::runtime::interpreter::jit_bridge) crate::vm::vm_exec::SynchronizedMethodGuard);

impl JitSynchronizedMonitorGuard {
    pub(in crate::runtime::interpreter) fn acquire(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cached: &CachedBytecodeMethod,
        args: &mut [Value],
    ) -> Result<Self, MethodCallFailed> {
        // Round 15 wave 4 (lane sync3, SY2-2): this thread's first mint.
        let mut minted = false;
        let monitor = if cached.is_static {
            // gen r5w1/oom5: first use on a full heap collects (pinning and
            // refreshing `args`) and throws `OutOfMemoryError`; every caller
            // re-reads its own argument pins after this returns.
            let mirror = super::constants::static_sync_monitor_or_oom(
                shared,
                thread,
                cached.declaring_class_id,
                args,
            )?;
            // Round 13 wave 11 (lane sync7): the class's GC-maintained mirror
            // slot, so the method's upgrade compile can be a self-locking body
            // (`CompileRequest::self_lock_mirror_slot`). Minted here, where a
            // compiled body of the method exists, and not by the interpreter's
            // doors: a minted slot roots the mirror and its loader for the
            // VM's life. Nothing collects between the read and the mint.
            if crate::runtime::env_cache::jit_self_locking_sync_static() {
                minted = crate::jit::helpers::mint_static_sync_mirror_slot(
                    shared,
                    cached.declaring_class_id,
                    mirror,
                );
            }
            mirror
        } else {
            match args.first() {
                Some(Value::Object(Some(obj))) => *obj,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "JIT entered synchronized instance method without this"
                            .to_string(),
                    }))
                }
            }
        };
        let guard = Self(crate::vm::vm_exec::SynchronizedMethodGuard::enter(
            shared, thread, monitor, args,
        ));
        // After the enter, which no longer needs `monitor`: the request only
        // reads tables and never collects.
        if minted {
            request_static_self_lock_recompile(shared, cached);
        }
        Ok(guard)
    }

    /// [`Self::acquire`] for a monitor the door already HOLDS, taken
    /// uncontended by `vm_exec::frame_first_door_try` before the door popped
    /// anything (interpreter round i2 wave 56, lane L6): the same static
    /// mirror-slot mint and recompile request, then the hold is adopted.
    /// Nothing collects between that acquire and this call, so `held` is
    /// current.
    pub(in crate::runtime::interpreter::jit_bridge) fn adopt_frame_first_hold(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cached: &CachedBytecodeMethod,
        held: ObjectRef,
    ) -> Self {
        let minted = cached.is_static
            && crate::runtime::env_cache::jit_self_locking_sync_static()
            && crate::jit::helpers::mint_static_sync_mirror_slot(
                shared,
                cached.declaring_class_id,
                held,
            );
        let guard = Self::adopt(shared, thread, held);
        if minted {
            request_static_self_lock_recompile(shared, cached);
        }
        guard
    }

    pub(in crate::runtime::interpreter) fn transfer_to_handler_frame(mut self, frame: &mut crate::runtime::frame::Frame) {
        // The native pin protects the monitor while the replacement frame is
        // built. Once the frame owns it, normal frame unwinding performs the
        // matching implicit monitorexit; the inner guard's `Drop` is disarmed.
        frame.monitor_on_exit = self.0.transfer_to_frame();
    }

    /// Take ownership of a monitor the caller ALREADY holds (a door's
    /// uncontended acquire, `invoke_fast::door_monitor_acquire`), with the
    /// same pin and the same release as [`Self::acquire`] (round 11 wave 6,
    /// lane tier). Nothing is acquired here and nothing can collect, so the
    /// caller's `monitor` address is current.
    pub(in crate::runtime::interpreter::jit_bridge) fn adopt(shared: &SharedVm, thread: &mut JvmThread, monitor: ObjectRef) -> Self {
        Self(crate::vm::vm_exec::SynchronizedMethodGuard::adopt(
            shared, thread, monitor,
        ))
    }

    /// Stop owning the monitor WITHOUT releasing it, and return its current
    /// (possibly relocated) address. The caller now owns the hold and must
    /// hand it to a frame's `monitor_on_exit` before anything can collect.
    /// `None` only for a guard already disarmed. (A pin truncated away
    /// underneath the guard answers the entry-time address, the inner
    /// guard's fallback, rather than `None` as the pre-merge copy did.)
    pub(in crate::runtime::interpreter::jit_bridge) fn surrender(mut self, _thread: &mut JvmThread) -> Option<ObjectRef> {
        self.0.transfer_to_frame()
    }

    /// Hand the method monitor to the interpreter frame a precise deopt resume
    /// just pushed for this method at `thread.frames[frame_index]` (round 11,
    /// lane tier).
    ///
    /// A resume sink returns `FramePushed`, and without this the door's return
    /// dropped the guard -- running the implicit `monitorexit` -- while the
    /// resumed frame, built with `monitor_on_exit == None`, executed the rest
    /// of the `ACC_SYNCHRONIZED` body UNLOCKED and then returned without
    /// releasing anything. The resumed frame now owns the monitor exactly as a
    /// routed handler frame does ([`Self::transfer_to_handler_frame`]). If no
    /// frame is at `frame_index` the guard stays armed and its `Drop` releases
    /// the monitor, as before.
    ///
    /// Round 13 wave 9 (lane chain4): a frame that already owns a method
    /// monitor -- the hold a self-locking body handed over in its stash
    /// (`deopt_resume::frame_hands_over_method_monitor`), on the same object --
    /// keeps it, and this guard's own recursion level is released by its drop:
    /// overwriting `monitor_on_exit` would leave one level held for good.
    pub(in crate::runtime::interpreter::jit_bridge) fn transfer_to_resumed_frame(mut self, thread: &mut JvmThread, frame_index: usize) {
        match thread.frames.get(frame_index) {
            None => return,
            Some(frame) if frame.monitor_on_exit.is_some() => return,
            Some(_) => {}
        }
        let monitor = self.0.transfer_to_frame();
        if let Some(frame) = thread.frames.get_mut(frame_index) {
            frame.monitor_on_exit = monitor;
        }
    }
}

/// Stage 2 of `CRATONVM_SYNC_METHOD_FRAME_FIRST` for the compiled-body doors
/// an interpreted caller takes ([`execute_jit_call`],
/// [`execute_jit_call_decoded`]; interpreter round i2 wave 56, lane L6):
/// asked before the door pops anything. `Err(())` -- the callee's monitor is
/// contended: decline, and the general path pushes the method's frame and
/// blocks with it on top (a compiled body has no frame to block in, so the
/// thread listed its CALLER on top while blocked). `Ok(Some(held))` -- taken
/// uncontended: adopt it ([`JitSynchronizedMonitorGuard::adopt_frame_first_hold`]).
/// `Ok(None)` -- acquire as before (switch off, a `MethodEntry` agent, a
/// static method whose mirror does not exist yet, no receiver at hand), or,
/// for a SELF-LOCKING body, enter it (it is declined only while another
/// thread owns the monitor). One predictable branch for an unsynchronized
/// callee.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn frame_first_compiled_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    receiver: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, ()> {
    if !cached.is_synchronized {
        return Ok(None);
    }
    frame_first_compiled_door_synchronized(shared, thread, compiled, cached, receiver)
}

#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn frame_first_compiled_door_synchronized(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    receiver: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, ()> {
    // The switch first: off, a synchronized call pays no mirror-map read.
    // (With it on, the read replaces the one `acquire` would have made.)
    if !shared.threads.monitors.sync_method_frame_first() {
        return Ok(None);
    }
    let door_holds = door_holds_method_monitor(compiled, cached);
    let lock = if cached.is_static {
        shared
            .classes
            .class_mirrors
            .read()
            .get(&cached.declaring_class_id)
            .copied()
    } else {
        receiver
    };
    let Some(lock) = lock else {
        return Ok(None);
    };
    if !door_holds {
        // A SELF-LOCKING body takes the monitor in its own prologue, where a
        // contended entry would block inside the compiled activation. Owned
        // by another thread now: decline, so the general path blocks frame
        // first. (A monitor taken between this read and the prologue still
        // blocks there; that window is the body's.) No `MethodEntry` agent
        // test: the interpreted path asks it.
        return match shared.threads.monitors.current_owner(lock) {
            Some(owner) if owner != thread.thread_id => {
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!(
                        "[SYNC_FRAME_FIRST] tid={} compiled door declined (self-locking body)",
                        thread.thread_id.0
                    );
                }
                Err(())
            }
            _ => Ok(None),
        };
    }
    match crate::vm::vm_exec::frame_first_door_try(shared, thread, lock) {
        crate::vm::vm_exec::DoorSyncTry::Unchanged => Ok(None),
        crate::vm::vm_exec::DoorSyncTry::Held(held) => Ok(Some(held)),
        crate::vm::vm_exec::DoorSyncTry::Decline => Err(()),
    }
}

/// [`JitSynchronizedMonitorGuard::transfer_to_resumed_frame`] for a door that
/// holds its guard as an `Option` (`None` for an unsynchronized method).
pub(in crate::runtime::interpreter::jit_bridge) fn hand_monitor_to_resumed_frame(
    guard: &mut Option<JitSynchronizedMonitorGuard>,
    thread: &mut JvmThread,
    frame_index: usize,
) {
    if let Some(guard) = guard.take() {
        guard.transfer_to_resumed_frame(thread, frame_index);
    }
}

/// The most recompiles [`request_static_self_lock_recompile`] asks for one
/// method until its class is redefined, each against a different wrapped body.
pub(in crate::runtime::interpreter::jit_bridge) const STATIC_SELF_LOCK_RECOMPILES_PER_EPOCH: u8 = 3;

/// `CRATONVM_JIT_STATIC_SELF_LOCK_RECOMPILE` (round 15 wave 4, lane sync3,
/// SY2-2 of `jit-r15w3-sync2-proposals-RETIRED-20261003.md`; **default ON**, `0` restores the
/// wave-3 behaviour: the compiled-caller door does not mint the mirror slot,
/// no recompile is asked for, and a refused supersede never keeps a
/// fall-through for shedding the door): see
/// [`request_static_self_lock_recompile`] and [`fall_through_sheds_the_door`].
/// Read on the doors' cold paths and once per refused supersede.
pub(in crate::runtime::interpreter::jit_bridge) fn static_self_lock_recompile_enabled() -> bool {
    true
}

/// Round 15 wave 4 (lane sync3, SY2-2 of `jit-r15w3-sync2-proposals-RETIRED-20261003.md`;
/// page `r15w4-sync3-static-sync-body-compiled-before-its-mirror-slot-FIXED-20260930.md`):
/// a door is entering the WRAPPED body of the `static synchronized` method
/// `cached`, and its class's mirror slot exists -- this thread just minted it
/// (`JitSynchronizedMonitorGuard::acquire`, [`execute_wrapped_body_door_locked`])
/// or a wrapped body was published since it last looked
/// ([`wrapped_body_memoized`]). The background worker only FINDS a minted slot
/// (`helpers::class_mirror_slot_for`), so every compile that ran before the
/// mint produced a wrapped body, and the ~400 ns door stayed for good once the
/// method was at its last tier. Ask the tiered manager for one more optimizing
/// compile (its deferred-`new` retry door: a C2 task that may replace a C2
/// body, under every other gate the manager keeps), which now carries the slot
/// and so builds a self-locking body.
///
/// Asked only when that compile could self-lock
/// (`cratonvm_jit::static_self_lock_recompile_would_self_lock`), under the
/// background compiler (a mutator's own compile mints the slot itself, and
/// with `CRATONVM_BG_COMPILE=0` nothing would run the task) and C1->C2
/// supersede, at most [`STATIC_SELF_LOCK_RECOMPILES_PER_EPOCH`] times per
/// method and never twice against one body. A publication only: running
/// activations of the wrapped body keep the door's hold and release it
/// through the door; the next entry takes the self-locking body, whose own
/// enter locks the same mirror.
pub(in crate::runtime::interpreter::jit_bridge) fn request_static_self_lock_recompile(shared: &SharedVm, cached: &CachedBytecodeMethod) {
    if !(cached.is_static && cached.is_synchronized)
        || !static_self_lock_recompile_enabled()
        || !crate::runtime::env_cache::jit_self_locking_sync_static()
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    let wrapped_entry = shared
        .jit
        .jit_cache
        .read()
        .get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        )
        .filter(|compiled| compiled.requires_wrapped_entry && !compiled.self_locks_monitor)
        .map(|compiled| compiled.entry_ptr() as usize); // Cast: entry address as an identity
    let Some(body) = wrapped_entry else {
        return;
    };
    let epoch = shared.jit.jit_cache.redefine_epoch();
    let key = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    // Round 15 wave 5 (lane sync4): the memo first. `wrapped_body_memoized`
    // asks on every `JitCache::generation` change it sees -- every publish in
    // the VM -- and the predicate below decodes the bytecode and probes the
    // slot table and the verdict registry, so a body already asked about (or
    // an exhausted method) is answered here, before any of that.
    let seen = shared
        .jit
        .bridge
        .static_self_lock_recompiles
        .lock()
        .map(|asked| {
            asked.get(&key).is_some_and(|row| {
                row.0 == epoch && (row.1 >= STATIC_SELF_LOCK_RECOMPILES_PER_EPOCH || row.2 == body)
            })
        })
        .unwrap_or(true);
    if seen {
        return;
    }
    let slot = crate::jit::helpers::class_mirror_slot_for(shared, cached.declaring_class_id);
    let would_self_lock = cratonvm_jit::static_self_lock_recompile_would_self_lock(
        jit_verdicts(shared),
        cached,
        slot,
    );
    let granted = shared
        .jit
        .bridge
        .static_self_lock_recompiles
        .lock()
        .map(|mut asked| {
            let row = asked.entry(key).or_insert((epoch, 0, 0));
            if row.0 != epoch {
                *row = (epoch, 0, 0);
            }
            if row.1 >= STATIC_SELF_LOCK_RECOMPILES_PER_EPOCH || row.2 == body {
                return false;
            }
            if !would_self_lock {
                // With the slot present the answer is a property of this body
                // (an exception table, monitor bytecode, a refused self-lock):
                // remember it without spending a recompile. With no slot yet,
                // the next mint must be able to ask again.
                if slot != 0 {
                    row.2 = body;
                }
                return false;
            }
            row.1 += 1;
            row.2 = body;
            true
        })
        .unwrap_or(false);
    if !granted {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] static self-lock recompile requested {}.{}{}: its published body is \
             wrapped and its class's mirror slot exists now",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    // Round 15 wave 5 (lane sync4): its own door, so the manager's dbg lines
    // name it (`static self-lock retry ...`), not the deferred-`new` retry.
    shared.jit.tiered_manager.request_self_lock_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            cached.declaring_class_id,
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
    );
}

/// `CRATONVM_JIT_LATE_BIND_RECOMPILE` (JIT round 16 wave 14, lane throw4,
/// TP16-20 of `docs/feature-designs/real-jdk-mode.md`; **default
/// OFF** until measured): does a statically bound call site that a compile left
/// on the dispatch helper only because its callee could not bind YET -- the
/// callee's class not loaded, or a static callee's class not initialized, when
/// the method was compiled ([`late_bind_refusal_may_lift`]) -- get its method
/// recompiled once the dispatch helper first serves that callee from a compiled
/// body, and the displaced body made not entrant so baked callers forward to
/// the new one? Read once per VM.
///
/// The shape it closes (`R16Throw3Split nofill`, 352 ns against HotSpot's 19):
/// the eager callee chain compiles a static method that has never run, while
/// binding its caller's call to it, so every class that method instantiates is
/// still unloaded; each `new X(..)`'s `invokespecial X.<init>` is refused
/// `callee-class-not-found` and stays a `jit_invoke_dispatch` round trip for the
/// life of the body. So does any compiled method whose `throw new MyException`
/// path had not run when it was compiled -- an exception class is loaded at its
/// first throw. HotSpot traps there and recompiles.
pub(crate) fn late_bind_recompile_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.late_bind_switch.get_or_init(|| {
        // Default ON since wave 14 (`R16Throw3Split nofill` 363 -> 64,
        // `R16Throw4Site` hook rows ~300 -> ~30 ns/op; loaded 20/20).
        true
    })
}

/// The most distinct callees [`note_late_bind_waiter`] remembers waiters for,
/// and the most waiters per callee: a bound on a map that only cold refusals
/// fill. Past either, a refusal is simply not remembered (today's behaviour).
pub(in crate::runtime::interpreter::jit_bridge) const LATE_BIND_CALLEE_CAP: usize = 4096;
pub(in crate::runtime::interpreter::jit_bridge) const LATE_BIND_WAITERS_PER_CALLEE: usize = 64;

/// A method whose published body left a call on the dispatch helper that a
/// recompile could now bind ([`JitBridgeState::late_bind_waiters`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct LateBindWaiter {
    pub(in crate::runtime::interpreter::jit_bridge) class_id: u32,
    pub(in crate::runtime::interpreter::jit_bridge) class_name: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) method_name: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) descriptor: Arc<str>,
}

/// The waiter map's key for a callee named `(class, method, descriptor)`: the
/// constant-pool names the refusal saw, which are the names the site's
/// `JitInvokeInfo` carries to the dispatch helper.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_callee_key(class: &str, method: &str, descriptor: &str) -> String {
    let mut key = String::with_capacity(class.len() + method.len() + descriptor.len() + 1);
    key.push_str(class);
    key.push('.');
    key.push_str(method);
    key.push_str(descriptor);
    key
}

/// Is a direct-bind refusal a compile-ORDER accident that a later compile of
/// the same method can lift -- the callee's class not loaded yet, or a static
/// callee's class not initialized yet -- rather than a standing policy (a
/// native shadow, an `invokedynamic` trap, a synchronized callee with no
/// self-locking body, ...)? Only these are remembered.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_refusal_may_lift(refusal: cratonvm_jit::DirectBindRefusal) -> bool {
    matches!(
        refusal,
        cratonvm_jit::DirectBindRefusal::CalleeClassNotFound
            | cratonvm_jit::DirectBindRefusal::DeclaringClassNotInitialized
    )
}

/// The background door refused to bind `callee` in the body of `waiting` it is
/// compiling: remember `waiting` when the refusal can lift
/// ([`late_bind_refusal_may_lift`]) and `CRATONVM_JIT_LATE_BIND_RECOMPILE` is
/// on. Cold: one compile-time refusal.
pub(in crate::runtime::interpreter::jit_bridge) fn note_late_bind_waiter(
    shared: &SharedVm,
    refusal: cratonvm_jit::DirectBindRefusal,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting: &CachedBytecodeMethod,
) {
    let waiter = LateBindWaiter {
        class_id: waiting.declaring_class_id.as_u32(),
        class_name: Arc::clone(&waiting.class_name),
        method_name: Arc::clone(&waiting.method_name),
        descriptor: Arc::clone(&waiting.method_descriptor),
    };
    note_late_bind_waiter_for(
        shared,
        refusal,
        callee_class,
        callee_method,
        callee_desc,
        waiter,
    );
}

/// [`note_late_bind_waiter`] for a waiter named by its parts (JIT round 18
/// wave 9, lane eagercold: the uninitialized-constructor splice refusal,
/// [`note_uninit_ctor_late_bind_waiter`], has no `CachedBytecodeMethod` in
/// scope). Same lift and switch rules, same caps.
pub(in crate::runtime::interpreter::jit_bridge) fn note_late_bind_waiter_for(
    shared: &SharedVm,
    refusal: cratonvm_jit::DirectBindRefusal,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiter: LateBindWaiter,
) {
    if !late_bind_refusal_may_lift(refusal) || !late_bind_recompile_enabled(shared) {
        return;
    }
    let key = late_bind_callee_key(callee_class, callee_method, callee_desc);
    let (class_name, method_name, descriptor) = (
        Arc::clone(&waiter.class_name),
        Arc::clone(&waiter.method_name),
        Arc::clone(&waiter.descriptor),
    );
    if note_late_bind_waiter_entry(shared, key, waiter) && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING {class_name}.{method_name}{descriptor} on {callee_class}.{callee_method}{callee_desc} ({refusal:?})"
        );
    }
}

/// Remember `waiting` under `key` in [`JitBridgeState::late_bind_waiters`];
/// `true` when it was added (the caps and duplicates answer `false`).
pub(in crate::runtime::interpreter::jit_bridge) fn note_late_bind_waiter_key(
    shared: &SharedVm,
    key: String,
    waiting: &CachedBytecodeMethod,
) -> bool {
    let waiter = LateBindWaiter {
        class_id: waiting.declaring_class_id.as_u32(),
        class_name: Arc::clone(&waiting.class_name),
        method_name: Arc::clone(&waiting.method_name),
        descriptor: Arc::clone(&waiting.method_descriptor),
    };
    note_late_bind_waiter_entry(shared, key, waiter)
}

/// [`note_late_bind_waiter_key`] for a waiter named by its parts: the one
/// place that takes the waiter map's lock and applies its caps. `true` when
/// `waiter` was added (the caps and duplicates answer `false`).
pub(in crate::runtime::interpreter::jit_bridge) fn note_late_bind_waiter_entry(shared: &SharedVm, key: String, waiter: LateBindWaiter) -> bool {
    let bridge = &shared.jit.bridge;
    let Ok(mut waiters) = bridge.late_bind_waiters.lock() else {
        return false;
    };
    if waiters.len() >= LATE_BIND_CALLEE_CAP && !waiters.contains_key(&key) {
        return false;
    }
    let list = waiters.entry(key).or_default();
    if list.len() >= LATE_BIND_WAITERS_PER_CALLEE || list.contains(&waiter) {
        return false;
    }
    list.push(waiter);
    bridge
        .late_bind_waiter_count
        .store(waiters.len(), std::sync::atomic::Ordering::Release);
    true
}

/// A caller of [`JitBridgeState::late_bind_cold`]: `(class id, method name,
/// descriptor)`, as `late_bind_pending` keys it.
pub(in crate::runtime::interpreter::jit_bridge) type LateBindColdCaller = (u32, Arc<str>, Arc<str>);

/// [`JitBridgeState::late_bind_cold`]'s rows.
#[derive(Default)]
pub(in crate::runtime::interpreter::jit_bridge) struct LateBindColdState {
    /// `(caller, callee key)` pairs registered from an `EagerChainCold`
    /// refusal, so a fire can tell a cold waiter from a may-lift one.
    pub(in crate::runtime::interpreter::jit_bridge) registered: std::collections::HashSet<(LateBindColdCaller, String)>,
    /// Callers already recompiled once on account of a cold callee.
    pub(in crate::runtime::interpreter::jit_bridge) spent: std::collections::HashSet<LateBindColdCaller>,
}



/// The background door left `callee` unbound in the OPTIMIZING body of
/// `waiting` because the callee was cold (`eager-callee-chain-cold`): remember
/// `waiting` so the callee's first compiled serve asks ONE recompile of it.
///
/// Why bounded, and why only here. A cold callee is deliberately not a
/// may-lift refusal ([`late_bind_refusal_may_lift`]): every cold callee of
/// every compiled method would wait, and a run with that row recompiled
/// thousands of methods. This row differs in three ways. Only an optimizing
/// body waits (a single-pass caller that stays hot is re-tiered anyway, and
/// that compile asks again). It fires only if the callee is later compiled
/// AND served through the dispatch helper or a direct-call cell at a site,
/// i.e. it really warmed.
/// And each caller is recompiled at most once on a cold callee's account per
/// VM ([`late_bind_cold_fire_admits`]), whatever number of cold callees it
/// had. The shape it closes: `CompletableFuture$UniApply.tryFire` compiled
/// while `UniCompletion.claim` was cold kept `claim` (and its `Unsafe` CAS)
/// a bound-site call for the run, 3x the pending row
/// (`r21-completablefuture-stage-cost-is-allocation-and-calls-20261018-RETIRED-20261024.md`,
/// wave 8b).
pub(in crate::runtime::interpreter::jit_bridge) fn note_cold_callee_late_bind_waiter(
    shared: &SharedVm,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting: &CachedBytecodeMethod,
    optimize: bool,
) {
    if !optimize || !late_bind_recompile_enabled(shared) {
        return;
    }
    let caller: LateBindColdCaller = (
        waiting.declaring_class_id.as_u32(),
        Arc::clone(&waiting.method_name),
        Arc::clone(&waiting.method_descriptor),
    );
    let key = late_bind_callee_key(callee_class, callee_method, callee_desc);
    {
        let Ok(mut cold) = shared.jit.bridge.late_bind_cold.lock() else {
            return;
        };
        if !late_bind_cold_may_register(&cold, &caller) {
            return;
        }
        cold.registered.insert((caller, key.clone()));
    }
    if note_late_bind_waiter_key(shared, key, waiting) && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING (cold callee) {}.{}{} on \
             {callee_class}.{callee_method}{callee_desc}",
            waiting.class_name, waiting.method_name, waiting.method_descriptor
        );
    }
}

/// May `caller` register one more cold-callee waiter? Not once it was
/// recompiled on a cold callee's account, and not past the caps.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_cold_may_register(cold: &LateBindColdState, caller: &LateBindColdCaller) -> bool {
    !cold.spent.contains(caller)
        && cold.registered.len() < LATE_BIND_CALLEE_CAP
        && cold.spent.len() < LATE_BIND_CALLEE_CAP
}

/// [`late_bind_key_served`]'s filter: `false` skips `waiter`'s recompile
/// request because it waits on `key` only as a COLD callee and its caller was
/// already recompiled once on that account. A may-lift waiter, and the first
/// cold fire of a caller, pass (the latter marks the caller spent).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_cold_fire_admits(shared: &SharedVm, waiter: &LateBindWaiter, key: &str) -> bool {
    let Ok(mut cold) = shared.jit.bridge.late_bind_cold.lock() else {
        return true;
    };
    let caller: LateBindColdCaller = (
        waiter.class_id,
        Arc::clone(&waiter.method_name),
        Arc::clone(&waiter.descriptor),
    );
    late_bind_cold_fire_decide(&mut cold, caller, key)
}

/// The decision of [`late_bind_cold_fire_admits`], on the state alone (unit
/// tested).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_cold_fire_decide(
    cold: &mut LateBindColdState,
    caller: LateBindColdCaller,
    key: &str,
) -> bool {
    if cold.registered.is_empty() {
        return true;
    }
    if !cold.registered.remove(&(caller.clone(), key.to_string())) {
        return true;
    }
    cold.spent.insert(caller)
}


/// `CRATONVM_JIT_FIRST_CALL_RECOMPILE` (al16, 2026-10-05): is a method-entry
/// body compiled while one of its call sites was left to its first execution
/// (`jit_first_call_resolve`) recompiled once that site has run? The compile
/// door builds such a site as a plain call with a first-execution guard --
/// never spliced, never lowered -- and compiled dispatch fills no site cache,
/// so without this the body keeps the plain call for life (a for-each method
/// compiled before its loop ran: `R16ArraylistSplit iter-foreach`). The same
/// recompile as the late-bind one ([`late_bind_key_served`]). Read once per
/// VM; on by default (`=0` keeps the body).
pub(in crate::runtime::interpreter::jit_bridge) fn first_call_recompile_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .first_call_recompile_switch
        .get_or_init(|| {
            true
        })
}

/// `CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST` (JIT round 21 wave 1, lane indy;
/// **default ON** since the r22 A/B, `=0` kills): may the compiled `getstatic` helper
/// (`jit_getstatic`) answer a read of a class the CURRENT thread is
/// initializing -- a compiled body reached from that class's own `<clinit>`,
/// the JVMS 5.5 step 3 recursive request -- from one untracked class-manager
/// read ([`class_initializing_on`]), instead of the full
/// `ensure_class_initialized_shared` walk plus the `Initialized` re-check
/// (three tracked class-manager reads per static read)? Same answers: that walk
/// returns `Ok` for the recursive request, and the re-check is `false` while
/// the class is still being initialized. Read once per VM.
pub(crate) fn getstatic_own_clinit_fast_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .getstatic_own_clinit_switch
        .get_or_init(|| {
            // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
            true
        })
}

/// Is `class_id` being initialized by the thread `thread_id` right now (its
/// INIT claim is held by that thread: state `Initializing`, see
/// [`clinit_init_claimant`])? `false` when the class manager's read side is not free
/// this instant (the caller then takes the ordinary path). Never blocks: an
/// untracked try-read adds no lock-order edge.
pub(crate) fn class_initializing_on(
    shared: &SharedVm,
    class_id: ClassId,
    thread_id: u64,
) -> bool {
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return false;
    };
    // Review rev-vmjit item 2: an INIT claim only (`clinit_init_claimant`); a
    // link-only claim takes the ordinary walk.
    cm.get_class(class_id)
        .is_some_and(|c| clinit_init_claimant(c.state, c.initializing_thread) == Some(thread_id))
}



/// The most init-owner cells one VM mints ([`clinit_owner_cell_for`]); past it
/// a compile keeps the plain helper.
pub(in crate::runtime::interpreter::jit_bridge) const CLINIT_OWNER_CELL_CAP: usize = 4096;

/// JIT round 21 wave 1 (lane indy, INDY21-2, `CRATONVM_JIT_IR_CLINIT_BARRIER`):
/// the address of `class_id`'s init-owner cell -- a word holding `1 +` the
/// thread id of the thread whose init claim the class is under, `0` without a
/// claim -- minted on first ask (a compile thread), or `0` (cap reached, the
/// class manager busy this instant, an unknown class).
///
/// Exactness. The word is written in three places, each ordered against the
/// class manager lock that guards `initializing_thread`: here, WHILE the read
/// side is held (so a claim or a completion cannot interleave between the
/// read and the store); by the init claim's `Initializing` transition
/// ([`clinit_owner_note`] under the write lock, `initialize_class_shared`;
/// never a link-only claim, never a `--compatible` stub); and cleared by
/// `finalize_class_init` after its write lock ended the claim. The count is
/// stored (SeqCst) BEFORE the class manager is read here, so a claim or
/// completion whose lock comes after that read also sees the cell. A stale
/// non-zero word is therefore impossible; a stale `0` only sends the claimant
/// to the helper.
///
/// LEAK(intentional, bounded): compiled code bakes the cell's address; at
/// most [`CLINIT_OWNER_CELL_CAP`] per VM.
pub(crate) fn clinit_owner_cell_for(shared: &SharedVm, class_id: ClassId) -> usize {
    use std::sync::atomic::{AtomicU64, Ordering};
    let bridge = &shared.jit.bridge;
    let Ok(mut cells) = bridge.clinit_owner_cells.lock() else {
        return 0;
    };
    let known: Option<&'static AtomicU64> = cells.get(&class_id.as_u32()).copied();
    let cell: &'static AtomicU64 = match known {
        Some(cell) => cell,
        None => {
            if cells.len() >= CLINIT_OWNER_CELL_CAP {
                return 0;
            }
            let cell: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
            cells.insert(class_id.as_u32(), cell);
            bridge
                .clinit_owner_cell_count
                .store(cells.len(), Ordering::SeqCst);
            cell
        }
    };
    // An existing cell is refreshed too: a claim made before the cell
    // existed found no cell to write.
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        // The cell stays (and the hooks keep it) at 0: this compile keeps
        // the plain helper.
        return 0;
    };
    let Some(class) = cm.get_class(class_id) else {
        return 0;
    };
    // Review rev-vmjit item 2: a `--compatible` stub may be upgraded in place,
    // which clears its claim with no hook, so it never gets a live cell (a
    // real class is never upgraded, and a stub becomes real, never the
    // reverse; the claim hook skips stubs too). And only an INIT claim names
    // its thread: `initializing_thread` is also set by link-only claims.
    if class.origin.is_compatibility_stub() {
        cell.store(0, Ordering::SeqCst);
        return 0;
    }
    let owner = clinit_init_claimant(class.state, class.initializing_thread);
    // Under the read side: see the doc.
    cell.store(owner.map_or(0, |t| t.wrapping_add(1)), Ordering::SeqCst);
    drop(cm);
    cell as *const AtomicU64 as usize
}

/// JIT round 21 wave 1 (lane indy; review rev-vmjit item 2): the thread an
/// INIT claim names, or `None`. `initializing_thread` alone also names a
/// link-only claim (`class_awaits_link`'s claim, `LINK_WITHOUT_JAVA_CLAIM`);
/// only the init claim moves the state to `Initializing`
/// (`initialize_class_shared`), and only until `finalize_class_init`.
pub(in crate::runtime::interpreter::jit_bridge) fn clinit_init_claimant(
    state: crate::classloading::ClassState,
    initializing_thread: Option<u64>,
) -> Option<u64> {
    initializing_thread.filter(|_| state == crate::classloading::ClassState::Initializing)
}

/// JIT round 21 wave 1 (lane indy, INDY21-2): `class_id`'s init claim moved
/// to `owner` (a thread id; the claim) or ended (`None`; `finalize_class_init`).
/// One load when the VM has no init-owner cell.
pub(crate) fn clinit_owner_note(shared: &SharedVm, class_id: ClassId, owner: Option<u64>) {
    use std::sync::atomic::Ordering;
    let bridge = &shared.jit.bridge;
    if bridge.clinit_owner_cell_count.load(Ordering::SeqCst) == 0 {
        return;
    }
    if let Ok(cells) = bridge.clinit_owner_cells.lock() {
        if let Some(cell) = cells.get(&class_id.as_u32()) {
            cell.store(owner.map_or(0, |t| t.wrapping_add(1)), Ordering::SeqCst);
        }
    }
}

/// `CRATONVM_JIT_STATIC_INIT_RACE_SERVE` (JIT round 21 wave 1, lane indy,
/// INDY21-3; **default ON**, `=0` restores the race): see
/// [`note_static_init_waiters`]. Read once per VM.
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_race_serve_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .static_init_race_switch
        .get_or_init(|| {
            true
        })
}

/// `CRATONVM_JIT_STATIC_INIT_RACE_EXACT` (JIT round 22 wave 1, lane runtime;
/// **default ON**, `=0` restores the windowed answer): which static holders a
/// method-entry compile may have read uninitialized is answered per class
/// ([`StaticInitLog::initialized_after`]) instead of from the last
/// [`STATIC_INIT_LOG_CAP`] entries. With the window, a compile during which
/// more than 1024 classes finished initializing (a large application's start,
/// a long optimizing compile) found the log no longer reaching back and called
/// EVERY initialized holder of the method raced, so its recompile was asked at
/// once; that recompile, when the same happened again, asked another, with no
/// bound (the late-bind retry door caps only failed compiles), against the
/// "one extra recompile at most per raced class" this rule promises. Exact,
/// a holder is raced only if it finished initializing after the compile began,
/// which a recompile that read it initialized can never see again. Read once
/// per VM.
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_race_exact_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .static_init_race_exact_switch
        .get_or_init(|| {
            true
        })
}

/// Entries [`StaticInitLog`] keeps; past it the oldest go, and a compile that
/// started before the oldest kept entry is answered conservatively (under
/// `CRATONVM_JIT_STATIC_INIT_RACE_EXACT=0`; the default answers per class).
pub(in crate::runtime::interpreter::jit_bridge) const STATIC_INIT_LOG_CAP: usize = 1024;

/// JIT round 21 wave 1 (lane indy, INDY21-3): the numbered tail of the
/// VM's successful class initializations.
#[derive(Default)]
pub(in crate::runtime::interpreter::jit_bridge) struct StaticInitLog {
    /// The number of the newest entry (`0`: none yet).
    pub(in crate::runtime::interpreter::jit_bridge) seq: u64,
    /// `(number, class id)`, oldest first, at most [`STATIC_INIT_LOG_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) recent: std::collections::VecDeque<(u64, u32)>,
    /// JIT round 22 wave 1 (lane runtime, `CRATONVM_JIT_STATIC_INIT_RACE_EXACT`):
    /// `class id -> its entry's number`, every logged class while the switch
    /// is on (one row per class; a class initializes once), never truncated by
    /// the window but pruned at class unload ([`Self::forget_classes`], wave 2).
    /// See [`Self::initialized_after`].
    pub(in crate::runtime::interpreter::jit_bridge) by_class: std::collections::HashMap<u32, u64>,
}

impl StaticInitLog {
    pub(in crate::runtime::interpreter::jit_bridge) fn push(&mut self, class_id: u32) {
        self.seq += 1;
        self.recent.push_back((self.seq, class_id));
        while self.recent.len() > STATIC_INIT_LOG_CAP {
            self.recent.pop_front();
        }
    }

    /// [`Self::push`], and (`CRATONVM_JIT_STATIC_INIT_RACE_EXACT` on) the
    /// class's row in [`Self::by_class`]. JIT round 22 wave 2, lane runtime
    /// (review `r22-review-w2-findings-FIXED-20261020.md` item 2): the per-class rows
    /// are filled only when the exact answer reads them.
    pub(in crate::runtime::interpreter::jit_bridge) fn push_tracked(&mut self, class_id: u32, exact: bool) {
        self.push(class_id);
        if exact {
            self.by_class.insert(class_id, self.seq);
        }
    }

    /// Drop the per-class rows of unloaded classes (a class id may be reused;
    /// a row is one entry per class initialized while the switch is on).
    ///
    /// Review w3 item 4 (lane sync): the window too. An unloaded id still in
    /// [`Self::recent`] read "initialized since" for a class that reuses the id
    /// (one spurious recompile under `CRATONVM_JIT_STATIC_INIT_RACE_EXACT=0`).
    /// Its entry is tombstoned (class id `0`, which names no class) rather
    /// than removed, so the window's reach (`recent.front()`'s number, which
    /// [`Self::initialized_since`] asks) does not change; the tombstones are
    /// skipped there.
    pub(in crate::runtime::interpreter::jit_bridge) fn forget_classes(&mut self, unloaded: &dyn Fn(u32) -> bool) {
        self.by_class.retain(|class_id, _| !unloaded(*class_id));
        for entry in self.recent.iter_mut() {
            if entry.1 != 0 && unloaded(entry.1) {
                entry.1 = 0;
            }
        }
    }

    /// Was `class_id` logged as initialized after entry `seq0`? Exact for every
    /// logged class however many entries [`Self::recent`] dropped since; a class
    /// the log never saw (initialized before the switch was read, or by a path
    /// that does not log) answers `false`, i.e. it is not treated as raced.
    pub(in crate::runtime::interpreter::jit_bridge) fn initialized_after(&self, class_id: u32, seq0: u64) -> bool {
        self.by_class.get(&class_id).is_some_and(|&seq| seq > seq0)
    }

    /// The classes initialized after entry `seq0`, or `None` when the log no
    /// longer reaches back to `seq0` (some may have been dropped).
    pub(in crate::runtime::interpreter::jit_bridge) fn initialized_since(&self, seq0: u64) -> Option<Vec<u32>> {
        if self.seq <= seq0 {
            return Some(Vec::new());
        }
        match self.recent.front() {
            Some(&(oldest, _)) if oldest <= seq0 + 1 => Some(
                self.recent
                    .iter()
                    .filter(|&&(s, class_id)| s > seq0 && class_id != 0)
                    .map(|&(_, class_id)| class_id)
                    .collect(),
            ),
            _ => None,
        }
    }
}

/// JIT round 21 wave 1 (lane indy, INDY21-3): `class_id`'s `<clinit>`
/// succeeded. Called by `finalize_class_init` BEFORE the class's initialized
/// bit is set, so a compile door that sees the bit set also finds the entry.
pub(crate) fn static_init_log_class(shared: &SharedVm, class_id: ClassId) {
    if !static_init_race_serve_enabled(shared) {
        return;
    }
    let exact = static_init_race_exact_enabled(shared);
    if let Ok(mut log) = shared.jit.bridge.static_init_log.lock() {
        log.push_tracked(class_id.as_u32(), exact);
    }
}

/// JIT round 22 wave 2, lane runtime (review `r22-review-w2-findings-FIXED-20261020.md`
/// item 2): class unload (`memory::gc`'s class-unload sweep) drops the
/// unloaded classes' [`StaticInitLog`] rows, so the table is bounded by the live
/// classes and a reused class id never inherits a dead class's entry number.
pub(crate) fn static_init_log_forget_classes(shared: &SharedVm, unloaded: &dyn Fn(u32) -> bool) {
    if let Ok(mut log) = shared.jit.bridge.static_init_log.lock() {
        log.forget_classes(unloaded);
    }
}

/// The log's newest entry number, read by a compile door BEFORE it reads any
/// class's initialization state (`0` with the switch off).
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_log_seq(shared: &SharedVm) -> u64 {
    if !static_init_race_serve_enabled(shared) {
        return 0;
    }
    shared
        .jit
        .bridge
        .static_init_log
        .lock()
        .map(|log| log.seq)
        .unwrap_or(0)
}

/// The late-bind waiter key of the static holder `class_id`.
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_waiter_key(class_id: u32) -> String {
    format!("#static-init:{class_id:x}")
}

/// Remember the method `compiled` is a body of as waiting on every class in
/// its `static_init_classes` that is not initialized now (its `getstatic`
/// sites of that class are helper calls), so the class's `<clinit>` end asks
/// for a recompile ([`static_init_class_initialized`]). A class whose
/// initialization completed after the compile read it but before this check is
/// missed (the compile and the `<clinit>` raced): its body keeps the helper,
/// today's behaviour.
///
/// JIT round 21 wave 1 (lane indy, INDY21-3, `CRATONVM_JIT_STATIC_INIT_RACE_SERVE`,
/// default ON): that miss left the body on the helper FOR LIFE (nothing else
/// recompiles it). `log_seq0` is [`static_init_log_seq`] read when the door
/// started, before the compile read any class's state: a holder initialized
/// since then (per [`StaticInitLog`]: per class by default,
/// `CRATONVM_JIT_STATIC_INIT_RACE_EXACT`; under `=0` from the last entries, and
/// every initialized holder when they no longer reach back) may have been read
/// uninitialized, so it is noted too and, being initialized, served at once --
/// one recompile, whose own `log_seq0` is newer, so it cannot repeat for the
/// same class (with `=0`, it can: a recompile that outlives the window calls
/// every holder raced again).
pub(in crate::runtime::interpreter::jit_bridge) fn note_static_init_waiters(
    shared: &SharedVm,
    compiled: &cratonvm_jit::CompiledMethod,
    waiting: &CachedBytecodeMethod,
    log_seq0: u64,
) {
    // JIT round 21, lane smallfix: the class's own `init_state`, as the OSR
    // twin asks (`static_init_pending_any_initialized`), not the lazily
    // filled `class_init_bits` memo alone: a long-initialized class with no
    // bit yet was noted as a waiter whose `<clinit>` end never comes.
    let initialized = |class_id: u32| static_init_class_is_initialized(shared, class_id);
    let mut pending: Vec<u32> = compiled
        .static_init_classes
        .iter()
        .copied()
        .filter(|&class_id| class_id != 0 && !initialized(class_id))
        .collect();
    if static_init_race_serve_enabled(shared) {
        // JIT round 22 wave 1, lane runtime (`CRATONVM_JIT_STATIC_INIT_RACE_EXACT`,
        // default ON): per class, so a log that no longer reaches back to
        // `log_seq0` cannot call every initialized holder raced (and recompile
        // the method again and again). `=0`: the windowed answer below.
        let exact = static_init_race_exact_enabled(shared);
        let log = shared.jit.bridge.static_init_log.lock().ok();
        let raced_exactly: Option<Vec<u32>> = match (exact, log.as_ref()) {
            (true, Some(log)) => Some(
                compiled
                    .static_init_classes
                    .iter()
                    .copied()
                    .filter(|&class_id| class_id != 0 && log.initialized_after(class_id, log_seq0))
                    .collect(),
            ),
            // A poisoned log: no class is called raced.
            (true, None) => Some(Vec::new()),
            (false, _) => None,
        };
        let since = if exact {
            None
        } else {
            log.as_ref().and_then(|log| log.initialized_since(log_seq0))
        };
        drop(log);
        let late: Vec<u32> = compiled
            .static_init_classes
            .iter()
            .copied()
            .filter(|&class_id| {
                class_id != 0
                    && initialized(class_id)
                    && match raced_exactly.as_ref() {
                        Some(raced) => raced.contains(&class_id),
                        None => since.as_ref().is_none_or(|s| s.contains(&class_id)),
                    }
            })
            .collect();
        if !late.is_empty() && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] static-init RACE {}.{}{}: class ids {late:?} finished \
                 initializing during the compile",
                waiting.class_name, waiting.method_name, waiting.method_descriptor
            );
        }
        pending.extend(late);
        pending.sort_unstable();
        pending.dedup();
    }
    if pending.is_empty() {
        return;
    }
    for class_id in pending {
        let key = static_init_waiter_key(class_id);
        if note_late_bind_waiter_key(shared, key.clone(), waiting)
            && crate::runtime::env_cache::dbg_jitc()
        {
            eprintln!(
                "[cratonvm-jitc] static-init WAITING {}.{}{} on class id {class_id}",
                waiting.class_name, waiting.method_name, waiting.method_descriptor
            );
        }
        // Initialized while the waiter was being noted: its `<clinit>` end
        // may have found no waiter, so ask now.
        if initialized(class_id) {
            late_bind_key_served(shared, &key, "finished initializing during the compile");
        }
    }
}

/// `class_id`'s `<clinit>` just completed (`finalize_class_init`): ask the
/// recompile of every method whose body reads its statics through the helper
/// ([`note_static_init_waiters`]). One relaxed load when nothing waits.
pub(crate) fn static_init_class_initialized(shared: &SharedVm, class_id: ClassId) {
    // JIT round 21 (lane tcshape): an OSR body may be the only waiter
    // (`note_osr_init_waiters`).
    if shared
        .jit
        .bridge
        .late_bind_waiter_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
        && shared
            .jit
            .bridge
            .late_bind_osr_waiter_count
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
    {
        return;
    }
    late_bind_key_served(
        shared,
        &static_init_waiter_key(class_id.as_u32()),
        "is initialized now",
    );
}

/// The first-call twin of [`note_osr_optimizing_static_init_waiters`]: an
/// accepted optimizing OSR body built while call sites of its class were
/// still unresolved ([`AcceptedOsrOptimizing::first_call_cells_at_build`])
/// waits, as an `optimizing` OSR late-bind waiter, on each of those cells.
/// The al16 forget is taken only at the door's next lookup, and a frame that
/// never leaves the body never causes one: a fill loop followed by the loop
/// that makes the calls (`BENCHMARK.md`'s String/Regex kernel) kept `find` /
/// `group` / `parseLong` as plain calls for life. The wake asks the OSR
/// compile ([`late_bind_osr_key_served`]), whose remembering arms the running
/// body's exit ([`arm_superseded_optimizing_osr_exits`]). Cold: once per
/// accepted body.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_first_call_waiters(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    entry_pc: usize,
) {
    if !late_bind_osr_optimizing_enabled()
        || !late_bind_osr_enabled()
        || !late_bind_recompile_enabled(shared)
    {
        return;
    }
    let (class_name, method_name, descriptor): (Arc<str>, Arc<str>, Arc<str>) =
        (names.0.into(), names.1.into(), names.2.into());
    // Empty with `CRATONVM_JIT_OSR_REBUILD_RESOLVED_FIRST_CALLS=0`.
    for &cell in armed_first_call_cells_of(shared, class_id.as_u32()).iter() {
        let waiter = OsrLateBindWaiter {
            class_id: class_id.as_u32(),
            class_name: Arc::clone(&class_name),
            method_name: Arc::clone(&method_name),
            descriptor: Arc::clone(&descriptor),
            entry_bci: u32::try_from(entry_pc).unwrap_or(u32::MAX),
            optimizing: true,
        };
        let key = first_call_waiter_key(cell);
        if note_osr_late_bind_waiter_entry(shared, key.clone(), waiter)
            && crate::runtime::env_cache::dbg_jitc()
        {
            eprintln!(
                "[cratonvm-jitc] first-call WAITING (optimizing osr body, entry_bci={entry_pc}) {class_name}.{method_name}{descriptor} on cell {cell:#x}"
            );
        }
        // Fired while the waiter was being noted: ask now.
        if some_first_call_cell_fired(&[cell]) {
            late_bind_key_served(shared, &key, "its first call ran during the OSR compile");
        }
    }
}

/// The late-bind waiter key of first-call cell `cell`.
pub(in crate::runtime::interpreter::jit_bridge) fn first_call_waiter_key(cell: usize) -> String {
    format!("#first-call:{cell:x}")
}

/// Remember the method `cp` compiled as waiting on every first-call cell the
/// compile was handed ([`CpResolvers::first_call`]), so the site's first
/// execution asks for a recompile ([`first_call_recompile_enabled`]).
pub(in crate::runtime::interpreter::jit_bridge) fn note_first_call_waiters(
    shared: &SharedVm,
    cp: &CpResolvers<'_>,
    waiting: &CachedBytecodeMethod,
) {
    let cells = std::mem::take(&mut *cp.first_calls_seen.borrow_mut());
    if cells.is_empty() || !first_call_recompile_enabled(shared) {
        return;
    }
    for cell in cells {
        let key = first_call_waiter_key(cell);
        if note_late_bind_waiter_key(shared, key.clone(), waiting)
            && crate::runtime::env_cache::dbg_jitc()
        {
            eprintln!(
                "[cratonvm-jitc] first-call WAITING {}.{}{} on cell {cell:#x}",
                waiting.class_name, waiting.method_name, waiting.method_descriptor
            );
        }
        // SAFETY: `cell` came from `first_call_cell_for`, which boxes every
        // cell in this VM's `first_call_cells` for the VM's life.
        let armed = unsafe { &*(cell as *const cratonvm_jit::JitFirstCallCell) }
            .armed
            .load(std::sync::atomic::Ordering::Acquire);
        if armed == 0 {
            // It ran while this compile was in flight, when a recompile it
            // asked for found this one queued and was refused: ask now.
            late_bind_key_served(shared, &key, "its first call ran during the compile");
        }
    }
}

/// `CRATONVM_JIT_DEFERRED_FIELD_RECOMPILE` (JIT round 21, lane tcshape; page
/// `jr21-typecheck-lane-perf-rows-20261014.md` row `late-r20-shape`; perf,
/// **opt-in** since the w3o A/B, `=1` arms it: it lost R21TcshapeLateInit
/// `unloaded` 1391 vs 149 ns and was flat elsewhere): is a body compiled with a DEFERRED
/// field site (`jit::helpers::jit_field_site_cp`: the owner was not loaded
/// when the method compiled, typically a cold branch) recompiled once that
/// site has linked; and is a single-pass OSR body that reads a static of a
/// class not initialized at its compile through `jit_getstatic` recompiled at
/// that class's `<clinit>` end? Neither had any trigger: the deferred site
/// re-ran its linkage (class-manager lock, loader-aware resolution) and the
/// access helper on every execution for the life of the body, and only the
/// method-entry door waited on a static holder's initialization
/// (`note_static_init_waiters`). HotSpot's C1 patches such a site in place at
/// its first execution; C2 traps there and recompiles. Read per compile that
/// deferred a site or read such a class (cold).
///
/// tcm21: this ONE opt-in also gates the two OSR static-init waiters
/// ([`note_osr_init_waiters`]'s static half and
/// [`note_osr_optimizing_static_init_waiters`]) that fix `TcuLateOsr`
/// `loaded-uninit` (the loop kept three helper reads for life in six runs of
/// ten), so that fix is inert by default although only the deferred-site half
/// was measured to lose. Whether to flip the switch, split it or close it is
/// the decision tree on
/// `tcm21-typecheck-miss-and-shape-gates-20261102.md` (gate G2).
pub(in crate::runtime::interpreter::jit_bridge) fn deferred_field_recompile_enabled() -> bool {
    false
}

/// The late-bind waiter key of the deferred field site `(holder, cp_idx)`.
pub(in crate::runtime::interpreter::jit_bridge) fn deferred_field_waiter_key(holder: u32, cp_idx: u16) -> String {
    format!("#deferred-field:{holder:x}:{cp_idx:x}")
}

/// May a body wait on the deferred field site `(holder, cp_idx)`? Records the
/// site as waited on ([`JitBridgeState::deferred_field_sites_waited`]) and
/// answers `true` unless its first linked execution already served it (a
/// body compiled after that still defers it because its owner cannot be
/// resolved by the compile doors: asking again would recompile it forever) or
/// the table is full.
pub(in crate::runtime::interpreter::jit_bridge) fn deferred_field_site_wait(shared: &SharedVm, holder: u32, cp_idx: u16) -> bool {
    let bridge = &shared.jit.bridge;
    let Ok(mut sites) = bridge.deferred_field_sites_waited.lock() else {
        return false;
    };
    match sites.get(&(holder, cp_idx)) {
        Some(&served) => !served,
        None => {
            if sites.len() >= LATE_BIND_CALLEE_CAP {
                return false;
            }
            sites.insert((holder, cp_idx), false);
            bridge
                .deferred_field_waiting_count
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            true
        }
    }
}

/// Remember the method `cp` compiled as waiting on every deferred field site
/// the compile built ([`CpResolvers::deferred_field_tag`]), so the site's first
/// linked execution asks for a recompile ([`deferred_field_site_linked`]),
/// which sees the owner loaded and binds the access. The method-entry twin of
/// [`note_osr_init_waiters`].
pub(in crate::runtime::interpreter::jit_bridge) fn note_deferred_field_waiters(
    shared: &SharedVm,
    cp: &CpResolvers<'_>,
    waiting: &CachedBytecodeMethod,
) {
    let cps = std::mem::take(&mut *cp.deferred_fields_seen.borrow_mut());
    if cps.is_empty() || !deferred_field_recompile_enabled() || !late_bind_recompile_enabled(shared)
    {
        return;
    }
    let holder = cp.class_id.as_u32();
    for cp_idx in cps {
        if !deferred_field_site_wait(shared, holder, cp_idx) {
            continue;
        }
        let key = deferred_field_waiter_key(holder, cp_idx);
        if note_late_bind_waiter_key(shared, key, waiting) && crate::runtime::env_cache::dbg_jitc()
        {
            eprintln!(
                "[cratonvm-jitc] deferred-field WAITING {}.{}{} on cp={cp_idx} of class id {holder}",
                waiting.class_name, waiting.method_name, waiting.method_descriptor
            );
        }
    }
}

/// The single-pass OSR door published a body of `class_id.names` entered at
/// `entry_pc` that compiled the pool indices `deferred_cps` as deferred field
/// sites and reads the statics of `static_init_classes` (some maybe through
/// `jit_getstatic`, for a class not initialized at its compile). Remember it
/// as an OSR late-bind waiter on each site and on each such class
/// ([`static_init_waiter_key`]), so the site's first linked execution
/// ([`deferred_field_site_linked`]) or the class's `<clinit>` end
/// ([`static_init_class_initialized`]) marks the cached OSR body stale and asks
/// an OSR compile at `entry_pc` ([`late_bind_osr_key_served`], EAGER19-4's
/// wake, which needs no switch of its own). The OSR body is what an
/// interpreted activation of the method enters at its loop, and nothing else
/// rebuilt it. Cold: once per published OSR body.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_init_waiters(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&Arc<str>, &Arc<str>, &Arc<str>),
    entry_pc: usize,
    static_init_classes: &[u32],
) {
    if static_init_classes.is_empty() || !late_bind_recompile_enabled(shared) {
        return;
    }
    let waiter = || OsrLateBindWaiter {
        class_id: class_id.as_u32(),
        class_name: Arc::clone(names.0),
        method_name: Arc::clone(names.1),
        descriptor: Arc::clone(names.2),
        entry_bci: u32::try_from(entry_pc).unwrap_or(u32::MAX),
        optimizing: false,
    };
    let dbg = crate::runtime::env_cache::dbg_jitc();
    let initialized = |id: u32| static_init_class_is_initialized(shared, id);
    for &static_class in static_init_classes {
        if static_class == 0 || initialized(static_class) {
            continue;
        }
        let key = static_init_waiter_key(static_class);
        if note_osr_late_bind_waiter_entry(shared, key.clone(), waiter()) && dbg {
            eprintln!(
                "[cratonvm-jitc] static-init WAITING (osr body, entry_bci={entry_pc}) {}.{}{} \
                 on class id {static_class}",
                names.0, names.1, names.2
            );
        }
        // Initialized while the waiter was being noted: its `<clinit>` end
        // may have found no waiter, so ask now.
        if initialized(static_class) {
            late_bind_key_served(shared, &key, "finished initializing during the OSR compile");
        }
    }
}

/// The optimizing OSR door's twin of [`note_osr_init_waiters`]'s static-init
/// half (2026-10-26, `jr21-typecheck-lane-perf-rows` retirement, row 6): an
/// accepted optimizing OSR body that reads a class not yet initialized at
/// build through `jit_getstatic` waits, as an `optimizing` OSR late-bind
/// waiter, on that class's `<clinit>` end. The wake marks the memo entry
/// stale and asks an OSR compile at `entry_pc` ([`late_bind_osr_key_served`]);
/// the rebuild reads the static directly, and its remembering arms the
/// running body's exit ([`arm_superseded_optimizing_osr_exits`]), so a
/// never-returning activation leaves the helper reads at its next back edge.
/// Without it such a loop (`TcuLateOsr loaded-uninit`) kept three helper reads
/// per iteration for life in about two runs in three: 64 us per 1000
/// iterations against 9-11. Under the same switches as the single-pass half
/// and INTERP719-2's optimizing waiter. Cold: once per accepted body.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_static_init_waiters(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    entry_pc: usize,
    artifact: &cratonvm_jit::CompiledMethod,
) {
    if artifact.static_init_classes.is_empty()
        || !late_bind_osr_optimizing_enabled()
        || !late_bind_recompile_enabled(shared)
    {
        return;
    }
    let initialized = |id: u32| static_init_class_is_initialized(shared, id);
    let pending: Vec<u32> = artifact
        .static_init_classes
        .iter()
        .copied()
        .filter(|&id| id != 0 && !initialized(id))
        .collect();
    if pending.is_empty() {
        return;
    }
    let (class_name, method_name, descriptor): (Arc<str>, Arc<str>, Arc<str>) =
        (names.0.into(), names.1.into(), names.2.into());
    let dbg = crate::runtime::env_cache::dbg_jitc();
    for static_class in pending {
        let waiter = OsrLateBindWaiter {
            class_id: class_id.as_u32(),
            class_name: Arc::clone(&class_name),
            method_name: Arc::clone(&method_name),
            descriptor: Arc::clone(&descriptor),
            entry_bci: u32::try_from(entry_pc).unwrap_or(u32::MAX),
            optimizing: true,
        };
        let key = static_init_waiter_key(static_class);
        if note_osr_late_bind_waiter_entry(shared, key.clone(), waiter) && dbg {
            eprintln!(
                "[cratonvm-jitc] static-init WAITING (optimizing osr body, entry_bci={entry_pc}) {class_name}.{method_name}{descriptor} on class id {static_class}"
            );
        }
        // Initialized while the waiter was being noted: its `<clinit>` end
        // may have found no waiter, so ask now.
        if initialized(static_class) {
            late_bind_key_served(shared, &key, "finished initializing during the OSR compile");
        }
    }
}

/// tcm21 (TCSHAPE21-1 census): count one execution of a deferred field site's
/// helper in this VM and answer the new total when it is a power of two (the
/// caller prints it), so a run's last line names the total within a factor of
/// two without a process-global counter or an exit hook.
pub(in crate::runtime::interpreter::jit_bridge) fn note_deferred_field_site_exec(bridge: &JitBridgeState) -> Option<u64> {
    let n = bridge
        .deferred_field_site_execs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .wrapping_add(1);
    n.is_power_of_two().then_some(n)
}

/// A compiled deferred field site of `holder`'s pool entry `cp_idx` has just
/// linked (`jit::helpers::jit_field_site_cp`): ask the recompile of every body
/// waiting on it ([`note_deferred_field_waiters`], [`note_osr_init_waiters`]),
/// once per site. One load when no body waits on any deferred site; otherwise
/// one lock per execution of a deferred site until the waiting bodies are
/// replaced.
pub(crate) fn deferred_field_site_linked(shared: &SharedVm, holder: u32, cp_idx: u16) {
    let bridge = &shared.jit.bridge;
    // tcm21 (TCSHAPE21-1 census): debug only; one cached flag read otherwise.
    if crate::runtime::env_cache::dbg_jitc() {
        if let Some(n) = note_deferred_field_site_exec(bridge) {
            eprintln!(
                "[cratonvm-jitc] deferred field site executions (helper path): {n} \
                 (last holder={holder} cp={cp_idx})"
            );
        }
    }
    if bridge
        .deferred_field_waiting_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return;
    }
    {
        let Ok(mut sites) = bridge.deferred_field_sites_waited.lock() else {
            return;
        };
        match sites.get_mut(&(holder, cp_idx)) {
            Some(served) if !*served => {
                *served = true;
                bridge
                    .deferred_field_waiting_count
                    .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
            _ => return,
        }
    }
    late_bind_key_served(
        shared,
        &deferred_field_waiter_key(holder, cp_idx),
        "its deferred field site linked",
    );
}

/// The dispatch helper has just cached a COMPILED body for a statically bound
/// call to `(callee_class, callee_method, callee_desc)` (`jit_invoke_dispatch`'s
/// two `DISPATCH_CACHE` fills, both behind the loader-faithful
/// `by_name_is_the_callers_class` test): the callee is loaded, initialized and
/// compiled, so every method remembered as waiting on it
/// ([`note_late_bind_waiter`]) would now bind it. Ask the tiered manager for one
/// more compile of each (the deferred-`new` retry's C2 door; its single-pass
/// fall-through binds more calls than the published body, so a refused
/// optimizing body still supersedes: `fall_through_resolves_more_calls`), and
/// mark it pending so its publication retires the body it displaces
/// ([`late_bind_retire_displaced`]). Each waiter is asked for once.
///
/// One relaxed load when nothing waits; otherwise one lock and a by-name key,
/// on a fill that runs once per thread per call site.
pub(crate) fn late_bind_site_served(
    shared: &SharedVm,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) {
    let key = late_bind_callee_key(callee_class, callee_method, callee_desc);
    late_bind_key_served(shared, &key, "is served compiled now");
}

/// [`late_bind_site_served`] for the waiters remembered under `key`; `why`
/// names the event in the `CRATONVM_DBG_JITC` line.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_key_served(shared: &SharedVm, key: &str, why: &str) {
    // EAGER19-4 (`CRATONVM_JIT_LATE_BIND_OSR`): the OSR bodies waiting on the
    // same callee; one relaxed load when none does.
    late_bind_osr_key_served(shared, key, why);
    let waiting = late_bind_take_waiters(shared, key);
    // With no background worker nothing would run the task, and with no C1 ->
    // C2 supersede the recompile could not replace the published body.
    if waiting.is_empty()
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    for waiter in waiting {
        // CF-W8-2: a cold-callee waiter recompiles its caller at most once.
        if !late_bind_cold_fire_admits(shared, &waiter, key) {
            continue;
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] late-bind recompile requested {}.{}{}: {key} {why}",
                waiter.class_name, waiter.method_name, waiter.descriptor
            );
        }
        late_bind_mark_and_request(shared, &waiter);
    }
}

/// Take every method remembered as waiting on `(callee_class, callee_method,
/// callee_desc)` ([`note_late_bind_waiter`]); empty when none. One relaxed load
/// when nothing waits anywhere in the VM.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_take_waiters(shared: &SharedVm, key: &str) -> Vec<LateBindWaiter> {
    let bridge = &shared.jit.bridge;
    if bridge
        .late_bind_waiter_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return Vec::new();
    }
    let Ok(mut waiters) = bridge.late_bind_waiters.lock() else {
        return Vec::new();
    };
    let taken = waiters.remove(key).unwrap_or_default();
    bridge
        .late_bind_waiter_count
        .store(waiters.len(), std::sync::atomic::Ordering::Release);
    taken
}

/// `CRATONVM_JIT_LATE_BIND_OSR` (JIT round 19 proposal wave 7, lane interp7,
/// EAGER19-4 of `jit-r19-eager19-proposals.md` -- TP16-22's OSR half; **default
/// OFF**, `1` enables; effective only with [`late_bind_recompile_enabled`]):
/// see [`note_osr_late_bind_waiter`]. Read on an OSR door refusal and on a
/// wake (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_osr_enabled() -> bool {
    // WARNING: tomcat-late-osr-default-flip: default ON (`=0` kills), was opt-in.
    true
}

/// An OSR body waiting on a callee ([`JitBridgeState::late_bind_osr_waiters`]):
/// the method whose single-pass OSR body left the call on the dispatch helper,
/// and the loop header that compile was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct OsrLateBindWaiter {
    pub(in crate::runtime::interpreter::jit_bridge) class_id: u32,
    pub(in crate::runtime::interpreter::jit_bridge) class_name: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) method_name: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) descriptor: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) entry_bci: u32,
    /// INTERP719-2 (wave 8, lane osr8): the waiting body is an OPTIMIZING OSR
    /// body (the accepted-artifact memo's), not the single-pass one in
    /// `JitCache`; its wake forgets the memo entry
    /// ([`late_bind_osr_optimizing_stale_taken`]) instead of marking the
    /// cached single-pass body stale.
    pub(in crate::runtime::interpreter::jit_bridge) optimizing: bool,
}

/// `CRATONVM_JIT_LATE_BIND_OSR_OPTIMIZING` (JIT round 19 proposal wave 8,
/// lane osr8, INTERP719-2 of `jit-r19-interp7-proposals-RETIRED-20261009.md`; **default ON** since
/// the tomcat-late-osr flip, `=0` kills; effective only with [`late_bind_osr_enabled`] and
/// [`late_bind_recompile_enabled`]): see
/// [`note_osr_optimizing_late_bind_waiter`]. Read on a compile-time refusal
/// (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_osr_optimizing_enabled() -> bool {
    // WARNING: tomcat-late-osr-default-flip: default ON (`=0` kills), was opt-in.
    true
}

/// INTERP719-2 ([`late_bind_osr_optimizing_enabled`]): the OPTIMIZING OSR
/// door's compile (`compile_optimizing_artifact` with `osr_door`, entered at
/// `entry_pc`) left the statically bound call to `callee` unbound for a
/// refusal that may lift. EAGER19-4's waiter covers only the single-pass OSR
/// door; an optimizing OSR body lives in the accepted-artifact memo
/// (`osr_optimizing_accepted`), which neither the single-pass stale mark nor
/// the method-entry late bind reaches, and the worker's optimizing route
/// returns early on a memo hit -- so the body kept the dispatch helper for
/// the life of the entry. Remembered here like EAGER19-4's waiter, marked
/// `optimizing`: the callee's first compiled serve then forgets the memo
/// entry at its next lookup ([`late_bind_osr_optimizing_stale_taken`], the
/// al16 first-call forget's route) and asks the same OSR compile at
/// `entry_pc`. Cold: one compile-time refusal.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_late_bind_waiter(
    shared: &SharedVm,
    refusal: cratonvm_jit::DirectBindRefusal,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting: &CachedBytecodeMethod,
    entry_pc: usize,
) {
    if !late_bind_refusal_may_lift(refusal)
        || !late_bind_osr_optimizing_enabled()
        || !late_bind_osr_enabled()
        || !late_bind_recompile_enabled(shared)
    {
        return;
    }
    let waiter = OsrLateBindWaiter {
        class_id: waiting.declaring_class_id.as_u32(),
        class_name: Arc::clone(&waiting.class_name),
        method_name: Arc::clone(&waiting.method_name),
        descriptor: Arc::clone(&waiting.method_descriptor),
        entry_bci: u32::try_from(entry_pc).unwrap_or(u32::MAX),
        optimizing: true,
    };
    let key = late_bind_callee_key(callee_class, callee_method, callee_desc);
    if note_osr_late_bind_waiter_entry(shared, key, waiter) && crate::runtime::env_cache::dbg_jitc()
    {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING (optimizing osr body, entry_bci={entry_pc}) {}.{}{} on \
             {callee_class}.{callee_method}{callee_desc} ({refusal:?})",
            waiting.class_name, waiting.method_name, waiting.method_descriptor
        );
    }
}

/// Remember `waiter` under `key` in [`JitBridgeState::late_bind_osr_waiters`]
/// with its caps; `true` when it was added. Shared by both OSR waiters.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_late_bind_waiter_entry(
    shared: &SharedVm,
    key: String,
    waiter: OsrLateBindWaiter,
) -> bool {
    let bridge = &shared.jit.bridge;
    let Ok(mut waiters) = bridge.late_bind_osr_waiters.lock() else {
        return false;
    };
    if waiters.len() >= LATE_BIND_CALLEE_CAP && !waiters.contains_key(&key) {
        return false;
    }
    let list = waiters.entry(key).or_default();
    let added = list.len() < LATE_BIND_WAITERS_PER_CALLEE && !list.contains(&waiter);
    if added {
        list.push(waiter);
    }
    bridge
        .late_bind_osr_waiter_count
        .store(waiters.len(), std::sync::atomic::Ordering::Release);
    added
}

/// Is a late-bind forget mark pending for the optimizing OSR memo entry `key`
/// ([`late_bind_osr_optimizing_stale_taken`] without taking it)? One relaxed
/// load when nothing is marked.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_osr_optimizing_stale_pending(shared: &SharedVm, key: (u32, u64, usize)) -> bool {
    let bridge = &shared.jit.bridge;
    bridge
        .late_bind_osr_optimizing_stale_count
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
        && bridge
            .late_bind_osr_optimizing_stale
            .lock()
            .is_ok_and(|stale| stale.contains(&key))
}

/// INTERP719-2: take the late-bind forget mark of the optimizing OSR memo
/// entry `key` (`osr_optimizing_refusal_key`'s shape), set by
/// [`late_bind_osr_key_served`] for an optimizing waiter: `true` once. One
/// relaxed load when nothing is marked.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_osr_optimizing_stale_taken(shared: &SharedVm, key: (u32, u64, usize)) -> bool {
    let bridge = &shared.jit.bridge;
    if bridge
        .late_bind_osr_optimizing_stale_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return false;
    }
    let Ok(mut stale) = bridge.late_bind_osr_optimizing_stale.lock() else {
        return false;
    };
    let taken = stale.remove(&key);
    bridge
        .late_bind_osr_optimizing_stale_count
        .store(stale.len(), std::sync::atomic::Ordering::Release);
    taken
}

/// EAGER19-4 ([`late_bind_osr_enabled`]): the single-pass OSR door
/// (`compile_osr_body`) left the statically bound call to `callee` on the
/// dispatch helper for `refusal`. When the refusal is a compile-ORDER accident
/// ([`late_bind_refusal_may_lift`]: the callee's class not loaded, a static
/// callee's class not initialized), remember the OSR body. The callee's first
/// compiled serve by the dispatch helper ([`late_bind_site_served`]) then asks
/// an OSR recompile of the method at `entry_bci` whose publication REPLACES
/// the cached OSR body ([`late_bind_osr_key_served`]); the method-entry
/// late bind (TP16-20) would buy a method-entry body the looping frame never
/// enters. A frame already running the old OSR body keeps it until its next
/// OSR entry (MEGAOSR19-3's early re-entry makes that the next activation's
/// 32nd back edge). Cold: one compile-time refusal.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_late_bind_waiter(
    shared: &SharedVm,
    refusal: cratonvm_jit::DirectBindRefusal,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting_class_id: ClassId,
    waiting: (&Arc<str>, &Arc<str>, &Arc<str>),
    entry_bci: usize,
) {
    if !late_bind_refusal_may_lift(refusal)
        || !late_bind_osr_enabled()
        || !late_bind_recompile_enabled(shared)
    {
        return;
    }
    let waiter = OsrLateBindWaiter {
        class_id: waiting_class_id.as_u32(),
        class_name: Arc::clone(waiting.0),
        method_name: Arc::clone(waiting.1),
        descriptor: Arc::clone(waiting.2),
        entry_bci: u32::try_from(entry_bci).unwrap_or(u32::MAX),
        optimizing: false,
    };
    let key = late_bind_callee_key(callee_class, callee_method, callee_desc);
    // Wave 8 (lane osr8): the insertion is shared with INTERP719-2's
    // optimizing waiter (`note_osr_late_bind_waiter_entry`); same caps.
    let added = note_osr_late_bind_waiter_entry(shared, key, waiter);
    if added && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING (osr body, entry_bci={entry_bci}) {}.{}{} on \
             {callee_class}.{callee_method}{callee_desc} ({refusal:?})",
            waiting.0, waiting.1, waiting.2
        );
    }
}

/// EAGER19-4: the callee named `key` is served compiled now. For every OSR
/// body waiting on it ([`note_osr_late_bind_waiter`]), mark the method's
/// cached OSR body stale for the background door
/// ([`take_late_bind_osr_stale`]) and ask an OSR compile at its entry bci
/// (`TieredCompilationManager::request_osr_classified`, the back edge's own
/// request). Each waiter is asked for once. One relaxed load when no OSR body
/// waits anywhere in the VM.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_osr_key_served(shared: &SharedVm, key: &str, why: &str) {
    let bridge = &shared.jit.bridge;
    if bridge
        .late_bind_osr_waiter_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return;
    }
    let waiting = {
        let Ok(mut waiters) = bridge.late_bind_osr_waiters.lock() else {
            return;
        };
        let taken = waiters.remove(key).unwrap_or_default();
        bridge
            .late_bind_osr_waiter_count
            .store(waiters.len(), std::sync::atomic::Ordering::Release);
        taken
    };
    // As the method-entry wake: with no background worker nothing would run
    // the task.
    if waiting.is_empty() || !crate::runtime::env_cache::bg_compile() {
        return;
    }
    for waiter in waiting {
        // INTERP719-2 (wave 8, lane osr8): an optimizing OSR body is forgotten
        // from the accepted-artifact memo at its next lookup instead
        // (`osr_optimizing_cached`), so the door -- or the background task
        // asked below -- builds it again with the callee bound.
        if waiter.optimizing {
            let opt_key = osr_optimizing_refusal_key(
                shared.vm_identity,
                ClassId::new(waiter.class_id),
                &waiter.method_name,
                &waiter.descriptor,
                // Cast: a bytecode index, widened.
                waiter.entry_bci as usize,
            );
            if let Ok(mut stale) = bridge.late_bind_osr_optimizing_stale.lock() {
                if stale.len() < LATE_BIND_CALLEE_CAP {
                    stale.insert(opt_key);
                }
                bridge
                    .late_bind_osr_optimizing_stale_count
                    .store(stale.len(), std::sync::atomic::Ordering::Release);
            }
        } else if let Ok(mut stale) = bridge.late_bind_osr_stale.lock() {
            if stale.len() < LATE_BIND_CALLEE_CAP {
                stale.insert((
                    waiter.class_id,
                    Arc::clone(&waiter.method_name),
                    Arc::clone(&waiter.descriptor),
                ));
            }
            bridge
                .late_bind_osr_stale_count
                .store(stale.len(), std::sync::atomic::Ordering::Release);
        }
        // A first-call wake while other sites of the class are still
        // unresolved: they fire in a burst (a loop's first trip), and a
        // rebuild per site measured 20 ms on a 170 ms run. The mark stays;
        // the last site's wake asks. A site that never fires leaves the body
        // as it was before these waiters existed.
        if waiter.optimizing
            && key.starts_with("#first-call:")
            && !armed_first_call_cells_of(shared, waiter.class_id).is_empty()
        {
            continue;
        }
        ensure_bg_compiler_started(shared);
        let request = shared.jit.tiered_manager.request_osr_classified(
            &crate::jit::tiered::MethodKey::with_class_id(
                ClassId::new(waiter.class_id),
                Arc::clone(&waiter.class_name),
                Arc::clone(&waiter.method_name),
                Arc::clone(&waiter.descriptor),
            ),
            waiter.entry_bci,
        );
        resolve_admitted_compile_strings(shared);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] late-bind OSR recompile requested {}.{}{} entry_bci={}: {key} {why} ({})",
                waiter.class_name,
                waiter.method_name,
                waiter.descriptor,
                waiter.entry_bci,
                match request {
                    crate::jit::tiered::OsrRequest::Queued(_) => "queued",
                    crate::jit::tiered::OsrRequest::Pending => "pending",
                    crate::jit::tiered::OsrRequest::Refused => "refused",
                },
            );
        }
    }
}

/// EAGER19-4: take `(class_id, method, descriptor)`'s late-bind stale mark
/// ([`late_bind_osr_key_served`]): `true` once, for the background OSR compile
/// that then compiles a fresh body instead of reusing the cached one. One
/// relaxed load when nothing is marked.
pub(in crate::runtime::interpreter::jit_bridge) fn take_late_bind_osr_stale(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &Arc<str>,
    descriptor: &Arc<str>,
) -> bool {
    let bridge = &shared.jit.bridge;
    if bridge
        .late_bind_osr_stale_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return false;
    }
    let Ok(mut stale) = bridge.late_bind_osr_stale.lock() else {
        return false;
    };
    let taken = stale.remove(&(
        class_id.as_u32(),
        Arc::clone(method_name),
        Arc::clone(descriptor),
    ));
    bridge
        .late_bind_osr_stale_count
        .store(stale.len(), std::sync::atomic::Ordering::Release);
    taken
}

/// OSRENTRY6-2 (JIT round 19 proposal wave 7, lane interp7): why the OSR
/// door's statically bound call to `callee_class.callee_method callee_desc`
/// (site kind `site_invoke_kind`: 3 `invokestatic`, 1 `invokespecial`) got no
/// compiled callee from `try_jit_compile_callee_from_site`, which answers a
/// bare `None`. Classified here from the class state the door can see, in the
/// order the slow door asks: the callee class not resolvable in `holder`'s
/// loader, the method not found, a static callee's declaring class not
/// initialized, a synchronized callee (no self-locking body), else
/// `CalleeNotYetCompiled` (cold, declined, or another arm of the slow door).
/// Cold: compile time.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_unbound_callee_reason(
    shared: &SharedVm,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    site_invoke_kind: u8,
) -> cratonvm_jit::DirectBindRefusal {
    use cratonvm_jit::DirectBindRefusal as R;
    let cm = shared.classes.class_manager.read();
    let Some(callee_cid) = jit_known_class(shared, &cm, callee_class, holder) else {
        return R::CalleeClassNotFound;
    };
    let store = cm.class_store();
    let Some((method, declaring_id)) =
        crate::classloading::find_method_recursive(callee_cid, callee_method, callee_desc, store)
    else {
        return R::CalleeMethodNotFound;
    };
    if site_invoke_kind == 3
        && !store
            .get(declaring_id)
            .is_some_and(crate::vm::is_class_initialized_fast)
    {
        return R::DeclaringClassNotInitialized;
    }
    if method.is_synchronized() {
        return R::Synchronized;
    }
    R::CalleeNotYetCompiled
}

/// OSRENTRY6-2: count one OSR-door site left on the dispatch helper under
/// `reason` in this VM's own table ([`JitBridgeState::osr_door_bind_refusals`]).
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_door_bind_refusal(shared: &SharedVm, reason: cratonvm_jit::DirectBindRefusal) {
    if let Some(cell) = shared
        .jit
        .bridge
        .osr_door_bind_refusals
        .get(reason as usize)
    {
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// OSRENTRY6-2: this VM's OSR-door refusal census, `(reason, count)` for every
/// non-zero reason, largest first. Unlike the process table
/// (`cratonvm_jit::direct_callee_bind_refusal_reasons`) it holds the OSR door
/// alone and names the reason of a slow-door `None`
/// ([`osr_unbound_callee_reason`]) instead of `callee-not-yet-compiled`.
pub(crate) fn osr_door_bind_refusal_reasons(shared: &SharedVm) -> Vec<(&'static str, u64)> {
    let mut out: Vec<(&'static str, u64)> = cratonvm_jit::DIRECT_BIND_REFUSAL_NAMES
        .iter()
        .zip(shared.jit.bridge.osr_door_bind_refusals.iter())
        .map(|(name, cell)| (*name, cell.load(std::sync::atomic::Ordering::Relaxed)))
        .filter(|(_, count)| *count > 0)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Cap of [`JitBridgeState::osr_method_backedges`]: past it a new method is
/// simply not accumulated (it keeps the per-frame schedule).
pub(in crate::runtime::interpreter::jit_bridge) const OSR_METHOD_BACKEDGE_KEYS_MAX: usize = 16384;

/// OSRENTRY6-3 (`CRATONVM_JIT_OSR_EARLY_REQUEST`): add `backedges` -- an
/// interpreted activation's `Frame::backward_count` at its pop -- to its
/// method's summed back edges (HotSpot's per-METHOD `backedge_counter`; the
/// interpreter's own count is per frame). One read lock and one relaxed add
/// per looping activation once the method has a row.
pub(crate) fn note_osr_method_backedges(shared: &SharedVm, key: u128, backedges: u32) {
    let _ = add_method_backedges(&shared.jit.bridge.osr_method_backedges, key, backedges);
}

/// The short-loop page's `i + b` (lane mon11, wave 11;
/// `CRATONVM_JIT_TIERUP_I_PLUS_B`): add `backedges` -- an interpreted
/// activation's `Frame::backward_count` at its pop -- to its method's RAW
/// summed back edges and return the new sum; `None` once the map is full and
/// the method has no row. Never reset.
pub(crate) fn note_tierup_method_backedges(
    shared: &SharedVm,
    key: u128,
    backedges: u32,
) -> Option<u32> {
    add_method_backedges(&shared.jit.bridge.tierup_method_backedges, key, backedges)
}

/// One saturating add of `backedges` to `key`'s row of a per-method back-edge
/// map, inserting the row while the map is under
/// [`OSR_METHOD_BACKEDGE_KEYS_MAX`]. Returns the row's new value, or `None`
/// when the map is full and `key` has no row. One read lock and one relaxed
/// add once the row exists.
pub(in crate::runtime::interpreter::jit_bridge) fn add_method_backedges(
    map: &parking_lot::RwLock<std::collections::HashMap<u128, std::sync::atomic::AtomicU32>>,
    key: u128,
    backedges: u32,
) -> Option<u32> {
    let add = |cell: &std::sync::atomic::AtomicU32| {
        let prev = cell
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |v| Some(v.saturating_add(backedges)),
            )
            .unwrap_or_else(|v| v);
        prev.saturating_add(backedges)
    };
    {
        let read = map.read();
        if let Some(cell) = read.get(&key) {
            return Some(add(cell));
        }
    }
    let mut write = map.write();
    if write.len() >= OSR_METHOD_BACKEDGE_KEYS_MAX && !write.contains_key(&key) {
        return None;
    }
    let cell = write
        .entry(key)
        .or_insert_with(|| std::sync::atomic::AtomicU32::new(0));
    Some(add(cell))
}

/// OSRENTRY6-3: has the method `key` summed at least `threshold` back edges
/// since its last early request? `true` resets its sum to zero (one request
/// per `threshold` summed back edges, as HotSpot's counter overflow asks once
/// and decays), so concurrent activations do not all ask.
pub(crate) fn take_osr_method_backedges(shared: &SharedVm, key: u128, threshold: u32) -> bool {
    let read = shared.jit.bridge.osr_method_backedges.read();
    let Some(cell) = read.get(&key) else {
        return false;
    };
    cell.try_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| (v >= threshold).then_some(0),
    )
    .is_ok()
}

/// Mark `waiter` as asked for: its next publication retires the body it
/// displaces ([`late_bind_take_pending`]).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_mark_pending(shared: &SharedVm, waiter: &LateBindWaiter) {
    let bridge = &shared.jit.bridge;
    let Ok(mut pending) = bridge.late_bind_pending.lock() else {
        return;
    };
    let key = (
        waiter.class_id,
        Arc::clone(&waiter.method_name),
        Arc::clone(&waiter.descriptor),
    );
    // JIT round 19 wave 2, lane eager2 (EAGER19-3): stamp the mark with a
    // fresh generation, under the set's lock, so a publication can tell a
    // compile that started before this request from one started after it.
    if late_bind_pending_stamp_enabled() {
        let stamp = bridge
            .late_bind_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if let Ok(mut stamps) = bridge.late_bind_pending_stamps.lock() {
            stamps.insert(key.clone(), stamp);
        }
    }
    pending.insert(key);
    bridge
        .late_bind_pending_count
        .store(pending.len(), std::sync::atomic::Ordering::Release);
}

/// Withdraw `waiter`'s late-bind pending mark (and its EAGER19-3 stamp), the
/// undo of [`late_bind_mark_pending`]. JIT round 20, lane stash (EAGER2-2).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_unmark_pending(shared: &SharedVm, waiter: &LateBindWaiter) {
    let bridge = &shared.jit.bridge;
    let Ok(mut pending) = bridge.late_bind_pending.lock() else {
        return;
    };
    let key = (
        waiter.class_id,
        Arc::clone(&waiter.method_name),
        Arc::clone(&waiter.descriptor),
    );
    pending.remove(&key);
    if let Ok(mut stamps) = bridge.late_bind_pending_stamps.lock() {
        stamps.remove(&key);
    }
    bridge
        .late_bind_pending_count
        .store(pending.len(), std::sync::atomic::Ordering::Release);
}



/// Mark `waiter` pending ([`late_bind_mark_pending`]) and ask the tiered
/// manager for its late-bind recompile.
///
/// JIT round 20, lane stash (EAGER2-2;
/// `r19-late-bind-pending-marks-outlive-a-refused-retry-FIXED-20261012.md`): the C2
/// retry door refuses silently (`c2_bailout`, ineligible, the retry budget,
/// the hotness gate, tiering disabled, the code-cache cap), and a mark left
/// behind by a refusal had no bound and no expiry: the method's next
/// publication, whatever compile produced it and however much later, was
/// taken for the late-bind republish and retired the body it displaced. The
/// mark is set BEFORE the request (a compile the request starts must find it,
/// and the EAGER19-3 stamp must predate that compile) and withdrawn when the
/// answer is [`crate::jit::tiered::C2RetryAnswer::Refused`]. `AlreadyQueued`
/// keeps it: the queued compile publishes (the EAGER19-3 race, which its stamp
/// switch decides).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_mark_and_request(
    shared: &SharedVm,
    waiter: &LateBindWaiter,
) -> crate::jit::tiered::C2RetryAnswer {
    late_bind_mark_pending(shared, waiter);
    let answer = shared.jit.tiered_manager.request_late_bind_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            ClassId::new(waiter.class_id),
            Arc::clone(&waiter.class_name),
            Arc::clone(&waiter.method_name),
            Arc::clone(&waiter.descriptor),
        ),
    );
    if !answer.compile_will_run() {
        late_bind_unmark_pending(shared, waiter);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] late-bind mark WITHDRAWN {}.{}{}: the recompile was refused",
                waiter.class_name, waiter.method_name, waiter.descriptor
            );
        }
    }
    answer
}

/// JIT round 21 wave 1 (lane indy, `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`;
/// the caller is `invokedynamic`'s retarget miss count, which bounds it): one
/// recompile of the method that owns a `MutableCallSite` / `VolatileCallSite`
/// instruction whose compiled guarded call planned a target the call site no
/// longer holds -- HotSpot's `setTarget` dependency, taken lazily. Through the
/// late-bind door ([`late_bind_mark_and_request`]): the C2 retry with the
/// pending mark, so the recompile's publication retires the displaced body and
/// its baked callers forward. Needs the background worker and the C1 -> C2
/// supersede, as every late-bind recompile does. Returns whether a compile
/// will run.
pub(crate) fn indy_mutable_retarget_recompile(
    shared: &SharedVm,
    class_id: u32,
    class_name: Arc<str>,
    method_name: Arc<str>,
    descriptor: Arc<str>,
) -> bool {
    if !crate::runtime::env_cache::bg_compile() || !crate::runtime::env_cache::c2_supersede() {
        return false;
    }
    let waiter = LateBindWaiter {
        class_id,
        class_name,
        method_name,
        descriptor,
    };
    late_bind_mark_and_request(shared, &waiter).compile_will_run()
}



/// The most caller artifacts [`stale_caller_recompile_on_stub_pass`] remembers
/// asking for; past it no more are asked (today's behaviour).
pub(in crate::runtime::interpreter::jit_bridge) const STALE_CALLER_RECOMPILE_CAP: usize = 4096;

/// JIT round 20 wave 11, lane pLate (W16L1-3): a call has just come through a
/// not-entrant stub's slow path (`jit_not_entrant_entry`) from the compiled
/// code at `ret_pc` -- a caller that BAKED the displaced callee's entry (a
/// direct bind, a splice's call). The stub forwards it (or, when the forward
/// is refused, re-dispatches it through the VM on EVERY call); HotSpot instead
/// pays the wrong-method stub once per call site and re-binds the caller.
///
/// When `ret_pc` lies in a method-entry body that is still the one published
/// for its method (not an OSR body, not itself superseded or retired), ask the
/// tiered manager for ONE recompile of that method through the late-bind door
/// ([`late_bind_mark_and_request`]: the C2 retry, with the pending mark that
/// makes the next publication retire the stale caller so ITS callers forward
/// too). The recompile's direct bind then bakes the callee's current body.
///
/// Bounded: once per caller artifact (by entry, [`JitBridgeState::stale_caller_recompiles`],
/// at most [`STALE_CALLER_RECOMPILE_CAP`]), never while an agent may need
/// methods interpreted, and only with the background worker and the C1 -> C2
/// supersede that a late-bind recompile needs. Returns whether it asked.
///
/// Never for a self-recursive call (`stale_callee`, the stub's method, is the
/// caller's own method): the recompile would bake the caller -- the body it
/// replaces -- as its own stale callee, and so on at every publication (a
/// recompile loop; the self rebind of `JitCache::rebind_stale_call_site`
/// serves that call instead, or the stub's forward does).
pub(crate) fn stale_caller_recompile_on_stub_pass(
    shared: &SharedVm,
    ret_pc: usize,
    stale_callee: (&str, &str, &str, u32),
) -> bool {
    if ret_pc == 0
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
        || crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(shared)
    {
        return false;
    }
    // The caller's frame is live (we return into it), so its body is pinned
    // by the code-range registry.
    let Some(caller) = cratonvm_jit::pin_jit_code_range_owner(ret_pc) else {
        return false;
    };
    let asked = stale_caller_recompile_for_pinned(shared, &caller, ret_pc, stale_callee);
    // Only a pin that is the LAST reference goes through the retirement queue.
    // A counted `RetainedCode` pin was the last holder of every superseded
    // caller, so a stale activation looping over a not-entrant callee queued
    // its own body once per call and each queueing re-partitioned the whole
    // queue (the J27Latin1CharRows `--real-jdk` hang).
    if Arc::strong_count(&caller) == 1 {
        drop(cratonvm_jit::RetainedCode::new(caller));
    }
    asked
}

pub(in crate::runtime::interpreter::jit_bridge) fn stale_caller_recompile_for_pinned(
    shared: &SharedVm,
    caller: &Arc<cratonvm_jit::CompiledMethod>,
    ret_pc: usize,
    stale_callee: (&str, &str, &str, u32),
) -> bool {
    if caller.superseded.load(std::sync::atomic::Ordering::Acquire)
        || caller.retired.load(std::sync::atomic::Ordering::Acquire)
    {
        return false;
    }
    let Some((class_name, method_name, descriptor, class_id)) = caller.published_key_parts() else {
        return false;
    };
    if (&*class_name, &*method_name, &*descriptor, class_id) == stale_callee {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] stale-caller recompile declined {class_name}.{method_name}{descriptor} \
                 (ret={ret_pc:#x}): a self-recursive call"
            );
        }
        return false;
    }
    // Only the method-entry body the cache publishes now: an OSR body lives in
    // the OSR map (`get` never answers it), and a body some other publication
    // displaced is already being replaced.
    let current = shared.jit.jit_cache.get(
        &class_name,
        &method_name,
        &descriptor,
        ClassId::new(class_id),
    );
    let is_current = current
        .as_ref()
        .is_some_and(|body| Arc::ptr_eq(body, caller));
    drop(current.map(cratonvm_jit::RetainedCode::new));
    if !is_current {
        return false;
    }
    // Cast: an address, the once-per-artifact key.
    let caller_entry = caller.entry_ptr() as usize;
    {
        let Ok(mut asked) = shared.jit.bridge.stale_caller_recompiles.lock() else {
            return false;
        };
        if asked.len() >= STALE_CALLER_RECOMPILE_CAP || !asked.insert(caller_entry) {
            return false;
        }
    }
    let waiter = LateBindWaiter {
        class_id,
        class_name,
        method_name,
        descriptor,
    };
    let answer = late_bind_mark_and_request(shared, &waiter);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] stale-caller recompile requested {}.{}{} (ret={ret_pc:#x}, entry={caller_entry:#x}): {}",
            waiter.class_name,
            waiter.method_name,
            waiter.descriptor,
            if answer.compile_will_run() { "will run" } else { "refused" },
        );
    }
    true
}

/// The most times one method's late-bind mark is put back after an
/// unpublished republish ([`late_bind_restore_after_unpublished_put`]).
pub(in crate::runtime::interpreter::jit_bridge) const LATE_BIND_RESTORES_PER_METHOD: u8 = 2;



/// A late-bind republish of `cached` TOOK the method's pending mark, but its
/// `put` did not publish the offered body (`JitCache::put` refused it -- an
/// install-epoch or bytecode-read witness, the code-cache cap, R12-4's
/// single-pass-over-optimizing refusal --, a concurrent publish won, or the
/// VarHandle-stale skip ran): the read-back is another body. The mark was
/// taken BEFORE the `put` and the waiter at the dispatch fill, so nothing
/// would ever ask again and the site stayed on the dispatch helper for the
/// life of the body, silently.
///
/// JIT round 20, lane stash (EAGER2-1;
/// `r19-late-bind-mark-is-consumed-before-a-put-that-may-refuse-FIXED-20261012.md`):
/// put the mark back and re-ask the recompile through
/// [`late_bind_mark_and_request`]. A background task of this method holds its
/// slot (the answer is `AlreadyQueued`), so the method is also remembered in
/// [`JitBridgeState::late_bind_owed`] and the worker asks the same C2 retry
/// after `finish`, exactly as [`late_bind_note_older_publication`] arranges.
/// Bounded: at most [`LATE_BIND_RESTORES_PER_METHOD`] restores per method
/// ([`JitBridgeState::late_bind_restores`]; a refusal R12-4 makes is read back
/// as a publish and charges no retry budget), and a refused retry withdraws
/// the mark (EAGER2-2).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_restore_after_unpublished_put(shared: &SharedVm, cached: &CachedBytecodeMethod) {
    let admitted = match shared.jit.bridge.late_bind_restores.lock() {
        Ok(mut restores) => {
            let key = (
                cached.declaring_class_id.as_u32(),
                Arc::clone(&cached.method_name),
                Arc::clone(&cached.method_descriptor),
            );
            if !restores.contains_key(&key) && restores.len() >= LATE_BIND_CALLEE_CAP {
                false
            } else {
                let spent = restores.entry(key).or_insert(0);
                let room = *spent < LATE_BIND_RESTORES_PER_METHOD;
                if room {
                    *spent += 1;
                }
                room
            }
        }
        Err(_) => false,
    };
    if !admitted {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] late-bind mark LOST {}.{}{}: the republish's put did not publish \
                 the offered body, and the restore budget is spent",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind mark RESTORED {}.{}{}: the republish's put did not \
             publish the offered body; recompile asked again",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    let waiter = LateBindWaiter {
        class_id: cached.declaring_class_id.as_u32(),
        class_name: Arc::clone(&cached.class_name),
        method_name: Arc::clone(&cached.method_name),
        descriptor: Arc::clone(&cached.method_descriptor),
    };
    let answer = late_bind_mark_and_request(shared, &waiter);
    // j27 w6 T1TIERMGR (TIERMGR21-1): an `AlreadyQueued` answer is remembered
    // here only when the manager does not own it (`CRATONVM_JIT_ONE_OWED_RETRY`).
    late_bind_owe_vm_copy(
        shared,
        (
            waiter.class_id,
            Arc::clone(&waiter.method_name),
            Arc::clone(&waiter.descriptor),
        ),
        answer,
        false,
        false,
        "refused-put restore",
    );
}

/// `CRATONVM_JIT_LATE_BIND_PENDING_STAMP` (JIT round 19 wave 2, lane eager2,
/// EAGER19-3 of `jit-r19-eager19-proposals.md`; **default OFF**): a late-bind
/// pending mark is consumed only by a publication whose compile STARTED after
/// the request ([`late_bind_take_pending_started`]).
///
/// Without it the FIRST publication of the method after the request takes the
/// mark, whatever compile produced it: a C2 task already compiling when the
/// callee was first served (so built with the site on the dispatch helper) can
/// publish first, consume the mark, and the request it raced was refused
/// (`already queued`), so nothing compiles the method again and the site stays
/// on the helper. With it that older publication leaves the mark and owes the
/// recompile once more ([`late_bind_note_older_publication`]). Read per
/// pending mark and per publication that finds one (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_pending_stamp_enabled() -> bool {
    false
}

/// The late-bind generation a compile starting NOW records
/// ([`late_bind_take_pending_started`]); `u64::MAX` with the stamp switch off,
/// so every publication consumes as before. One load.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_compile_started(shared: &SharedVm) -> u64 {
    if !late_bind_pending_stamp_enabled() {
        return u64::MAX;
    }
    shared
        .jit
        .bridge
        .late_bind_generation
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// What a publication found among the late-bind pending marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) enum LateBindPendingTake {
    /// No mark for the method.
    None,
    /// A mark this publication's compile answers: it is taken.
    Taken,
    /// A mark set AFTER this publication's compile started (EAGER19-3): it is
    /// kept for the compile the request asked for.
    Older,
}

/// [`late_bind_take_pending`] for a compile that started at late-bind
/// generation `started` ([`late_bind_compile_started`]): the mark is taken
/// only when the compile began at or after it was set; an older compile
/// leaves it ([`LateBindPendingTake::Older`]). A mark with no stamp (set with
/// the switch off) is taken as before.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_take_pending_started(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    started: u64,
) -> LateBindPendingTake {
    let bridge = &shared.jit.bridge;
    if bridge
        .late_bind_pending_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return LateBindPendingTake::None;
    }
    let Ok(mut pending) = bridge.late_bind_pending.lock() else {
        return LateBindPendingTake::None;
    };
    let key = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    if !pending.contains(&key) {
        return LateBindPendingTake::None;
    }
    let stamp = bridge
        .late_bind_pending_stamps
        .lock()
        .ok()
        .and_then(|stamps| stamps.get(&key).copied());
    if stamp.is_some_and(|stamp| started < stamp) {
        return LateBindPendingTake::Older;
    }
    pending.remove(&key);
    if let Ok(mut stamps) = bridge.late_bind_pending_stamps.lock() {
        stamps.remove(&key);
    }
    bridge
        .late_bind_pending_count
        .store(pending.len(), std::sync::atomic::Ordering::Release);
    LateBindPendingTake::Taken
}

/// A publication of `cached` whose compile started before its late-bind
/// request ([`LateBindPendingTake::Older`]): ask the recompile again. A
/// mutator door's ask may be granted at once; a background task's own is
/// refused (`already queued`: the task holds the slot), so the method is also
/// remembered in [`JitBridgeState::late_bind_owed`]; the task's outcome
/// carries to the worker loop (`CompileOutcome::deferred_new_retry`, the C2
/// retry asked after `finish`). Bounded: the next compile starts after the
/// stamp and takes the mark.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_note_older_publication(shared: &SharedVm, cached: &CachedBytecodeMethod) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind mark KEPT {}.{}{}: this publication's compile started \
             before the request; recompile asked again",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    let row = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    // j27 w6 T1TIERMGR (TIERMGR21-1): with `CRATONVM_JIT_ONE_OWED_RETRY` the
    // row is remembered only AFTER the answer, and only when the manager does
    // not own the debt; off, it is remembered first, as before.
    let one_owner = crate::jit::tiered::one_owed_retry_enabled();
    if !one_owner {
        late_bind_owed_insert(shared, row.clone());
    }
    let answer = shared.jit.tiered_manager.request_late_bind_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            cached.declaring_class_id,
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
    );
    late_bind_owe_vm_copy(shared, row, answer, true, !one_owner, "older publication");
}

/// Take `(class_id, method_name, descriptor)` out of
/// [`JitBridgeState::late_bind_owed`]: `true` when a publication of it found
/// its late-bind mark newer than its compile ([`late_bind_note_older_publication`]).
/// One lock, asked once per background method-entry task.
pub(in crate::runtime::interpreter::jit_bridge) fn take_late_bind_owed_retry(
    shared: &SharedVm,
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Ok(mut owed) = shared.jit.bridge.late_bind_owed.lock() else {
        return false;
    };
    if owed.is_empty() {
        return false;
    }
    owed.remove(&(class_id, Arc::from(method_name), Arc::from(descriptor)))
}



/// JIT round 21 lane indyconst: a ONE-SHOT optimizing-recompile request (its
/// latch already spent: the deferred-`new` memo, the inline-cache seed's
/// per-method claim) that the tier answered `AlreadyQueued` is owed to the
/// task holding the method's slot, exactly as EAGER19-3's
/// [`late_bind_note_older_publication`] owes its own: the method is remembered
/// in [`JitBridgeState::late_bind_owed`], and that background task's
/// publication carries `CompileOutcome::deferred_new_retry`, so the worker
/// asks the same C2 retry after `finish`. Without it the request was dropped
/// whenever the queued compile had read the method's state before the
/// condition the request answers (the class load, the seeded profile row) --
/// it then publishes the very body the request meant to replace, and the
/// latch never asks again. A queued compile that had not started yet costs
/// one redundant optimizing compile; bounded by [`LATE_BIND_CALLEE_CAP`] rows
/// and by each latch's own one-shot. With the tier manager's own owed retry
/// on (`CRATONVM_JIT_TIER_RETRY_OWED`, default ON since j27) the manager
/// already remembers the request, so this door stands down (t1 wave 8; the
/// test is `cratonvm_jit::tiered::manager_owes_answer` since j27 wave 6), and
/// under the defaults this copy is inert. `door` names the latch
/// on the `CRATONVM_DBG_JITC` line. Returns whether the method was remembered.
pub(crate) fn owe_one_shot_retry_if_already_queued(
    shared: &SharedVm,
    answer: crate::jit::tiered::C2RetryAnswer,
    class_id: u32,
    method_name: &str,
    descriptor: &str,
    door: &str,
) -> bool {
    // A row keyed by class id 0 (a re-sweep that could not resolve the class)
    // matches no background task (`take_late_bind_owed_retry` is asked with
    // the task's own id) and would only crowd the shared, capped set that
    // EAGER19-3's late-bind owes live in.
    if answer != crate::jit::tiered::C2RetryAnswer::AlreadyQueued
        || class_id == 0
        // j27 w6 T1TIERMGR (TIERMGR21-1): the manager's own decision of who
        // owns the debt (for these non-late-bind doors it is
        // `CRATONVM_JIT_TIER_RETRY_OWED`, as before).
        || crate::jit::tiered::manager_owes_answer(answer, door)
    {
        return false;
    }
    let owed = match shared.jit.bridge.late_bind_owed.lock() {
        Ok(mut owed) if owed.len() < LATE_BIND_CALLEE_CAP => {
            owed.insert((class_id, Arc::from(method_name), Arc::from(descriptor)));
            true
        }
        _ => false,
    };
    if owed && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] {door} retry OWED {method_name}{descriptor} (class id {class_id}): \
             already queued; asked again after that task publishes"
        );
    }
    owed
}

/// What a VM-side late-bind owe copy ([`JitBridgeState::late_bind_owed`])
/// does with the answer to its `late-bind` ask (j27 wave 6, lane T1TIERMGR,
/// TIERMGR21-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) enum LateBindVmOwe {
    /// Remember the row; nobody else owns the debt.
    Remember,
    /// Remember the row although the manager owns the debt too (switch off):
    /// the census line `late-bind owed DOUBLE` names it. Each such debt costs
    /// one redundant optimizing compile after the task.
    Double,
    /// The manager owns the debt; the copy stands down (switch on).
    StoodDown,
    /// No debt: a compile will run for this ask, or it was refused and the
    /// site does not keep refused asks.
    Nothing,
}

/// [`LateBindVmOwe`] for `answer`. `manager_owes`: the manager took the debt
/// (`cratonvm_jit::tiered::manager_owes_answer`); `one_owner`:
/// `CRATONVM_JIT_ONE_OWED_RETRY`; `keeps_refused`: the site remembered a
/// refused ask before (EAGER19-3's older publication does, the refused-put
/// restore does not). Pure.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_vm_owe(
    answer: crate::jit::tiered::C2RetryAnswer,
    manager_owes: bool,
    one_owner: bool,
    keeps_refused: bool,
) -> LateBindVmOwe {
    use crate::jit::tiered::C2RetryAnswer;
    match answer {
        C2RetryAnswer::Enqueued => LateBindVmOwe::Nothing,
        C2RetryAnswer::Refused if keeps_refused => LateBindVmOwe::Remember,
        C2RetryAnswer::Refused => LateBindVmOwe::Nothing,
        C2RetryAnswer::AlreadyQueued if !manager_owes => LateBindVmOwe::Remember,
        C2RetryAnswer::AlreadyQueued if one_owner => LateBindVmOwe::StoodDown,
        C2RetryAnswer::AlreadyQueued => LateBindVmOwe::Double,
    }
}

/// Insert `row` into [`JitBridgeState::late_bind_owed`] (bounded by
/// [`LATE_BIND_CALLEE_CAP`]).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_owed_insert(shared: &SharedVm, row: (u32, Arc<str>, Arc<str>)) {
    if let Ok(mut owed) = shared.jit.bridge.late_bind_owed.lock() {
        if owed.len() < LATE_BIND_CALLEE_CAP {
            owed.insert(row);
        }
    }
}

/// Apply [`late_bind_vm_owe`] to the answer of a late-bind ask for `row`
/// (`site` names the asking path on the `CRATONVM_DBG_JITC` line).
/// `already_remembered`: the site inserted the row before asking (EAGER19-3
/// with the switch off), so only the census line is left to do.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_owe_vm_copy(
    shared: &SharedVm,
    row: (u32, Arc<str>, Arc<str>),
    answer: crate::jit::tiered::C2RetryAnswer,
    keeps_refused: bool,
    already_remembered: bool,
    site: &str,
) {
    let decision = late_bind_vm_owe(
        answer,
        crate::jit::tiered::manager_owes_answer(answer, "late-bind"),
        crate::jit::tiered::one_owed_retry_enabled(),
        keeps_refused,
    );
    let word = match decision {
        LateBindVmOwe::Remember | LateBindVmOwe::Nothing => None,
        LateBindVmOwe::Double => Some("DOUBLE (the manager owes it too)"),
        LateBindVmOwe::StoodDown => Some("STOOD DOWN (the manager owes it)"),
    };
    if crate::runtime::env_cache::dbg_jitc() {
        if let Some(word) = word {
            eprintln!(
                "[cratonvm-jitc] late-bind owed {word} {}{} (class id {}): {site}",
                row.1, row.2, row.0
            );
        }
    }
    if !already_remembered && matches!(decision, LateBindVmOwe::Remember | LateBindVmOwe::Double) {
        late_bind_owed_insert(shared, row);
    }
}

/// Is `cached` a method [`late_bind_site_served`] asked a recompile of? Takes
/// the mark: only the first publication after the request retires what it
/// displaces. One relaxed load when nothing is pending.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_take_pending(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    late_bind_take_pending_started(shared, cached, u64::MAX) == LateBindPendingTake::Taken
}

/// `CRATONVM_JIT_ARM_NOT_ENTRANT_AT_PUBLISH` (JIT round 20, lane stash;
/// opt-in since the round-20 A/B: neutral on R16AllocPremium; `1` enables): see [`arm_not_entrant_at_publish`].
pub(in crate::runtime::interpreter::jit_bridge) fn arm_not_entrant_at_publish_enabled() -> bool {
    false
}

/// Arm this VM's JIT cache for not-entrant patches before a callee
/// publication (JIT round 20, lane stash; item 1 of
/// `r21-supersede-redirect-residuals-CLOSED-20261012.md`).
///
/// `JitCache::put` redirects the body a publication displaces
/// (`CRATONVM_JIT_SUPERSEDE_REDIRECT`) only once the cache is armed; until
/// then the displaced body waits, weakly, for the next publication of its key,
/// and its baked callers keep it when none comes. The VM armed the cache only
/// at its first redefinition, VarHandle mint, late bind, MH-constant retire,
/// trap eviction or hm5 C2 supersede. It cannot be armed in `SharedVm::new`:
/// the hook carries the VM's address, which is stable only once the VM is
/// shared; a publication is the first point every displacement passes. One
/// uncontended lock per publication (cold). Idempotent, as every other arming
/// site.
pub(in crate::runtime::interpreter::jit_bridge) fn arm_not_entrant_at_publish(shared: &SharedVm) {
    if !arm_not_entrant_at_publish_enabled() {
        return;
    }
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
}



/// A C2 publish replaced `superseded` (C1) with `replacement` under the same
/// key: make `superseded` not entrant, its stub forwarding to `replacement`
/// (`JitCache::make_not_entrant_after_eviction`, as
/// [`late_bind_retire_displaced`] does for a late-bind republish).
///
/// hm5 (`r16-alloc-hashmap-put-get-premium-is-the-call-path-not-allocation`;
/// W19-3 of `jit-r11-lhm-proposals`, C12-1 of `jit-r12-calls-proposals`). A
/// supersede evicts the callers that baked a direct call to the C1 body
/// (`supersede-baked-callers`), but only those: a compiled caller of THAT
/// caller keeps entering the evicted one, which keeps calling C1. In
/// `R16AllocPremium` the timed rounds run inside one `drive` activation whose
/// body baked `run`, whose body baked `hashmapCached`'s C1 body; the C2 body
/// published mid-warm-up was never reached (`hashmap-cached` 136 ns per pair,
/// the C1 body 17.7 % of the profile, against 42 ns for the C2 body). HotSpot
/// makes the old nmethod not entrant, which redirects every such caller at its
/// next call. Frames already running `superseded` finish in it.
///
/// Only for a pair whose stub can forward
/// (`cratonvm_jit::supersede_forward_admissible`): a stub that must re-dispatch
/// by name on every call would cost more than the C1 body. A body still
/// published (a refused or raced `put`) is skipped by the cache itself.
///
/// Returns whether `superseded` is not entrant afterwards (HANDLER9-2's
/// census reads it: a C1 body left entrant keeps every caller that baked it).
pub(in crate::runtime::interpreter::jit_bridge) fn supersede_retire_superseded(
    shared: &SharedVm,
    superseded: &cratonvm_jit::RetainedCode,
    replacement: &cratonvm_jit::RetainedCode,
    method_name: &str,
) -> bool {
    // JIT r25 wave 10 (W10-2b): name a refused forward, so a C1 body that
    // stays entrant (and keeps its baked callers) says why.
    let refusal = cratonvm_jit::supersede_forward_refusal(superseded.arc(), replacement.arc());
    if let Some(why) = refusal.filter(|_| crate::runtime::env_cache::dbg_jitc()) {
        // JIT r25 wave 11 (W11-2): an already patched body is not KEPT
        // entrant; its stub forwards from its first call.
        if why == cratonvm_jit::ALREADY_NOT_ENTRANT {
            eprintln!(
                "[cratonvm-jitc] c2-supersede {method_name}: superseded body already not entrant \
                 (redirected at publication; forwarded yet: {})",
                superseded.arc().not_entrant_stub_ever_forwarded()
            );
        } else {
            eprintln!(
                "[cratonvm-jitc] c2-supersede {method_name}: superseded body KEPT entrant ({why})"
            );
        }
    }
    if refusal.is_some()
        || supersede_forward_refused_for_cycle(shared, superseded.arc())
    {
        // JIT round 21 (lane lambdacap): name an entry-ABI mismatch, the
        // refusal that kept every baked caller of a lambda kernel on its
        // single-pass body (`ir_lambda_proxy_keeps_context_enabled`).
        if crate::runtime::env_cache::dbg_jitc()
            && superseded.arc().needs_context() != replacement.arc().needs_context()
        {
            eprintln!(
                "[cratonvm-jitc] c2-supersede {method_name}: forward REFUSED (entry ABI: \
                 superseded needs_context={} replacement needs_context={})",
                superseded.arc().needs_context(),
                replacement.arc().needs_context(),
            );
        }
        return superseded.is_not_entrant();
    }
    // Armed exactly as the redefinition, the VarHandle mint and the late bind
    // arm it; idempotent.
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
    let patched = shared
        .jit
        .jit_cache
        .make_not_entrant_after_eviction(std::slice::from_ref(superseded.arc()));
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] c2-supersede {method_name}: superseded body made not entrant (patched={patched})"
        );
    }
    superseded.is_not_entrant()
}

/// STASH20-1 (JIT round 20 wave 19, lane yStash;
/// `CRATONVM_JIT_SUPERSEDE_SKIP_REFUSED_FORWARD`, default ON, `=0` kills): is
/// `body`'s method a sticky recursion-cycle member, whose not-entrant stub
/// `jit_not_entrant_entry` would never forward (a statically bound call into it
/// must keep the dispatch helper's depth guard)? Then making it not entrant
/// only turns each call its baked callers make into a by-name re-dispatch, so
/// hm5's supersede ([`supersede_retire_superseded`]) and the late-bind retire
/// ([`late_bind_retire_displaced`]) leave it entrant, as `JitCache::put`'s
/// redirect does under the same switch. Cold (once per displacement).
pub(in crate::runtime::interpreter::jit_bridge) fn supersede_forward_refused_for_cycle(
    shared: &SharedVm,
    body: &cratonvm_jit::CompiledMethod,
) -> bool {
    if !cratonvm_jit::supersede_skip_refused_forward_enabled() {
        return false;
    }
    let Some((class_name, method_name, descriptor, class_id)) = body.published_key_parts() else {
        return false;
    };
    let refused = shared
        .jit
        .tiered_manager
        .verdicts()
        .direct_call_requires_dispatch_of(&class_name, &method_name, &descriptor, || {
            (class_id != 0).then_some(ClassId::new(class_id))
        });
    if refused && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] supersede-redirect SKIPPED (the VM would refuse its forward: \
             a direct call must dispatch this method): {}",
            body.method_label
        );
    }
    refused
}

/// `CRATONVM_JIT_LATE_BIND_RETIRE_ASKS_FORWARD` (JIT round 19 wave 2, lane
/// eager2, EAGER19-6 of `jit-r19-eager19-proposals.md`; **default OFF**, perf):
/// [`late_bind_retire_displaced`] asks `supersede_forward_admissible` before it
/// patches, exactly as hm5's [`supersede_retire_superseded`] does, and leaves a
/// pair whose stub could not forward alone. Without it such a pair's baked
/// callers re-dispatch BY NAME on every call (correct -- the not-entrant
/// stub's VM re-dispatch -- but slower than the displaced body, whose one
/// helper round trip is what the late bind repairs). Not a correctness fix:
/// both arms run the class's current bytecode. Read once per late-bind
/// republish (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_retire_asks_forward_enabled() -> bool {
    false
}

/// A late-bind recompile published `published` over `displaced`: make the
/// displaced body not entrant (`JitCache::make_not_entrant`, the redefinition's
/// and the VarHandle mint's tool), so a caller that BAKED a direct call to it --
/// the eager chain's caller, which is how the body was compiled early -- is
/// forwarded to `published` instead of running the dispatch-helper body until
/// it recompiles itself (a supersede alone never re-binds a baked caller:
/// `JitCache::note_supersede_baked_callers`). Frames already running the
/// displaced body finish in it. Nothing happens when
/// the publication kept the displaced body (a refused `put`, a raced publish).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_retire_displaced(
    shared: &SharedVm,
    published: &Arc<cratonvm_jit::CompiledMethod>,
    displaced: Arc<cratonvm_jit::CompiledMethod>,
    cached: &CachedBytecodeMethod,
) {
    if published.artifact_id == displaced.artifact_id {
        return;
    }
    // JIT round 19 wave 2, lane eager2 (EAGER19-6): ask hm5's question
    // first, under its switch ([`late_bind_retire_asks_forward_enabled`]).
    // Released through `RetainedCode` (it may be the last owner).
    if late_bind_retire_asks_forward_enabled()
        && !cratonvm_jit::supersede_forward_admissible(&displaced, published)
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] late-bind republished {}.{}{}: displaced body KEPT entrant \
                 (its stub could not forward)",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        drop(cratonvm_jit::RetainedCode::new(displaced));
        return;
    }
    // STASH20-1: a stub the VM would never forward is left unpatched.
    if supersede_forward_refused_for_cycle(shared, &displaced) {
        drop(cratonvm_jit::RetainedCode::new(displaced));
        return;
    }
    // Armed exactly as the redefinition and the VarHandle mint arm it;
    // idempotent.
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
    // Round 16 wave 15 (lane trapfix): the eviction flavour, as a
    // deoptimization's eviction uses it (`helpers::make_trap_evicted_body_not_entrant`):
    // the displaced body runs the class's current bytecode, so it is not
    // marked withdrawn by a redefinition. Released through `RetainedCode`: it
    // may be the body's last owner, and a bare drop would free published code
    // outside an authorised reclamation.
    let displaced = cratonvm_jit::RetainedCode::new(displaced);
    let patched = shared
        .jit
        .jit_cache
        .make_not_entrant_after_eviction(std::slice::from_ref(displaced.arc()));
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind republished {}.{}{}: displaced body made not entrant (patched={patched})",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
}

/// `CRATONVM_JIT_SUPERSEDE_FORWARD_EVERY_DOOR` (JIT round 19, lane eager19,
/// TP16-23 / C12-1; **default OFF**): a publication through
/// [`try_jit_compile_callee_slow`] or the method-entry upgrade door that
/// replaces a published body with DIFFERENT code makes the displaced body not
/// entrant, its stub forwarding to the new one ([`supersede_forward_displaced`]).
///
/// The C1 -> C2 supersede of a background C2 task already does this
/// (`supersede_retire_superseded`, hm5, default ON), and so does the late-bind
/// republish ([`late_bind_retire_displaced`]); every other replacement under
/// one key -- a single-pass republish (a deferred-`new` or first-call retry
/// that fell through), a mutator upgrade under `CRATONVM_BG_COMPILE=0`, a body
/// published by a door that is not a background C2 task's own -- left the
/// callers that BAKED a direct call to the old body on it until they
/// recompiled themselves. Idempotent with hm5 (a body already not entrant is
/// not patched twice). Read once per publication that displaced a body.
pub(in crate::runtime::interpreter::jit_bridge) fn supersede_forward_every_door_enabled() -> bool {
    false
}

/// A publication replaced `displaced` with `published` under one key: make
/// `displaced` not entrant, forwarding to `published`, when the code changed
/// ([`classify_supersede`]: an identical republish is left alone, the old
/// entry runs the same instructions) and the pair's stub can forward
/// (`supersede_forward_admissible`, asked by [`supersede_retire_superseded`]).
/// The eviction flavour, as hm5 and the late bind use it: the displaced body
/// runs the class's current bytecode.
pub(in crate::runtime::interpreter::jit_bridge) fn supersede_forward_displaced(
    shared: &SharedVm,
    displaced: Arc<cratonvm_jit::CompiledMethod>,
    published: &Arc<cratonvm_jit::CompiledMethod>,
    method_name: &str,
) {
    // Released through `RetainedCode` on every path: it may be the body's last
    // owner, and a bare drop would free published code outside an authorised
    // reclamation.
    let displaced = cratonvm_jit::RetainedCode::new(displaced);
    if displaced.arc().artifact_id == published.artifact_id
        || classify_supersede(
            Some(displaced.arc().code_bytes()),
            Some(published.code_bytes()),
        ) != SupersedeOutcome::Changed
    {
        return;
    }
    let published = cratonvm_jit::RetainedCode::new(Arc::clone(published));
    supersede_retire_superseded(shared, &displaced, &published, method_name);
}

/// `CRATONVM_JIT_LATE_BIND_SELF_LOCK` (JIT round 19, lane eager19, TP16-21 of
/// `jit-r16-throw4-proposals-RETIRED-20261006.md`; **default ON** since the round-19 proposal round, `=0` turns it off): a
/// `synchronized` callee the background door refused ONLY because its
/// self-locking body is not published yet (`self_locking_only` with no such
/// body: `bg-direct-call DECLINED ...: synchronized`) remembers the method
/// being compiled as a late-bind waiter, and the publication of that callee's
/// self-locking body asks the one late-bind recompile
/// ([`late_bind_self_lock_published`]), which then binds it. Woken at the
/// publication, not at a dispatch fill: such a site is usually an
/// `invokevirtual` served by the MIC helper, which fills no dispatch cache.
/// Needs `CRATONVM_JIT_LATE_BIND_RECOMPILE` (default ON). Read on the refusal
/// and on a self-locking publication only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_self_lock_enabled() -> bool {
    true
}

/// The late-bind waiter key of the self-locking body of `class.method desc`
/// (`class` the DECLARING class): its own namespace, so a dispatch fill of the
/// same callee ([`late_bind_site_served`]) does not take these waiters.
pub(in crate::runtime::interpreter::jit_bridge) fn self_lock_waiter_key(class: &str, method: &str, descriptor: &str) -> String {
    format!(
        "#self-lock:{}",
        late_bind_callee_key(class, method, descriptor)
    )
}

/// See [`late_bind_self_lock_enabled`]: remember `waiting` on the self-locking
/// body of `declaring_class.callee_method callee_desc`. Same caps as every
/// late-bind waiter; idempotent.
pub(in crate::runtime::interpreter::jit_bridge) fn note_self_lock_late_bind_waiter(
    shared: &SharedVm,
    declaring_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting: &CachedBytecodeMethod,
) {
    if !late_bind_self_lock_enabled() || !late_bind_recompile_enabled(shared) {
        return;
    }
    let key = self_lock_waiter_key(declaring_class, callee_method, callee_desc);
    if note_late_bind_waiter_key(shared, key, waiting) && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING {}.{}{} on the self-locking body of \
             {declaring_class}.{callee_method}{callee_desc}",
            waiting.class_name, waiting.method_name, waiting.method_descriptor
        );
    }
}

/// `body` was just published for `class.method desc`: when it is a
/// self-locking body, ask the late-bind recompile of every method waiting on
/// it ([`note_self_lock_late_bind_waiter`]). One relaxed load when nothing
/// waits anywhere.
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_self_lock_published(
    shared: &SharedVm,
    class: &str,
    method: &str,
    descriptor: &str,
    body: &cratonvm_jit::CompiledMethod,
) {
    if !is_self_locking_body(body)
        || shared
            .jit
            .bridge
            .late_bind_waiter_count
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        || !late_bind_self_lock_enabled()
    {
        return;
    }
    let key = self_lock_waiter_key(class, method, descriptor);
    late_bind_key_served(shared, &key, "published its self-locking body");
}

/// `CRATONVM_JIT_LATE_BIND_UPGRADE_DOOR` (JIT round 19, lane eager19, TP16-22
/// of `jit-r16-throw4-proposals-RETIRED-20261006.md`; **default OFF**): the
/// optimizing door's own direct-bind resolver (`compile_optimizing_artifact`'s
/// `callee_compiler_bound`: the method-entry upgrade, which is the mutator's
/// door under `CRATONVM_BG_COMPILE=0` and the inline eager first-call door)
/// remembers a method-entry compile as a late-bind waiter on a "not yet"
/// refusal (callee class not found, static callee's class not initialized),
/// as the background door has since TP16-20. Not the OSR door: an OSR body is
/// not replaced by the late-bind recompile, which compiles the method entry
/// (book `jit-r19-eager19-proposals.md`, EAGER19-4). The wake is the existing
/// dispatch fill ([`late_bind_site_served`]), which asks only under the
/// background compiler. Read on the refusal only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_upgrade_door_enabled() -> bool {
    false
}

/// The optimizing door refused to bind `callee` in the body of `waiting`, a
/// METHOD-ENTRY compile (`osr_door == false`), for a reason that may lift:
/// remember `waiting` as the background door does ([`note_late_bind_waiter`];
/// [`late_bind_upgrade_door_enabled`]). Called with no class-manager guard held.
///
/// JIT round 19 wave 8 (lane osr8, INTERP719-2): for an OSR-door compile
/// (`osr_door`, entered at `osr_entry_pc`), the optimizing OSR waiter instead
/// ([`note_osr_optimizing_late_bind_waiter`]), under its own switch.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_upgrade_door_late_bind_waiter(
    shared: &SharedVm,
    osr_entry_pc: Option<usize>,
    osr_door: bool,
    refusal: cratonvm_jit::DirectBindRefusal,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    waiting: &CachedBytecodeMethod,
) {
    if osr_door {
        if let Some(entry_pc) = osr_entry_pc {
            note_osr_optimizing_late_bind_waiter(
                shared,
                refusal,
                callee_class,
                callee_method,
                callee_desc,
                waiting,
                entry_pc,
            );
        }
        return;
    }
    if !late_bind_upgrade_door_enabled() {
        return;
    }
    note_late_bind_waiter(
        shared,
        refusal,
        callee_class,
        callee_method,
        callee_desc,
        waiting,
    );
}



/// Round 15 wave 5 (lane sync4, SY3-1): an interpreter tier-up offer
/// ([`offer_invocation_to_tiered_manager`]) just queued a compile of the
/// `static synchronized` method `cached`. Mint its class's mirror slot now,
/// on this MUTATOR (`helpers::class_mirror_slot_for`: from the mirror the
/// class already has, no allocation, nothing collects in between), so that the
/// background compile finds it and the method's FIRST body is self-locking --
/// instead of a wrapped body that the JIT door's later mint repairs through
/// [`request_static_self_lock_recompile`]. A compile that already started
/// before this mint still gets that repair.
///
/// Only for a method whose compile could self-lock with the slot
/// (`cratonvm_jit::static_self_lock_recompile_would_self_lock`, asked with a
/// stand-in slot: it reads only the slot's presence), so no mirror is rooted
/// for a method that could never use it. The rooting rule moves one step
/// earlier than `JitSynchronizedMonitorGuard::acquire`'s ("a compiled body of
/// the method exists" becomes "a compile of it is queued"); a minted slot keeps
/// the mirror and its loader reachable for the VM's life
/// (`r15w4-sync3-review-findings-FIXED-20260930.md` item 5).
pub(in crate::runtime::interpreter::jit_bridge) fn mint_static_self_lock_slot_at_offer(shared: &SharedVm, cached: &CachedBytecodeMethod) {
    if !(cached.is_static && cached.is_synchronized)
        || !crate::runtime::env_cache::jit_self_locking_sync_static()
    {
        return;
    }
    // Any non-zero word stands for "the slot the mint would create".
    const SLOT_PRESENT: usize = 1;
    if !cratonvm_jit::static_self_lock_recompile_would_self_lock(
        jit_verdicts(shared),
        cached,
        SLOT_PRESENT,
    ) {
        return;
    }
    let slot = crate::jit::helpers::class_mirror_slot_for(shared, cached.declaring_class_id);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] static self-lock mirror slot at offer {}.{}{}: {}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            if slot != 0 { "minted" } else { "unavailable" }
        );
    }
}



/// Most requests the CONF21-1 queue holds between two drains; past it a
/// method's String literals resolve at first execution, as before.
pub(in crate::runtime::interpreter::jit_bridge) const ADMITTED_COMPILE_STRINGS_CAP: usize = 1024;

/// CONF21-1: the `(key, optimizing)` rows the tiered manager's admission sink
/// pushed (`TieredCompilationManager::set_compile_admission_sink`) and a
/// mutator has not yet drained. The sink runs under the manager's `methods`
/// lock, on whatever thread queued the compile, so it only records; this lock
/// is a leaf.
#[derive(Default)]
pub(crate) struct AdmittedCompileStrings {
    pub(in crate::runtime::interpreter::jit_bridge) pending: std::sync::Mutex<Vec<(crate::jit::tiered::MethodKey, bool)>>,
    pub(in crate::runtime::interpreter::jit_bridge) nonempty: std::sync::atomic::AtomicBool,
}

impl AdmittedCompileStrings {
    pub(in crate::runtime::interpreter::jit_bridge) fn push(&self, key: &crate::jit::tiered::MethodKey, optimizing: bool) {
        if let Ok(mut pending) = self.pending.lock() {
            if pending.len() < ADMITTED_COMPILE_STRINGS_CAP {
                pending.push((key.clone(), optimizing));
                self.nonempty.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn take(&self) -> Vec<(crate::jit::tiered::MethodKey, bool)> {
        match self.pending.lock() {
            Ok(mut pending) => {
                self.nonempty.store(false, std::sync::atomic::Ordering::Release);
                std::mem::take(&mut *pending)
            }
            Err(_) => Vec::new(),
        }
    }
}

/// CONF21-1: install the tiered manager's compile admission sink, which
/// queues each admitted request for [`resolve_admitted_compile_strings`].
/// Nothing under `CRATONVM_JIT_COMPILE_RESOLVES_STRINGS=0`.
pub(crate) fn install_compile_admission_sink(shared: &SharedVm) {
    let queue = Arc::clone(&shared.jit.bridge.admitted_compile_strings);
    let _ = shared
        .jit
        .tiered_manager
        .set_compile_admission_sink(Box::new(move |key, optimizing| queue.push(key, optimizing)));
}

/// CONF21-1 (JIT round 21 lane conf, proposal CONF21-1; page
/// `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`): HotSpot resolves
/// String constants when a compile is requested, on the requesting thread --
/// before a C2 compile the holder's whole constant pool
/// (`CompileBroker::compile_method`), and a C1 compile the method's own
/// literals while parsing -- so past the first compile of a method (an OSR
/// compile of a hot loop included) its literals are pooled even where no
/// `ldc` has run yet; its interpreter keeps them lazy. This VM compiles on the
/// GC-neutral worker, which must not allocate, so the manager only records
/// each admitted request ([`install_compile_admission_sink`]) and a MUTATOR
/// resolves here (`constants::resolve_string_ldcs_eagerly`): the holder's
/// pool for an optimizing or OSR request (the worker builds the optimizing
/// OSR body first), the method's `ldc` literals for a single-pass one.
///
/// Called right after the mutator's tiering doors ask the manager (the
/// method-entry offer, the OSR offers, the compiled-entry counter drain), so a
/// request a mutator queued is resolved before that mutator runs on; one a
/// worker queued (the C1 -> C2 supersede) at the next door any mutator
/// passes. No-op on the worker and with nothing queued (one relaxed load).
/// Call with no class-manager lock held.
pub(crate) fn resolve_admitted_compile_strings(shared: &SharedVm) {
    let queue = &shared.jit.bridge.admitted_compile_strings;
    if !queue.nonempty.load(std::sync::atomic::Ordering::Relaxed)
        || std::thread::current().name() == Some("cratonvm-jit-compiler")
    {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for (key, optimizing) in queue.take() {
        if !seen.insert((key.clone(), optimizing)) {
            continue;
        }
        let Some((class_id, code, _)) = fetch_osr_compile_inputs(
            shared,
            key.class_id,
            &key.class_name,
            &key.method_name,
            &key.descriptor,
        ) else {
            continue;
        };
        let scope = (!optimizing).then_some(&code[..]);
        let resolved = super::constants::resolve_string_ldcs_eagerly(shared, class_id, scope);
        if resolved != 0 && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] compile strings {}.{}{} ({}): {resolved} literal(s) resolved",
                key.class_name,
                key.method_name,
                key.descriptor,
                if optimizing { "pool" } else { "method" },
            );
        }
    }
}


/// `CRATONVM_JIT_LDC_SLOT_MINT_BEFORE_ENQUEUE` (nio21 wave 2, lane L2,
/// proposal L2-5; **default ON**, `0` mints after the tiered manager's verdict
/// as in nio20 wave 6): the method-entry door mints before it asks the
/// manager, which is what enqueues the compile. Moot while
/// `CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0` (the mint itself is then off).
pub(in crate::runtime::interpreter::jit_bridge) fn is_security_walk_ignored_for_jit(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    is_hidden: bool,
) -> bool {
    if class_name == "java/lang/reflect/Method" {
        return method_name == "invoke";
    }
    if class_name.starts_with("jdk/internal/reflect/") {
        if shared
            .classes
            .class_manager
            .read()
            .is_subclass_of_by_name(class_id, "jdk/internal/reflect/MethodAccessorImpl")
        {
            return true;
        }
    }
    let Some(simple) = class_name.strip_prefix("java/lang/invoke/") else {
        return false;
    };
    is_hidden
        || simple.starts_with("LambdaForm$")
        || simple.ends_with("$Holder")
        || (simple == "MethodHandle"
            && matches!(
                method_name,
                "invokeWithArguments"
                    | "invokeBasic"
                    | "linkToStatic"
                    | "linkToVirtual"
                    | "linkToSpecial"
                    | "linkToInterface"
                    | "linkToNative"
            ))
}

pub(in crate::runtime::interpreter::jit_bridge) fn ldc_slot_mint_before_enqueue_enabled() -> bool {
    true
}

/// The most `ldc <Class>` / `ldc <String>` constant-pool entries one offer
/// mints slots for (each is two short lock holds); a method past it keeps the
/// helper CALL on the rest, as before.
pub(in crate::runtime::interpreter::jit_bridge) const LDC_MINT_AT_OFFER_MAX_SITES: usize = 64;
