#[allow(unused_imports)]
use super::super::*;

/// `CRATONVM_JIT_IR_FFM_ELEMENT_CALL` (JIT round 23 wave 2d, lane L8;
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// perf, **default OFF** since r23 wave 3 (measured on n23-w2: segoff 8.4 -> 114 us/op,
/// segget 44 -> 133 us/op with it on -- the call loses the single-pass intrinsic's
/// helper road to the MIC dispatch; `=1` opts in): a `java/lang/foreign/MemorySegment`
/// element accessor site (`getAtIndex` / `setAtIndex`, and the offset `get` /
/// `set` under `CRATONVM_JIT_FFM_OFFSET_ACCESS`; the single-pass
/// `JitIntrinsic::FfmSegment*` rows) no longer refuses its whole method the
/// optimizing tier. There it stays an ordinary interface call: the JDK's
/// `AbstractMemorySegmentImpl` accessor, whose `VarHandle.get/set` the funnel
/// serves on the lean segment road (`try_varhandle_segment_access`; native and,
/// since r23 w2c, heap segments). Arrays.equals' shape
/// ([`ir_arrays_equals_call_admits`]). Compile time only.
pub(crate) fn ir_ffm_element_call_admits(class: &str, method: &str) -> bool {
    class == "java/lang/foreign/MemorySegment"
        && ffm_element_accessor_name(method)
        && false
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT` (JIT round 23 wave 3, lane L8; perf,
/// **default ON**, `=0` kills;
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// the thin helper an optimizing-tier `MemorySegment` element accessor site
/// binds to, or `None`. The site is admitted to the tier (instead of refusing
/// its whole method, the single-pass intrinsic's rule) and, after the build,
/// bound as a JUDGED direct call (`VarHandleJudgedSite`): the helper serves
/// the access on the FFM element door's cached road (native and heap
/// segments) and otherwise dispatches the site with its own row -- never the
/// MIC funnel `CRATONVM_JIT_IR_FFM_ELEMENT_CALL` paid. Only the shapes the
/// single-pass intrinsic takes: `getAtIndex` / `get(layout, long)` returning
/// the carrier, `setAtIndex` / `set(layout, long, carrier)`, a carrier
/// [`ffm_kind_for_descriptor`] names. Compile time only.
pub(crate) fn ir_ffm_element_direct_entry(
    class: &str,
    method: &str,
    desc: &str,
    table: &DirectHelperTable,
) -> Option<usize> {
    if class != "java/lang/foreign/MemorySegment"
        || !ffm_element_accessor_name(method)
        || table.static_base_resolver_ctx == 0
        // r23 w3 review: the bind is an `ir_direct_calls` row, so the IR
        // direct-call capability gates it as it gates `ir_arraycopy_entry`
        // and the judged `VarHandle` binds (off: the method is refused as
        // before, by the planner rule this entry exempts).
        || !ir_direct_calls_enabled()
        // r23 w4 A/B on n23-w3: segget 35.9 us/op ON vs 1.55 us/op =0 (and segput/segoff
        // alike), so the bind is opt-in until it beats the single-pass route.
        || !false
    {
        return None;
    }
    ffm_kind_for_descriptor(desc)?;
    // `(LValueLayout$OfX;J)X` / `(LValueLayout$OfX;JX)V`: one layout, one
    // `long`, and the carrier last.
    let (params, ret) = desc.strip_prefix('(')?.split_once(')')?;
    let after_layout = params.split_once(';')?.1;
    let entry = if ffm_element_accessor_is_get(method) {
        if after_layout != "J" || ret.len() != 1 || ret == "V" {
            return None;
        }
        table.ffm_element_get
    } else {
        if after_layout.len() != 2 || !after_layout.starts_with('J') || ret != "V" {
            return None;
        }
        table.ffm_element_set
    };
    (entry != 0).then_some(entry)
}

/// JIT r23 wave 4 (lane L8, L8-8 stage 5; `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`,
/// opt-in): the inline-expansion plan of one `MemorySegment` element site, or
/// `None`. Planned when all hold:
///
/// * the switch is on, the site is an `invokeinterface` / `invokevirtual` the
///   compile door resolved (not a first-call site), and it is an integral
///   accessor ([`ffm_element_inline::ffm_element_accessor`]);
/// * its receiver profile names ONE dominant class
///   (`ir::profile_guarded_receiver_class`) that the VM describes as a JDK
///   segment class ([`ffm_element_inline::ffm_segment_shape`]); a bimorphic
///   site (heap and native through one call) is not served in this cut;
/// * the method reads at least one filed layout of the site's carrier
///   (`layout_facts`): the builder checks the site's actual operand.
///
/// `bail_on_decline` is set when no direct bind serves the site, so a builder
/// decline refuses the method (the single-pass route) instead of leaving a
/// dispatch. Compile time only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_ffm_element_inline_plan(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    table: &DirectHelperTable,
    layout_facts: &std::collections::HashMap<(u32, u32), ffm_element_inline::FfmLayoutFacts>,
    after_unreached_trap: bool,
) -> Option<ffm_element_inline::FfmElementInlineSite> {
    if first_call || !matches!(opcode, 0xb6 | 0xb9) || !ir::ir_ffm_element_inline_enabled() {
        return None;
    }
    let accessor = ffm_element_inline::ffm_element_accessor(class, method, desc)?;
    if !layout_facts.values().any(|f| f.carrier == accessor.carrier) {
        return None;
    }
    // JIT r23 wave 11 (lane L4): after this site's unreached trap fired
    // (`after_unreached_trap`), the receivers the interpreter recorded since
    // are the only evidence there will be -- the single-pass route records
    // none -- so a profile that names ONE class for every observation it has
    // (at least one) plans the expansion. Its exact-class guards deopt on any
    // other class, at the call's own bci.
    let receiver = ir::profile_guarded_receiver_class(profile, pc).or_else(|| {
        if !after_unreached_trap {
            return None;
        }
        let counts = profile?.receivers.get(&pc)?;
        let evidence = profile::ReceiverEvidence::of(counts);
        let summary = evidence.summary();
        let (class_id, count) = summary.top?;
        (summary.observations >= 1 && count == summary.observations && class_id != 0)
            .then_some(class_id)
    })?;
    let shape = ffm_element_inline::ffm_segment_shape(table, receiver)?;
    Some(ffm_element_inline::FfmElementInlineSite {
        shape,
        accessor,
        bail_on_decline: ir_ffm_element_direct_entry(class, method, desc, table).is_none(),
    })
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC` (JIT round 24; perf,
/// **default ON since j27 2026-10-28**, `=0` kills): an element site whose receiver profile splits between two
/// JDK segment classes the VM describes (`ir::profile_bimorphic_receiver_classes`:
/// neither class at the one-class share, the pair at 90 % or more) is planned
/// too, and the builder expands it as a class test with one expansion per
/// class (`IrBuilder::try_emit_ffm_element_bimorphic`). Without it such a site
/// has no plan and its method stays off the optimizing tier: one kernel
/// called with both a heap and a native segment (`R26SegVecPut bi-put-add`,
/// about 5400 ns per pair of 64-element puts against HotSpot's 60). Read per
/// site at compile time; no static.
pub(crate) fn ir_ffm_element_bimorphic_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// JIT round 24 ([`ir_ffm_element_bimorphic_enabled`]): the plan of a
/// bimorphic element site, `(the site with the first class's shape, the
/// second class's shape)`, or `None`. Both classes must be segment classes
/// the VM describes, distinct, and the site an `invokevirtual` /
/// `invokeinterface` element accessor with a filed layout of its carrier, as
/// for the one-class plan ([`ir_ffm_element_inline_plan`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_ffm_element_bimorphic_plan(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    table: &DirectHelperTable,
    layout_facts: &std::collections::HashMap<(u32, u32), ffm_element_inline::FfmLayoutFacts>,
) -> Option<(ffm_element_inline::FfmElementInlineSite, ffm_element_inline::FfmSegmentShape)> {
    if first_call
        || !matches!(opcode, 0xb6 | 0xb9)
        || !ir::ir_ffm_element_inline_enabled()
        || !ir_ffm_element_bimorphic_enabled()
    {
        return None;
    }
    ir_ffm_element_bimorphic_site_of(
        class,
        method,
        desc,
        ir::profile_bimorphic_receiver_classes(profile, pc)?,
        table,
        layout_facts,
    )
}

/// t1 wave 8 (lane FFM27, FFM27-2; `CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP`,
/// default ON since j27 2026-10-28, under `CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC`): the bimorphic plan
/// of an element site whose unreached trap fired, asked BEFORE the one-class
/// rules. Planned when the speculations withdrawn at the bci are that trap and
/// at most the one-class expansion's class test (`despec_ids`,
/// [`ffm_element_inline::ffm_after_trap_despec`]), and the row names exactly
/// two classes ([`ffm_element_inline::ffm_bimorphic_after_trap_pair`]) or,
/// with the trap alone withdrawn, passes the ordinary pair rule
/// (`ir::profile_bimorphic_receiver_classes`). Before, the
/// planner skipped the bimorphic expansion at any de-spec'd bci, so a heap /
/// native kernel first compiled before its loop ran never got it, and its
/// seeded or deopt-skewed row planned a one-class expansion that deopted on
/// every call of the other class (`T1J6FfmSeedRows bi-mixed-put`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_ffm_element_bimorphic_plan_after_trap(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    table: &DirectHelperTable,
    layout_facts: &std::collections::HashMap<(u32, u32), ffm_element_inline::FfmLayoutFacts>,
    despec_ids: &[u32],
) -> Option<(ffm_element_inline::FfmElementInlineSite, ffm_element_inline::FfmSegmentShape)> {
    if first_call
        || !matches!(opcode, 0xb6 | 0xb9)
        || !ir::ir_ffm_element_inline_enabled()
        || !ir_ffm_element_bimorphic_enabled()
        || !ir_ffm_unreached_site_trap_enabled()
        || !ffm_element_inline::ffm_bimorphic_after_trap_enabled()
    {
        return None;
    }
    let exactly_two = ffm_element_inline::ffm_bimorphic_after_trap_pair(profile, pc);
    let pair = match ffm_element_inline::ffm_after_trap_despec(despec_ids, pc)? {
        ffm_element_inline::FfmAfterTrapDespec::TrapOnly => {
            exactly_two.or_else(|| ir::profile_bimorphic_receiver_classes(profile, pc))?
        }
        // The one-class expansion's class test kept failing: two classes and
        // nothing else, so a third class ends the expansion.
        ffm_element_inline::FfmAfterTrapDespec::TrapAndClassMiss => exactly_two?,
    };
    ir_ffm_element_bimorphic_site_of(class, method, desc, pair, table, layout_facts)
}

/// The bimorphic plan of an element accessor site for the receiver pair
/// `(first, second)`: both described segment classes, distinct, the site an
/// element accessor with a filed layout of its carrier. The shared tail of
/// [`ir_ffm_element_bimorphic_plan`] and
/// [`ir_ffm_element_bimorphic_plan_after_trap`].
pub(crate) fn ir_ffm_element_bimorphic_site_of(
    class: &str,
    method: &str,
    desc: &str,
    (first, second): (u32, u32),
    table: &DirectHelperTable,
    layout_facts: &std::collections::HashMap<(u32, u32), ffm_element_inline::FfmLayoutFacts>,
) -> Option<(ffm_element_inline::FfmElementInlineSite, ffm_element_inline::FfmSegmentShape)> {
    let accessor = ffm_element_inline::ffm_element_accessor(class, method, desc)?;
    if !layout_facts.values().any(|f| f.carrier == accessor.carrier) {
        return None;
    }
    let shape1 = ffm_element_inline::ffm_segment_shape(table, first)?;
    let shape2 = ffm_element_inline::ffm_segment_shape(table, second)?;
    if shape1.class_id == shape2.class_id {
        return None;
    }
    Some((
        ffm_element_inline::FfmElementInlineSite {
            shape: shape1,
            accessor,
            bail_on_decline: ir_ffm_element_direct_entry(class, method, desc, table).is_none(),
        },
        shape2,
    ))
}

/// `CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP` (JIT round 23 wave 11, lane L4;
/// perf, **default ON** since R23-W13-FLIP, `=0` kill: with the wave-13 seed
/// call it took `HgZero zero` n=64 from ~2,900 to ~500 ns/call on n23-w13a,
/// `normal` unchanged). Read per site at compile time; no static. See
/// [`ir_ffm_element_unreached_site`].
pub(crate) fn ir_ffm_unreached_site_trap_enabled() -> bool {
    true
}

/// JIT round 23 wave 11 (lane L4; page
/// `docs/feature-designs/jit-ir-optimizations.md`): is this
/// `MemorySegment` element site one NO execution has reached -- the method
/// has a profile, but the site's receiver row is absent or empty -- so the
/// optimizing build plants an unreached-site trap there
/// (`IrBuilder::try_emit_ffm_unreached_trap`) instead of refusing the whole
/// method for want of a receiver class (the inline plan,
/// [`ir_ffm_element_inline_plan`], needs one)?
///
/// HotSpot's shape: C2 compiles a never-reached call as an uncommon trap; the
/// first arrival deopts, the interpreter profiles the site, and the
/// recompile has the profile. Here the trap is an `UnstableIf` guard
/// (withdrawn after one charge, `deopt_resume::per_bci_despec_limit`), so the
/// first arrival costs one deopt and one recompile, and the recompile plans
/// the inline expansion. Without it the method stayed on the single-pass body
/// forever (the single-pass FFM route records no receiver, so the profile
/// never arrives): `HgZero` about 45 ns per access against 1.9.
///
/// A method with no profile at all counts as unreached: it was compiled before
/// it ever ran in the interpreter (an eager callee compile; n23-w11a showed
/// `HgZero.heapGet` compiled that way, which is why the first cut, which
/// refused a missing profile, never engaged).
///
/// `false` with the switch off, for a first-call site, when the site has ANY receiver
/// observation (a thin or polymorphic profile keeps the old route), and when
/// the registry already holds a speculation at this bci (a trap that fired
/// once: the next compile takes the old route rather than trapping again).
/// Compile time only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_ffm_element_unreached_site(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    despec_at_pc: bool,
) -> bool {
    if first_call
        || despec_at_pc
        || !matches!(opcode, 0xb6 | 0xb9)
        || !ir::ir_ffm_element_inline_enabled()
        || !ir_ffm_unreached_site_trap_enabled()
        || ffm_element_inline::ffm_element_accessor(class, method, desc).is_none()
    {
        return false;
    }
    // No profile at all: the method never ran in the interpreter (an eager
    // callee compile, n23-w11a `HgZero zero`), so neither did the site.
    let Some(profile) = profile else {
        return true;
    };
    match profile.receivers.get(&pc) {
        None => true,
        Some(counts) => profile::ReceiverEvidence::of(counts).summary().observations == 0,
    }
}

/// `CRATONVM_JIT_IR_DIRECT_STATIC_SPLICED_FEED` (JIT round 23 wave 13, lane
/// L7; perf, **default ON** since the w2c close, 2026-10-24, `=0` kill;
/// opt-in until then): `Graph::direct_static_fields` --
/// the list every "is this `getstatic` the direct, trap-free read" question
/// asks (`ir_optimize::is_direct_static_read`) -- also names the static reads
/// of SPLICED callee bodies, not only the compiling method's own sites. Read
/// per compile; no `static`.
///
/// Why it matters (n23-w12a, RIP-sampled): `W2cTypecheckLoops`' timed rows run
/// the row spliced into `time()`'s OSR body (`r.run()` -> the lambda ->
/// `castRow` / `nocastRow`), and 99% of the samples are in that body's inner
/// loop, which re-reads `a`, `a.length`, `h.k`, `h.tab` and `h.tab[5]` on
/// every element (about 95 instructions per element). The loop's pre-header
/// holds the lambda's two `getstatic`s (`subs`, `holder`); they are spliced,
/// so they were not on the list, so `preheader_traps_block_entry_predicate`
/// counted them as trapping (`[ir-licm] null-base predicate: header 14
/// pre-header may trap`) and every loop-entry predicate and hoist of that
/// loop stood down. The lowering already reads them directly (`mov rax,
/// mirror; mov rax,[rax]; mov rax,[rax+off]`): its rule is the same
/// `resolve_static_base` answer this feed asks (an initialised class, never
/// `java/lang/System`), so the list now says what the code does.
pub(crate) fn ir_direct_static_spliced_feed_enabled() -> bool {
    true
}

/// The non-volatile `getstatic` rows of `graph` (its own and every spliced
/// body's `Op::LoadStatic`) that `resolves` admits as direct and `have` does
/// not list yet, each once, in node order. `resolves(class_id, field_index)`
/// is the compile's `DirectHelperTable::resolve_static_base(..).is_some()`.
pub(crate) fn ir_direct_static_spliced_rows(
    graph: &ir::Graph,
    have: &[(u32, u32)],
    resolves: impl Fn(u32, usize) -> bool,
) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for n in &graph.nodes {
        let ir::Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } = n.op
        else {
            continue;
        };
        let key = (class_id, field_index);
        if have.contains(&key) || out.contains(&key) {
            continue;
        }
        let Ok(index) = usize::try_from(field_index) else {
            continue;
        };
        if resolves(class_id, index) {
            out.push(key);
        }
    }
    out
}

/// Feed the VM-filed lengths of initialized trusted static arrays, including
/// reads introduced by inlining. Never read the heap on a compiler thread.
pub(crate) fn ir_feed_trusted_static_arrays(
    graph: &mut ir::Graph,
    resolves: impl Fn(u32, usize) -> bool,
    length_of: &dyn Fn(u32, u32) -> Option<i32>,
) {
    let mut rows: Vec<((u32, u32), i32)> = Vec::new();
    for n in &graph.nodes {
        let ir::Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } = n.op else { continue };
        if rows.iter().any(|&(key, _)| key == (class_id, field_index)) {
            continue;
        }
        let Ok(index) = usize::try_from(field_index) else { continue };
        if !resolves(class_id, index) {
            continue;
        }
        if let Some(length) = length_of(class_id, field_index) {
            rows.push(((class_id, field_index), length));
        }
    }
    // A VM-filed length names a non-null array held by an initialized,
    // trusted final field. Preserve that reference fact for every graph read,
    // including one introduced by inlining, without treating elements as final.
    for &(key, _) in &rows {
        if !graph.trusted_final_static_fields.contains(&key) {
            graph.trusted_final_static_fields.push(key);
        }
    }
    graph.trusted_static_array_lengths = rows;
}


/// `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP` (JIT round 23 wave 12, lane L7;
/// perf, **opt-in** `=1` until A/B'd). Read per site at compile time; no
/// static. See [`ir_unreached_call_site_shape`].
pub(crate) fn ir_unreached_call_trap_enabled() -> bool {
    false
}

/// JIT round 23 wave 12 (lane L7; page
/// `docs/feature-designs/jit-ir-optimizations.md`): the
/// generic form of [`ir_ffm_element_unreached_site`]. A call-site intrinsic
/// the optimizing planner has no node for refuses the WHOLE method ("loop-
/// shaped or memory call-site intrinsic"), and a C2 task refused that way
/// settles on its single-pass body for good. When no execution has reached
/// the site, that refusal is a verdict on a profile that does not exist yet:
/// the site becomes an `UnstableIf` trap instead
/// (`IrBuilder::try_emit_ffm_unreached_trap`), whose first arrival deopts,
/// lets the interpreter run and profile it, and recompiles. The VM withdraws
/// the pc after that one charge (`despec_at_pc` is then `true`), so the next
/// compile takes the old refusal: one deopt and one recompile per site, at
/// most.
///
/// `Some((operand entries, return tag))` -- one entry per argument plus the
/// receiver for `invokevirtual` / `invokeinterface`, as the builder's stack
/// holds them -- or `None`: the switch off, a first-call site, a pc withdrawn
/// once already, an opcode other than 0xb6 / 0xb8 / 0xb9, a descriptor
/// [`static_call_shape`] declines, or a profile that saw the site run (any
/// call count at `pc`, or any receiver observation there). No profile at all
/// is "never ran", as for the element sites: an eagerly compiled callee.
pub(crate) fn ir_unreached_call_site_shape(
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    despec_at_pc: bool,
) -> Option<(usize, u8)> {
    if first_call
        || despec_at_pc
        || !matches!(opcode, 0xb6 | 0xb8 | 0xb9)
        || !ir_unreached_call_trap_enabled()
    {
        return None;
    }
    if let Some(p) = profile {
        if p.call_sites.get(&pc).copied().unwrap_or(0) != 0 {
            return None;
        }
        if p
            .receivers
            .get(&pc)
            .is_some_and(|c| profile::ReceiverEvidence::of(c).summary().observations != 0)
        {
            return None;
        }
    }
    let (args, ret) = static_call_shape(desc)?;
    Some((args + usize::from(opcode != 0xb8), ret))
}



/// `CRATONVM_JIT_IR_FFM_UNREACHED_SEED_CALL` (JIT round 23 wave 13, lane L4;
/// perf, **default ON** (R23-W13-FLIP), `=0` kill, and only with
/// `CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP` on; n23-w13a `HgZero zero` n=4096
/// 7.3-8.4 us/call against 174-194 us default). Read per site at compile time;
/// no static. See [`ir_ffm_element_seed_call_site`].
pub(crate) fn ir_ffm_unreached_seed_call_enabled() -> bool {
    true
}

/// JIT round 23 wave 13 (lane L4): should this `MemorySegment` element site be
/// kept an inline-cache CALL, with the cache's profile seed armed
/// ([`IcProfileSeedSite`]), rather than refusing the method to the
/// single-pass body?
///
/// The case: the site's unreached trap ([`ir_ffm_element_unreached_site`])
/// fired, which records a speculation at the bci (`despec_at_pc`), and the
/// recompile that follows still finds NO receiver for it. n23-w12a showed why.
/// The `UnstableIf` deopt re-queues the method at once
/// (`RecompileAndReinterpret`), so the recompile reads the profile before the
/// resumed interpreted invocation has recorded anything. That recompile
/// refused the method (an intrinsic site with no plan). Its single-pass body
/// records no receiver, so the method stayed there: `HgZero zero` 2,100-2,200
/// ns per n=64 call, against about 125 when warmed in the other order.
///
/// Kept a call, the site's inline cache is the observer. The planner arms its
/// profile seed (a starved in-loop own site), the cache's miss helper records
/// the receiver class it serves and asks for one recompile, and that recompile
/// plans the inline expansion from the one-class row
/// ([`ir_ffm_element_inline_plan`]'s `after_unreached_trap` rule). The call
/// costs the generic dispatch until then. That is bounded: one miss records,
/// and the seed closes after [`IC_PROFILE_SEED_MAX_ENTRIES`] entries.
///
/// All of: both switches and the IC seed switch, an interface or virtual
/// element accessor that is not a first-call site, a speculation at the bci,
/// and a receiver row that is absent or empty. A row with receivers that
/// still did not plan (two classes, say) keeps the old route.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_ffm_element_seed_call_site(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&profile::MethodProfile>,
    despec_at_pc: bool,
) -> bool {
    if first_call
        || !despec_at_pc
        || !matches!(opcode, 0xb6 | 0xb9)
        || !ir::ir_ffm_element_inline_enabled()
        || !ir_ffm_unreached_site_trap_enabled()
        || !ir_ffm_unreached_seed_call_enabled()
        || !ir_ic_profile_seed_enabled()
        || ffm_element_inline::ffm_element_accessor(class, method, desc).is_none()
    {
        return false;
    }
    profile
        .and_then(|p| p.receivers.get(&pc))
        .map_or(true, |counts| {
            profile::ReceiverEvidence::of(counts).summary().observations == 0
        })
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT_KEEP_CALL` (JIT round 23 wave 4, lane
/// L8; perf, default ON, kill `0`): does the root splice planner leave this
/// site a call, because this compile binds it to its judged thin helper
/// ([`ir_ffm_element_direct_entry`])? Spliced, the bind has no `Op::Call` to
/// sit on and the site runs the JDK default body through the generic
/// `VarHandle` road (n23-w3, `R23W2L8HeapBB` segget: 35.9 us/op spliced vs
/// 3.4 kept a call). Compile time only.
pub(crate) fn ir_ffm_element_bound_site_stays_a_call(
    class: &str,
    method: &str,
    desc: &str,
    table: &DirectHelperTable,
) -> bool {
    ir_ffm_element_direct_entry(class, method, desc, table).is_some()
}


/// `CRATONVM_JIT_ZIP_CHECKSUM_DIRECT` (JIT round 22 wave 2, lane intrinsics,
/// INTRINSICS22-2; perf, **default ON (flipped; `=0` kills)**). Read once per candidate site, at
/// compile time.
pub fn zip_checksum_direct_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// The thin helper for an `invokestatic` of `java/util/zip/CRC32.updateBytes0`
/// / `updateBytes` or `java/util/zip/Adler32.updateBytes` (all `(I[BII)I`), or
/// `0`. Both doors ask this. Each triple has its OWN helper, because the
/// helper's decline dispatches the site's own method: `CRC32.updateBytes` is
/// the JDK's range-checking wrapper (its AIOOBE must come from it), and the
/// two natives are registered separately. The registry admission is
/// `direct_native_helper`'s.
pub(crate) fn zip_checksum_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if descriptor != "(I[BII)I" {
        return 0;
    }
    let entry = match (class, method) {
        ("java/util/zip/CRC32", "updateBytes0") => table.crc32_update_bytes0,
        ("java/util/zip/CRC32", "updateBytes") => table.crc32_update_bytes,
        ("java/util/zip/Adler32", "updateBytes") => table.adler32_update_bytes,
        _ => return 0,
    };
    if !zip_checksum_direct_enabled() {
        return 0;
    }
    direct_native_helper(entry, jdk_only, direct_bind_admission, class, method, descriptor)
}

/// `CRATONVM_JIT_VECTORIZED_MISMATCH_DIRECT` (JIT round 22 wave 1, lane
/// intrinsics, INTRINSICS21-1; perf, **default ON (flipped; `=0` kills)**). Read once per candidate
/// site, at compile time.
pub fn vectorized_mismatch_direct_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// The thin helper for an `invokestatic` of
/// `jdk/internal/util/ArraysSupport.vectorizedMismatch(Ljava/lang/Object;JLjava/lang/Object;JII)I`,
/// or `0`. Both doors ask this, so they cannot disagree about the shape.
///
/// Every `Arrays.equals` / `Arrays.mismatch` / `Arrays.compare` range of eight
/// or more bytes, `ByteBuffer.equals` / `mismatch` and the optimizing tier's
/// spliced `Arrays.equals` bodies end here. The callee is a registered
/// `Intrinsic` native (`phases_early::native_arrays_support_vectorized_mismatch`),
/// so compiled code reached it through the generic dispatcher (argument decode,
/// the native funnel, a `Value` per argument) on every call. The helper
/// compares two on-heap primitive arrays whose element width is the scale in
/// place and hands every other shape (a null base, a typed view over a
/// `byte[]`, a range it cannot prove inside both arrays) to that dispatcher,
/// so the native still decides them. The strict-mode admission is the
/// registry's (`direct_native_helper`).
pub(crate) fn vectorized_mismatch_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if class != "jdk/internal/util/ArraysSupport"
        || method != "vectorizedMismatch"
        || descriptor != "(Ljava/lang/Object;JLjava/lang/Object;JII)I"
        || !vectorized_mismatch_direct_enabled()
    {
        return 0;
    }
    direct_native_helper(
        table.vectorized_mismatch,
        jdk_only,
        direct_bind_admission,
        class,
        method,
        descriptor,
    )
}

/// `(checkIndex, reachabilityFence)` sites bound to a thin direct helper.
pub fn census_direct_helper_sites() -> (u64, u64) {
    (
        PRECONDITIONS_CHECK_INDEX_SITES.load(std::sync::atomic::Ordering::Relaxed),
        REACHABILITY_FENCE_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(AtomicInteger, AtomicLong, Object.hashCode string-guard)` intrinsic sites
/// the single-pass backend EMITTED this process.
///
/// Read together, and printed on one line by `vm-cli`, because each of the
/// three counters' own doc comment states the same acceptance criterion in the
/// same words — [`ATOMIC_INTRINSIC_SITES`]: "a perf claim about this family is
/// not believable without checking that this is non-zero — the intrinsic
/// answering the same values as the native it replaced proves nothing about
/// whether it actually ran"; [`OBJECT_HASHCODE_STRING_GUARD_SITES`]: "so 'did
/// this land' is answerable without a timing run". Until this accessor landed,
/// none of the three was answerable at all: `rg -nw` on all three names
/// returned declarations, `fetch_add`s, doc comments and one line of
/// `jit/tests/process_global_statics_ratchet.rs`'s static inventory, and no
/// `.load(` anywhere
/// (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// The 32- and 64-bit atomic families go on one line because they are ONE bind
/// decision taken twice: `try_resolve_atomic_intrinsic_for_site` and
/// `try_resolve_atomic_long_intrinsic_for_site` are dispatched from the same
/// `match` on the receiver class name, gate on the same always-emitted
/// exact-class-id guard (neither `AtomicInteger` nor `AtomicLong` is final), and
/// are switched off by sibling flags. A workload that binds one and not the
/// other is the interesting reading, and it is only visible when the two
/// numbers are adjacent — a single "atomic intrinsics" total that moves says
/// which family landed no better than a timing run does.
///
/// The `Object.hashCode` string guard rides the same line for a different
/// reason: it is the third member of the set of single-pass binds whose doc
/// promises "answerable without a timing run", and it is the one whose zero is
/// most easily misread. It is speculative — a site is only bound when the
/// receiver profile is String-dominant AND the speculation has not already been
/// despecialised — so zero means "no site had enough String-dominant profile
/// evidence", NOT "the guard is off". `CRATONVM_JIT_OBJECT_HASHCODE_STRING`
/// defaults ON, so the flag is almost never the explanation, and an operator
/// who reads the zero as "feature disabled" goes and sets a flag that was
/// already set.
///
/// Returns raw relaxed loads: monotonic, never decremented, and counted at
/// EMISSION rather than in the matcher (review #80), so a code-buffer retry
/// that re-emits a method counts its sites again. A number here is therefore an
/// upper bound on distinct source sites, which is why it is only ever compared
/// against zero and against its siblings, never divided by anything.
pub fn intrinsic_site_census() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        // `ATOMIC_INTRINSIC_SITES` and `ATOMIC_LONG_INTRINSIC_SITES` are
        // `AtomicUsize`, whose `load` returns `usize`; the cast widens on the
        // 64-bit targets this VM builds for and is lossless on 32-bit ones
        // too, since `usize` is never wider than `u64` on any supported target.
        // `OBJECT_HASHCODE_STRING_GUARD_SITES` is already `AtomicU64` and needs
        // no cast.
        ATOMIC_INTRINSIC_SITES.load(Relaxed) as u64,
        ATOMIC_LONG_INTRINSIC_SITES.load(Relaxed) as u64,
        OBJECT_HASHCODE_STRING_GUARD_SITES.load(Relaxed),
    )
}

/// `CRATONVM_JIT_NO_LONG_INTRINSICS`, read once. The matcher runs for every
/// candidate call site of every compile, so an environment lookup there was a
/// per-site cost for a switch that cannot change mid-run.
pub(crate) fn no_long_intrinsics() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// `CRATONVM_JIT_STATIC_INTRINSIC_FIRST` (**default ON**; `0` restores the old
/// order): the single-pass method compile gives an `invokestatic` that
/// [`try_resolve_intrinsic`] matches to the intrinsic ladder instead of first
/// splicing or direct-binding the callee's JDK body. Round 13 wave 1, lane
/// callcost; asked only at a matching site, so read live (no cache static).
pub(crate) fn static_intrinsic_first_enabled() -> bool {
    true
}



/// `CRATONVM_JIT_STRICTMATH_ROUNDING_EXACT` (JIT round 21 wave 3, lane
/// intrinsics; correctness, **default ON**, `0` restores the `ROUNDSD` arms):
/// `StrictMath.floor` / `ceil` / `rint` are NOT answered by `ROUNDSD` in
/// either tier ([`try_resolve_intrinsic`]'s Math family, and
/// `ir::try_ir_scalar_intrinsic`'s `FloorD` / `CeilD` / `RintD` rows); the
/// site keeps its call, i.e. the registered `Intrinsic` native, which answers
/// the JDK body bit for bit (`lang_math::native_math_floor`,
/// `CRATONVM_MATH_ROUNDING_NAN_EXACT`).
///
/// Why only `StrictMath`: HotSpot 25 intrinsifies `java.lang.Math.floor` /
/// `ceil` / `rint` (vmIntrinsics `_floor` / `_ceil` / `_rint`, C2
/// `RoundDoubleMode`, i.e. the same `ROUNDSD`), so a hot `Math` site's answer
/// IS `ROUNDSD`'s, while `StrictMath`'s bodies always run as bytecode there:
/// `floorOrCeil` returns a NaN argument unchanged (`ROUNDSD` quiets a
/// signalling one) and `rint`'s `sign * abs(a)` clears a NaN's sign (`ROUNDSD`
/// keeps it). Asked only at a `StrictMath` rounding site, so read live.
pub fn strictmath_rounding_exact_enabled() -> bool {
    true
}

/// Resolve a method invocation to a JIT call-site intrinsic, if one applies.
///
/// Returns `Some((entry, num_params, return_type))` where `entry` is the
/// [`JitIntrinsic::as_entry`] sentinel, `num_params` is the JLS argument
/// count *excluding* any receiver, and `return_type` is the JVM type tag
/// of the result. Returns `None` when no intrinsic matches, when the
/// required CPU feature is absent, or when inlining would be incorrect —
/// in which case the call falls back to the normal dispatch path.
///
/// CPU-feature gating must match the codegen ladder in `x64.rs` exactly
/// (e.g. floor/ceil/rint require `x64::has_sse41()`), so that a method is
/// never registered as an intrinsic the codegen cannot emit.
///
/// **Per-family regions.** The Math family is matched inline below. Every
/// other family has an empty `INTRINSIC REGION BEGIN/END: <TAG>` block;
/// a follow-up agent fills exactly one region with an
/// `if let Some(hit) = <match>; return Some(hit)` block. Because each
/// region is an independent statement, 8 agents editing 8 regions never
/// produce a merge conflict.
pub fn try_resolve_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
) -> Option<(usize, usize, u8)> {
    // --- java.lang.Math / java.lang.StrictMath family ---
    if class == "java/lang/StrictMath"
        && matches!((name, descriptor), ("floor" | "ceil" | "rint", "(D)D"))
        && strictmath_rounding_exact_enabled()
    {
        // JIT round 21 wave 3, lane intrinsics: the JDK body's NaN bits, not
        // `ROUNDSD`'s; see `strictmath_rounding_exact_enabled`.
        return None;
    }
    if class == "java/lang/Math" || class == "java/lang/StrictMath" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("sqrt", "(D)D") => Some((JitIntrinsic::MathSqrt, 1, b'D')),
            ("floor", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathFloor, 1, b'D')),
            ("ceil", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathCeil, 1, b'D')),
            ("rint", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathRint, 1, b'D')),
            ("abs", "(D)D") => Some((JitIntrinsic::MathAbsDouble, 1, b'D')),
            ("abs", "(F)F") => Some((JitIntrinsic::MathAbsFloat, 1, b'F')),
            ("abs", "(I)I") => Some((JitIntrinsic::MathAbsInt, 1, b'I')),
            ("abs", "(J)J") => Some((JitIntrinsic::MathAbsLong, 1, b'J')),
            // T1.1.28 — Math.fma (fused multiply-add).
            ("fma", "(DDD)D") => Some((JitIntrinsic::MathFmaDouble, 3, b'D')),
            ("fma", "(FFF)F") => Some((JitIntrinsic::MathFmaFloat, 3, b'F')),
            // Round-8 Bug 8 — branchless integer min/max via CMOV.
            ("min", "(II)I") => Some((JitIntrinsic::MathMinInt, 2, b'I')),
            ("max", "(II)I") => Some((JitIntrinsic::MathMaxInt, 2, b'I')),
            ("min", "(JJ)J") => Some((JitIntrinsic::MathMinLong, 2, b'J')),
            ("max", "(JJ)J") => Some((JitIntrinsic::MathMaxLong, 2, b'J')),
            // The float/double twins of the two lines above. These four were
            // missing for a long time, so a `Math.min(float,float)` in a hot
            // loop paid a full Java-method dispatch into a JDK body that
            // itself calls `Float.floatToRawIntBits` — measured at ~74 ns a
            // call, 47% of a ray-tracer kernel. See `JitIntrinsic::MathMinFloat`.
            ("min", "(FF)F") => Some((JitIntrinsic::MathMinFloat, 2, b'F')),
            ("max", "(FF)F") => Some((JitIntrinsic::MathMaxFloat, 2, b'F')),
            ("min", "(DD)D") => Some((JitIntrinsic::MathMinDouble, 2, b'D')),
            ("max", "(DD)D") => Some((JitIntrinsic::MathMaxDouble, 2, b'D')),
            // High 64 bits of the 128-bit product — one `IMUL`/`MUL r64`.
            // Hottest leaf in SunEC P-256 Montgomery field arithmetic.
            ("multiplyHigh", "(JJ)J") => Some((JitIntrinsic::MathMultiplyHigh, 2, b'J')),
            ("unsignedMultiplyHigh", "(JJ)J") => {
                Some((JitIntrinsic::MathUnsignedMultiplyHigh, 2, b'J'))
            }
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }

    // ===== INTRINSIC REGION BEGIN: SCALAR_ROWS_SP =====
    // JIT round 22 wave 2, lane intrinsics: single-pass twins of optimizing-tier
    // scalar rows, each registered only where `ir::try_ir_scalar_intrinsic`
    // claims the same triple (else the planner's `is_intrinsic_site` refusal
    // would pin the caller to this backend). See `JitIntrinsic::MathRoundDouble`.
    if let Some(row) = sp_scalar_row(class, name, descriptor) {
        return Some(row);
    }
    // ===== INTRINSIC REGION END: SCALAR_ROWS_SP =====

    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
    // java.lang.Integer bit-manipulation intrinsics (Phase 1a). Each maps to
    // one or two x86-64 instructions, all pure leaves with no memory access.
    // CPU-feature gates here MUST match the codegen ladder in x64.rs exactly:
    //   * bitCount needs POPCNT.
    //   * numberOfLeadingZeros / numberOfTrailingZeros are emitted on every
    //     host — LZCNT/TZCNT when available, otherwise a BSR/BSF sequence
    //     with the input-zero fixup — so they are not feature-gated.
    //   * reverseBytes (BSWAP), highestOneBit, lowestOneBit, reverse and
    //     compare use only baseline instructions.
    if class == "java/lang/Integer" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("bitCount", "(I)I") if x64::has_popcnt() => Some((JitIntrinsic::IntBitCount, 1, b'I')),
            ("numberOfLeadingZeros", "(I)I") => {
                Some((JitIntrinsic::IntNumberOfLeadingZeros, 1, b'I'))
            }
            ("numberOfTrailingZeros", "(I)I") => {
                Some((JitIntrinsic::IntNumberOfTrailingZeros, 1, b'I'))
            }
            ("reverseBytes", "(I)I") => Some((JitIntrinsic::IntReverseBytes, 1, b'I')),
            ("highestOneBit", "(I)I") => Some((JitIntrinsic::IntHighestOneBit, 1, b'I')),
            ("lowestOneBit", "(I)I") => Some((JitIntrinsic::IntLowestOneBit, 1, b'I')),
            ("reverse", "(I)I") => Some((JitIntrinsic::IntReverse, 1, b'I')),
            ("compare", "(II)I") => Some((JitIntrinsic::IntCompare, 2, b'I')),
            ("rotateLeft", "(II)I") => Some((JitIntrinsic::IntRotateLeft, 2, b'I')),
            ("rotateRight", "(II)I") => Some((JitIntrinsic::IntRotateRight, 2, b'I')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: INT_BITS =====

    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
    // java.lang.Long bit-manipulation intrinsics (Phase 1b). All operate on a
    // single 64-bit operand (occupying one JIT stack slot) and lower to
    // REX.W-prefixed instructions. CPU-feature gates here MUST match the
    // codegen ladder in x64.rs exactly:
    //   * bitCount needs POPCNT.
    //   * numberOfLeadingZeros / numberOfTrailingZeros are emitted on every
    //     host — LZCNT/TZCNT when available, otherwise a BSR/BSF sequence
    //     with the input-zero fixup — so they are not feature-gated.
    //   * reverseBytes (BSWAP), highestOneBit, lowestOneBit and compare use
    //     only baseline instructions.
    //   * Long.reverse is intentionally NOT registered: it has no single-
    //     instruction lowering and the multi-mask SWAR sequence is omitted in
    //     favour of safe fallback to normal dispatch (roadmap §3.4).
    if class == "java/lang/Long" && !no_long_intrinsics() {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("bitCount", "(J)I") if x64::has_popcnt() => {
                Some((JitIntrinsic::LongBitCount, 1, b'I'))
            }
            ("numberOfLeadingZeros", "(J)I") => {
                Some((JitIntrinsic::LongNumberOfLeadingZeros, 1, b'I'))
            }
            ("numberOfTrailingZeros", "(J)I") => {
                Some((JitIntrinsic::LongNumberOfTrailingZeros, 1, b'I'))
            }
            ("reverseBytes", "(J)J") => Some((JitIntrinsic::LongReverseBytes, 1, b'J')),
            ("highestOneBit", "(J)J") => Some((JitIntrinsic::LongHighestOneBit, 1, b'J')),
            ("lowestOneBit", "(J)J") => Some((JitIntrinsic::LongLowestOneBit, 1, b'J')),
            ("compare", "(JJ)I") => Some((JitIntrinsic::LongCompare, 2, b'I')),
            ("rotateLeft", "(JI)J") => Some((JitIntrinsic::LongRotateLeft, 2, b'J')),
            ("rotateRight", "(JI)J") => Some((JitIntrinsic::LongRotateRight, 2, b'J')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: LONG_BITS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
    // java.lang.System.arraycopy (Phase 2). The descriptor is type-erased
    // — the element kind is unknown until runtime — so a SINGLE variant
    // (`ArraycopyPrimitive`) is registered. The x64 codegen emits a
    // runtime dispatch: if both arrays are non-null, are arrays, share the
    // same PRIMITIVE element kind, and all five positions are in bounds,
    // it inlines a memmove-correct `REP MOVSB`. Every other case (null,
    // non-array, reference array, mismatched/incompatible element kinds,
    // out-of-bounds) takes the uncommon-trap deopt stub, which re-runs the
    // call in the interpreter via the native `System.arraycopy` — that
    // path preserves NPE / ArrayStoreException / AIOOBE and the GC store
    // barrier verbatim. Reference arrays therefore intentionally bail to
    // native (roadmap §3.4: never trade correctness for inlining).
    if class == "java/lang/System"
        && name == "arraycopy"
        && descriptor == "(Ljava/lang/Object;ILjava/lang/Object;II)V"
    {
        return Some((JitIntrinsic::ArraycopyPrimitive.as_entry(), 5, b'V'));
    }
    // ===== INTRINSIC REGION END: ARRAYCOPY =====

    // ===== SCOPED_MEMORY_UNALIGNED: REGION RETIRED (r10, lane `pagefix`) =====
    // There is no region here any more. The four `ScopedMemoryGet*Unaligned`
    // enum variants this comment used to point at were deleted from
    // `intrinsic_catalogue.rs`: they were matched by nothing, emitted by
    // nothing, and registered by nothing, while their own doc comment
    // described them in the present tense as if they were live. The surviving
    // mechanism for this family is `direct_helpers::ScopedMemoryOp` via
    // `DirectHelperTable`, which is wired through this file and
    // `vm/src/runtime/interpreter/jit_bridge.rs`.
    //
    // The invariant the old comment existed to protect still holds and is
    // still worth stating: registering a sentinel the codegen cannot emit is
    // the one failure mode this matcher must never have, because `try_compile`
    // would push a `direct_calls` entry whose `entry` no arm recognises. The
    // catch-all that enforces it is `JitIntrinsic::is_sentinel_entry` in
    // `x64/op_invoke.rs`, which fails the compile rather than emitting a trap.

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // java.lang.String access intrinsics (length/charAt/isEmpty/hashCode).
    // These ARE intrinsified, but only when a `StringFieldLayout` is
    // available — and the layout is NOT a parameter of this 3-argument
    // matcher (kept stable for the INT_BITS / LONG_BITS / ARRAYCOPY /
    // ARRAYS_* / CRC32 families and their tests). The String family is
    // therefore matched by the layout-aware `try_resolve_string_intrinsic`
    // below, which `try_compile_inner` calls instead. This 3-arg entry
    // point deliberately registers nothing for `java/lang/String`, so a
    // bare 3-arg call (no layout context) safely falls back to dispatch.
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // java.lang.String search intrinsics (equals): same story as
    // STRING_ACCESS — matched by the layout-aware
    // `try_resolve_string_intrinsic` below, not this 3-arg entry point.
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
    // java.util.Arrays.fill / Arrays.equals — Phase 4a.
    //
    // Both families are pure leaf calls over primitive arrays with no
    // safepoint. `num_params` excludes the (absent) receiver: `fill` takes
    // 2 (array, value), `equals` takes 2 (array a, array b).
    //
    // Deliberately NOT registered (fall back to native dispatch):
    //   * `fill([FF)V` / `fill([DD)V` — the fill value is an FP operand that
    //     the JIT keeps in an XMM stack slot; routing it through the integer
    //     REP STOS path is not provably correct, so we bail.
    //   * The 3-arg ranged `fill(...IIX)V` overloads — out of scope.
    //   * `equals` reference-array / `Object[]` overloads — element equality
    //     requires calling `Object.equals`, which is not a leaf op.
    //   * `Arrays.deepEquals`, `Arrays.hashCode`, etc. — not targeted here.
    if class == "java/util/Arrays" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            // --- fill(array, value) : void --- (2 args, void return)
            ("fill", "([BB)V") => Some((JitIntrinsic::ArraysFill1, 2, b'V')),
            ("fill", "([ZZ)V") => Some((JitIntrinsic::ArraysFill1, 2, b'V')),
            ("fill", "([CC)V") => Some((JitIntrinsic::ArraysFill2, 2, b'V')),
            ("fill", "([SS)V") => Some((JitIntrinsic::ArraysFill2, 2, b'V')),
            ("fill", "([II)V") => Some((JitIntrinsic::ArraysFill4, 2, b'V')),
            ("fill", "([JJ)V") => Some((JitIntrinsic::ArraysFill8, 2, b'V')),
            // --- fill(array, fromIndex, toIndex, value) : void --- (4 args, void return)
            ("fill", "([BIIB)V") => Some((JitIntrinsic::ArraysFillRange1, 4, b'V')),
            ("fill", "([ZIIZ)V") => Some((JitIntrinsic::ArraysFillRange1, 4, b'V')),
            ("fill", "([CIIC)V") => Some((JitIntrinsic::ArraysFillRange2, 4, b'V')),
            ("fill", "([SIIS)V") => Some((JitIntrinsic::ArraysFillRange2, 4, b'V')),
            ("fill", "([IIII)V") => Some((JitIntrinsic::ArraysFillRange4, 4, b'V')),
            ("fill", "([JIIJ)V") => Some((JitIntrinsic::ArraysFillRange8, 4, b'V')),
            // --- equals(a, b) : boolean --- (2 args, int/boolean return)
            ("equals", "([B[B)Z") => Some((JitIntrinsic::ArraysEquals1, 2, b'Z')),
            ("equals", "([Z[Z)Z") => Some((JitIntrinsic::ArraysEquals1, 2, b'Z')),
            ("equals", "([C[C)Z") => Some((JitIntrinsic::ArraysEquals2, 2, b'Z')),
            ("equals", "([S[S)Z") => Some((JitIntrinsic::ArraysEquals2, 2, b'Z')),
            ("equals", "([I[I)Z") => Some((JitIntrinsic::ArraysEquals4, 2, b'Z')),
            ("equals", "([J[J)Z") => Some((JitIntrinsic::ArraysEquals8, 2, b'Z')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
    // java.util.Arrays.sort(prim[]) — single-argument overloads for the five
    // integral element types. The codegen emits an in-place insertion sort
    // (O(n^2), but provably correct for all lengths including empty/single).
    //
    // float[]/double[] are deliberately excluded: their JLS ordering uses
    // Double.compare semantics (NaN sorts last, -0.0 before +0.0) which a
    // plain signed compare does not honour. The 3-argument range overloads
    // (sort([III)V etc.) are also out of scope. Both fall through to None
    // and use the normal dispatch path.
    if class == "java/util/Arrays" && name == "sort" {
        let hit: Option<JitIntrinsic> = match descriptor {
            "([I)V" => Some(JitIntrinsic::ArraysSortInt),
            "([J)V" => Some(JitIntrinsic::ArraysSortLong),
            "([C)V" => Some(JitIntrinsic::ArraysSortChar),
            "([S)V" => Some(JitIntrinsic::ArraysSortShort),
            "([B)V" => Some(JitIntrinsic::ArraysSortByte),
            _ => None,
        };
        if let Some(intrinsic) = hit {
            // num_params = 1 (the array ref), return kind 'V' (void).
            return Some((intrinsic.as_entry(), 1, b'V'));
        }
    }
    // ===== INTRINSIC REGION END: ARRAYS_SORT =====

    // ===== INTRINSIC REGION BEGIN: CRC32 =====
    // java.util.zip.CRC32 / CRC32C `update` call-site intrinsics (Phase 4c).
    //
    // The foundation wave (commit 2fb0df0) pinned the receiver layout
    // (crc_layout_contract.md): both classes carry exactly one
    // instance field — `private int crc` at slot 0 — holding the running,
    // uncomplemented CRC state. It also added a bit-exact native CRC32C
    // (native-builtins/src/zip_crc32c.rs) that serves as the differential
    // oracle. With a stable layout and an oracle, both `update` overloads are
    // soundly inlinable:
    //
    //   * CRC32C — the x86 `CRC32` instruction computes exactly the
    //     Castagnoli CRC-32C, so the codegen folds bytes with it directly.
    //     Gated on `has_sse42()` (the codegen ladder applies the same gate;
    //     when SSE4.2 is absent the intrinsic is not registered and the call
    //     dispatches to the native CRC32C override).
    //   * CRC32 — IEEE 802.3 (reflected poly 0xEDB88320). The hardware
    //     `CRC32` instruction is the wrong polynomial; the codegen emits a
    //     tight inline reflected-CRC bit loop instead (no table, no CALL),
    //     correct on every host, so this variant needs no CPU gate.
    //
    // `num_params` excludes the receiver — `update(I)V` is 1, `update([BII)V`
    // is 3. The 0xb6 codegen ladder adds the receiver back (`num_params + 1`)
    // and emits the receiver class-id guard. `update([B)V` (whole-array) is
    // intentionally NOT registered: it is a separate overload that the native
    // path already handles; keeping the intrinsic surface to the two hot
    // signatures the brief targets avoids speculative codegen.
    if class == "java/util/zip/CRC32C" {
        match (name, descriptor) {
            ("update", "(I)V") if x64::has_sse42() => {
                return Some((JitIntrinsic::Crc32cUpdateByte.as_entry(), 1, b'V'));
            }
            ("update", "([BII)V") if x64::has_sse42() => {
                return Some((JitIntrinsic::Crc32cUpdateBytes.as_entry(), 3, b'V'));
            }
            _ => {}
        }
    }
    // Real-JDK CRC32 stores its public (not complemented running) value in
    // `crc`. Keep archive creation on the proven updateBytes0 native path
    // until every compiled call shape has one state-representation contract.
    // CRC32C uses its own running-state contract and remains eligible above.
    if class == "java/util/zip/CRC32" {
        return None;
    }
    // ===== INTRINSIC REGION END: CRC32 =====

    // ===== INTRINSIC REGION BEGIN: FP_BITS =====
    // See the enum region of the same tag for the census that motivated this
    // and for why only the RAW conversion is admitted.
    if class == "java/lang/Double" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("doubleToRawLongBits", "(D)J") => Some((JitIntrinsic::DoubleToRawLongBits, 1, b'J')),
            ("longBitsToDouble", "(J)D") => Some((JitIntrinsic::LongBitsToDouble, 1, b'D')),
            // `doubleToLongBits` canonicalises NaN and is deliberately absent.
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // The `Float` pair (round 15 wave 7, lane drainB; IR2-2): until now a
    // single-pass body paid the dispatch funnel for these while the optimizing
    // tier emits one `MOVD` (`ScalarOp::FloatToRawIntBits` / `IntBitsToFloat`).
    // x86-64 only: the aarch64 backend emits neither `Double` nor `Float` arm.
    // `floatToIntBits` canonicalises NaN and is deliberately absent.
    if class == "java/lang/Float" && cfg!(target_arch = "x86_64") {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("floatToRawIntBits", "(F)I") => Some((JitIntrinsic::FloatToRawIntBits, 1, b'I')),
            ("intBitsToFloat", "(I)F") => Some((JitIntrinsic::IntBitsToFloat, 1, b'F')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: FP_BITS =====

    None
}

/// Layout-aware matcher for the `java/lang/String` call-site intrinsics
/// (the STRING_ACCESS and STRING_SEARCH families).
///
/// Unlike [`try_resolve_intrinsic`], a String intrinsic can only be inlined
/// when the JIT has resolved `java/lang/String`'s heap field layout — the
/// codegen must read the receiver's `value` (`byte[]`), `coder` (`byte`)
/// and `hash` (`int`) fields with inline machine code. That layout is not a
/// `(class, name, descriptor)` property, so it cannot live in the 3-arg
/// `try_resolve_intrinsic` (whose signature is shared with — and pinned by
/// the tests of — every non-String family). `try_compile_inner` therefore
/// calls THIS function for instance-method invokes, passing the
/// `StringFieldLayout` it resolved once for the compilation.
///
/// Returns `Some((entry, num_params, return_type, guard_class_id))`. The
/// `guard_class_id` is `0` for a statically-monomorphic `java/lang/String`
/// receiver (no runtime guard needed — String is `final`) and the
/// `java/lang/String` `ObjectHeader` class id for a `java/lang/CharSequence`
/// receiver (the codegen then emits a `[recv+0] == guard` check, deopting to
/// native dispatch for any non-String CharSequence). Returns `None` — so the
/// call falls back to normal native dispatch — when:
///   * `class` is neither `java/lang/String` nor `java/lang/CharSequence`;
///   * `string_layout` is `None` (String not loaded / resolver unavailable);
///   * the resolved layout has no `coder` field (`has_coder == false`, the
///     legacy `char[]` String layout) — every inlined String intrinsic
///     decodes via `coder`, so a layout without it cannot be inlined;
///   * a CharSequence site has no resolved String class id to guard against;
///   * `(name, descriptor)` is not one of the inlined signatures.
///
/// CharSequence eligibility is limited to the accessors CharSequence actually
/// declares — `charAt`/`length`/`isEmpty`; the String-specific
/// `hashCode`/`equals`/`compareTo`/`indexOf` are `java/lang/String` only.
///
/// The codegen ladder in `x64.rs` (the 0xb6/b7/b9 STRING_ACCESS /
/// STRING_SEARCH regions) consumes `compiler.string_layout`, which is
/// populated from the SAME `string_layout_resolver`, so the matcher's
/// "registered" decision and the codegen's "can emit" decision are always
/// consistent within one compilation — a registered String sentinel is
/// never left for the plain direct-call path to mis-`CALL`.
/// Matcher for the ATOMIC_INT region.
///
/// Mirrors [`try_resolve_string_intrinsic`]: returns the intrinsic entry, the
/// parameter count (excluding the receiver), the return type tag, and the
/// receiver class id to guard on. Returns `None` — leaving the call to ordinary
/// native dispatch — whenever the layout did not resolve.
///
/// The guard is ALWAYS emitted for this family. Unlike `java/lang/String`,
/// `AtomicInteger` is not final, so a receiver could be a subclass that
/// overrides `getAndIncrement`; only an exact class-id match may take the
/// inline path.
/// Number of ATOMIC_INT call sites (the `XADD`/load arm and the `CMPXCHG` arm)
/// the single-pass backend has EMITTED inline this process.
///
/// Bumped where the codegen emits the site, not in the matcher (review #80): a
/// registered site can still be dropped before emission, and the matcher is a
/// query, so counting there counted questions rather than sites. A code-buffer
/// retry re-emits its method and counts its sites again.
/// `CRATONVM_DBG_ATOMIC_INTRINSIC=1` prints each matcher hit. A perf claim about this
/// family is not believable without checking that this is non-zero — the
/// intrinsic answering the same values as the native it replaced proves
/// nothing about whether it actually ran.
pub static ATOMIC_INTRINSIC_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `CRATONVM_DBG_INTRINSIC=1` — trace which compiled bodies get the call-site
/// intrinsic pass at all.
///
/// Added 2026-08-13. Every call-site intrinsic (Math, Integer/Long bits,
/// AtomicInteger, String, and the HashMap/Integer/ConcurrentMap thin direct
/// binds) is registered in ONE place: the single-pass invoke loop near the end
/// of `try_compile_inner`. A body produced by the optimizing IR pipeline
/// returns from that function *before* the loop runs, so it carries none of
/// them — a fact with no other observable trace, which is why an intrinsic can
/// measure as completely inert while its resolver, codegen and unit tests are
/// all correct and all green. This flag prints one line per single-pass invoke
/// site and one line per IR body, so "did this site even get offered an
/// intrinsic" is answerable in one run.
pub(crate) fn dbg_intrinsic_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1` — keep every `AtomicInteger` RMW call
/// on ordinary native dispatch. The kill switch for bisecting a suspected
/// miscompile, and the B arm of an in-binary A/B (cross-run wall time on a
/// shared host is not a measurement).
/// `CRATONVM_JIT_NO_FFM_INTRINSIC=1` — the FFM element-accessor kill switch.
/// See [`ffm_kind_for_descriptor`].
/// `CRATONVM_JIT_FFM_OFFSET_ACCESS` (default on): the byte-offset element
/// accessors `MemorySegment.get/set(ValueLayout$OfX, long[, X])` take the same
/// single-pass fast path as `getAtIndex`/`setAtIndex` (the emitter converts an
/// aligned offset into an index; see the FFM_SEGMENT region of
/// `x64/op_invoke.rs`). Off keeps them on native dispatch. Read at site
/// registration, not per element.
pub fn ffm_offset_access_enabled() -> bool {
    true
}

/// Whether `method_name` on `java/lang/foreign/MemorySegment` names an FFM
/// ELEMENT accessor the single-pass fast path serves (with
/// [`ffm_kind_for_descriptor`] deciding the element kind). Every door that
/// registers, routes or refuses to splice those sites asks this one predicate.
pub fn ffm_element_accessor_name(method_name: &str) -> bool {
    matches!(method_name, "getAtIndex" | "setAtIndex")
        || (matches!(method_name, "get" | "set") && ffm_offset_access_enabled())
}

/// The read half of [`ffm_element_accessor_name`].
pub fn ffm_element_accessor_is_get(method_name: &str) -> bool {
    matches!(method_name, "getAtIndex" | "get")
}

pub(crate) fn ffm_intrinsic_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

pub(crate) fn atomic_intrinsic_disabled() -> bool {
    false
}

pub fn try_resolve_atomic_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    try_resolve_atomic_intrinsic_for_site(class, None, name, descriptor, guard_class_id)
}

/// Is `name descriptor` one of the methods the ATOMIC_INT / ATOMIC_LONG ladders
/// claim AND declared `final` by `jdk_class` in the JDK?
///
/// A `final` method cannot be overridden, so a site that resolves to it runs
/// exactly the JDK body whatever the receiver's subclass — the one proof this
/// crate can supply without a class-hierarchy resolver. The RMW family
/// (`get*`, `getAnd*`, `*AndGet`, `compareAndSet`, `weakCompareAndSet`) is
/// `public final` on both classes. `AtomicLong.longValue()` is claimed by the
/// long ladder but is an ordinary overridable `Number` accessor, so it is
/// deliberately absent: an exact `AtomicLong` site may take it, a subclass
/// site may not.
pub(crate) fn atomic_intrinsic_method_is_final_in_jdk(jdk_class: &str, name: &str, descriptor: &str) -> bool {
    match jdk_class {
        "java/util/concurrent/atomic/AtomicInteger" => matches!(
            (name, descriptor),
            ("get", "()I")
                | ("getPlain", "()I")
                | ("getAcquire", "()I")
                | ("getAndIncrement", "()I")
                | ("getAndDecrement", "()I")
                | ("incrementAndGet", "()I")
                | ("decrementAndGet", "()I")
                | ("getAndAdd", "(I)I")
                | ("compareAndSet", "(II)Z")
                | ("weakCompareAndSet", "(II)Z")
                | ("addAndGet", "(I)I")
        ),
        "java/util/concurrent/atomic/AtomicLong" => matches!(
            (name, descriptor),
            ("get", "()J")
                | ("getPlain", "()J")
                | ("getAcquire", "()J")
                | ("getAndIncrement", "()J")
                | ("getAndDecrement", "()J")
                | ("incrementAndGet", "()J")
                | ("decrementAndGet", "()J")
                | ("getAndAdd", "(J)J")
                | ("compareAndSet", "(JJ)Z")
                | ("weakCompareAndSet", "(JJ)Z")
                | ("addAndGet", "(J)J")
        ),
        _ => false,
    }
}

/// May an invoke site be matched against the `Atomic*` intrinsic family of
/// `jdk_class`? (review #80)
///
/// * Yes when the constant-pool class IS `jdk_class` — the historical match.
/// * Otherwise only when method resolution says the site's method is DECLARED
///   by `jdk_class` (`resolved_declaring_class`) AND that method is `final`
///   there ([`atomic_intrinsic_method_is_final_in_jdk`]). `class Counter
///   extends AtomicInteger` calling `incrementAndGet()` names `Counter` in its
///   constant pool, yet binds to the JDK body no subclass can replace.
///
/// A declaring class that is NOT `jdk_class` — an intermediate class that could
/// have overridden the method, or no resolution at all — never matches, so this
/// can never intrinsify a call a subclass could override.
pub fn atomic_intrinsic_site_class_matches(
    jdk_class: &str,
    cp_class: &str,
    resolved_declaring_class: Option<&str>,
    name: &str,
    descriptor: &str,
) -> bool {
    cp_class == jdk_class
        || (resolved_declaring_class == Some(jdk_class)
            && atomic_intrinsic_method_is_final_in_jdk(jdk_class, name, descriptor))
}

/// [`try_resolve_atomic_intrinsic`] for a site whose constant-pool class may be
/// a SUBCLASS of `AtomicInteger`; see [`atomic_intrinsic_site_class_matches`]
/// for when that is admitted.
///
/// `resolved_declaring_class` is the class that declares the method the invoke
/// resolves to. `try_compile_inner` fills it from the VM's
/// `cp_invoke_declaring_class_resolver` (see
/// `atomic_intrinsic_for_invoke_site`), and the OSR door in
/// `vm/src/runtime/interpreter/jit_bridge.rs` resolves it through the
/// class-manager guard it already holds.
///
/// `guard_class_id` stays the SITE's class id for a subclass site: the emitted
/// guard is an exact class-id compare, so only receivers of exactly that class
/// take the inline path. The layout is slot 0 resolved through that class id.
/// Slot 0 is `value` on every subclass instance, because `compute_field_layout`
/// (`classloading/src/class_manager.rs`) gives superclass instance fields the
/// slots `0..N` and starts a class's own fields at `N`. The compact offset is
/// read from the subclass's own registered layout.
pub fn try_resolve_atomic_intrinsic_for_site(
    class: &str,
    resolved_declaring_class: Option<&str>,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    if !atomic_intrinsic_site_class_matches(
        "java/util/concurrent/atomic/AtomicInteger",
        class,
        resolved_declaring_class,
        name,
        descriptor,
    ) {
        return None;
    }
    if atomic_intrinsic_disabled() {
        return None;
    }
    // The layout is derived from the SITE's declared class id (via
    // `cp_invoke_class_id_resolver`) and field slot 0 — the same slot the
    // registered native addresses (`get_field_volatile(this, 0)`). Deriving it
    // here and again in the codegen from the same two inputs keeps the
    // matcher's "registered" decision and the codegen's "can emit" decision
    // from ever disagreeing, without threading a layout through `try_compile`.
    //
    // `AtomicIntFieldLayout::new` returns `None` when the compact storage
    // width is not exactly 4 bytes, so a layout this 32-bit `LOCK XADD` could
    // not address never reaches codegen — the call keeps its native dispatch.
    let layout = AtomicIntFieldLayout::new(0, guard_class_id)?;
    if layout.class_id == 0 {
        return None;
    }
    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
    let hit: Option<(JitIntrinsic, usize)> = match (name, descriptor) {
        ("get", "()I") => Some((JitIntrinsic::AtomicIntGet, 0)),
        ("getPlain", "()I") => Some((JitIntrinsic::AtomicIntGet, 0)),
        ("getAcquire", "()I") => Some((JitIntrinsic::AtomicIntGet, 0)),
        ("getAndIncrement", "()I") => Some((JitIntrinsic::AtomicIntGetAndIncrement, 0)),
        ("getAndDecrement", "()I") => Some((JitIntrinsic::AtomicIntGetAndDecrement, 0)),
        ("incrementAndGet", "()I") => Some((JitIntrinsic::AtomicIntIncrementAndGet, 0)),
        ("decrementAndGet", "()I") => Some((JitIntrinsic::AtomicIntDecrementAndGet, 0)),
        ("getAndAdd", "(I)I") => Some((JitIntrinsic::AtomicIntGetAndAdd, 1)),
        ("compareAndSet", "(II)Z") => Some((JitIntrinsic::AtomicIntCompareAndSet, 2)),
        ("weakCompareAndSet", "(II)Z") => Some((JitIntrinsic::AtomicIntCompareAndSet, 2)),
        ("addAndGet", "(I)I") => Some((JitIntrinsic::AtomicIntAddAndGet, 1)),
        _ => None,
    };
    // ===== INTRINSIC REGION END: ATOMIC_INT =====
    let (intrinsic, num_params) = hit?;
    if false {
        eprintln!(
            "[atomic-intrinsic] {}.{}{} class_id={} compact_off={} legacy_off={}",
            class,
            name,
            descriptor,
            layout.class_id,
            layout.value_compact_offset,
            layout.value_legacy_offset,
        );
    }
    // `compareAndSet` is the one member of this family whose result is not the
    // field: it reports success, so its tag is `Z`. Everything else returns the
    // 32-bit payload. (`Z` and `I` take the same `push_from_rax` arm in the
    // emitter's return ladder — only `D`/`F` and `L`/`[` are special-cased —
    // so this is about the SIGNATURE the caller sees, not the encoding.)
    let ret = if matches!(intrinsic, JitIntrinsic::AtomicIntCompareAndSet) {
        b'Z'
    } else {
        b'I'
    };
    Some((intrinsic.as_entry(), num_params, ret, layout.class_id))
}

/// Could an invoke site naming `cp_class` reach the `jdk_class` `Atomic*`
/// intrinsic family at all? (review #80)
///
/// Yes for the JDK class itself. For any other class, only when
/// `name descriptor` is a method `final` in `jdk_class`: that is the one case
/// [`atomic_intrinsic_site_class_matches`] admits through a resolved declaring
/// class. The answer is the cheap pre-filter a registration site applies
/// BEFORE it asks the VM which class declares the resolved method. That lookup
/// takes the class-manager lock and walks the hierarchy, and a method named
/// `get()I` on some unrelated class must not pay it.
pub fn atomic_intrinsic_site_may_match(
    jdk_class: &str,
    cp_class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    cp_class == jdk_class || atomic_intrinsic_method_is_final_in_jdk(jdk_class, name, descriptor)
}

/// Is a call to `class.name descriptor` inside a callee body one the
/// `AtomicInteger` / `AtomicLong` ladders turn into a single `LOCK XADD` /
/// `LOCK CMPXCHG` when the CALLEE is compiled on its own?
///
/// A body spliced into its caller loses that: `try_emit_inline_body` lowers
/// every call of a spliced body as an ordinary call, and splicing the JDK's own
/// `getAndIncrement` instead copies `Unsafe.getAndAddInt`'s retry loop (a CAS
/// loop with a spliced-header poll and a chain snapshot per guard). Measured on
/// `new FastThreadLocal<Boolean>()` in a loop, whose constructor calls
/// `InternalThreadLocalMap.nextVariableIndex()`: 8-9 ns per iteration with the
/// callee kept as its own compile, 135-142 ns once `CRATONVM_JIT_IR_INLINE_COLD_ATHROW`
/// let `nextVariableIndex` (its `athrow` arm masked) be spliced into the loop.
/// `resolve_inline_site_from` therefore refuses a callee that contains such a
/// call, as it does for the other thin-bound families
/// (`thin-bound-native-keeps-its-own-fast-path`).
///
/// Only the JDK classes by name: a subclass site resolves its intrinsic through
/// the declaring class and a guard on the site's own class id, which a spliced
/// body has no table for either way. `CRATONVM_JIT_NO_ATOMIC_INTRINSIC` turns
/// the family off, and with it this refusal.
pub fn splice_loses_atomic_intrinsic(class: &str, name: &str, descriptor: &str) -> bool {
    !atomic_intrinsic_disabled()
        && matches!(
            class,
            "java/util/concurrent/atomic/AtomicInteger" | "java/util/concurrent/atomic/AtomicLong"
        )
        && atomic_intrinsic_method_is_final_in_jdk(class, name, descriptor)
}

/// The class name [`ir::try_ir_unbox_intrinsic`] should match an IR-tier
/// unbox site as (review #80).
///
/// The constant-pool class, except at a subclass site of `AtomicInteger` or
/// `AtomicLong` whose method resolves to the JDK class's `final` body
/// ([`atomic_intrinsic_site_class_matches`]): that site matches as the JDK
/// class. The guard and the layout still come from the SITE's class id, and
/// the lowered guard is an exact class-id compare, so a receiver of any other
/// class deopts. The declaring-class resolver is asked only when some JDK
/// class declares this method `final`.
pub(crate) fn ir_unbox_match_class<'a>(
    cp_class: &'a str,
    name: &str,
    descriptor: &str,
    cp_idx: u16,
    cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> &'a str {
    let Some(jdk) = [
        "java/util/concurrent/atomic/AtomicInteger",
        "java/util/concurrent/atomic/AtomicLong",
    ]
    .into_iter()
    .find(|&jdk| cp_class != jdk && atomic_intrinsic_method_is_final_in_jdk(jdk, name, descriptor)) else {
        return cp_class;
    };
    let declaring = cp_invoke_declaring_class_resolver.and_then(|resolve| resolve(cp_idx));
    if atomic_intrinsic_site_class_matches(jdk, cp_class, declaring.as_deref(), name, descriptor) {
        jdk
    } else {
        cp_class
    }
}

/// The ATOMIC_INT (`jdk_class` = `AtomicInteger`) or ATOMIC_LONG (`AtomicLong`)
/// registration for one `invokevirtual` site, as `try_compile_inner` performs
/// it: `(entry, num_params, return tag, guard class id)`, or `None` to keep
/// ordinary dispatch.
///
/// - `guard_class_id` comes from `cp_invoke_class_id_resolver`: the class the
///   constant pool names at the site. For `class Counter extends AtomicInteger`
///   that is `Counter`. The emitted guard is an exact header class-id compare,
///   so only receivers of exactly that class take the inline path, and any
///   other subclass falls back. The field layout (`AtomicIntFieldLayout::new(0,
///   guard_class_id)`) is resolved through that same class id, so a subclass's
///   own compact layout supplies the offset of `value`. The slot is 0 in every
///   subclass, because `compute_field_layout` in `classloading` places
///   superclass instance fields first.
/// - `cp_invoke_declaring_class_resolver` answers which class declares the
///   method the site resolves to. It is asked only for a non-JDK constant-pool
///   class calling a method that is `final` in the JDK class
///   ([`atomic_intrinsic_site_may_match`]), and its answer admits the site only
///   when it names the JDK class ([`atomic_intrinsic_site_class_matches`]).
pub(crate) fn atomic_intrinsic_for_invoke_site(
    jdk_class: &str,
    cp_idx: u16,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> Option<(usize, usize, u8, u32)> {
    if !atomic_intrinsic_site_may_match(jdk_class, class_name, method_name, descriptor) {
        return None;
    }
    let class_id_resolver = cp_invoke_class_id_resolver?;
    let guard_class_id = class_id_resolver(cp_idx)?;
    let declaring_class = if class_name == jdk_class {
        None
    } else {
        cp_invoke_declaring_class_resolver.and_then(|resolve| resolve(cp_idx))
    };
    match jdk_class {
        "java/util/concurrent/atomic/AtomicInteger" => try_resolve_atomic_intrinsic_for_site(
            class_name,
            declaring_class.as_deref(),
            method_name,
            descriptor,
            guard_class_id,
        ),
        "java/util/concurrent/atomic/AtomicLong" => try_resolve_atomic_long_intrinsic_for_site(
            class_name,
            declaring_class.as_deref(),
            method_name,
            descriptor,
            guard_class_id,
        ),
        _ => None,
    }
}

/// Call sites the single-pass backend has EMITTED as `AtomicLong` intrinsics
/// this process — counted at emission, not in the matcher (review #80); see
/// [`ATOMIC_INTRINSIC_SITES`].
///
/// The acceptance criterion, and not the ns/op: the 32-bit twin's own doc says
/// "a perf claim about this family is not believable without checking that this
/// is non-zero — the intrinsic answering the same values as the native it
/// replaced proves nothing about whether it actually ran."
/// `CRATONVM_DBG_ATOMIC_INTRINSIC=1` prints each site.
pub static ATOMIC_LONG_INTRINSIC_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC=1` — keep every `AtomicLong` RMW call
/// on ordinary native dispatch.
///
/// Separate from `CRATONVM_JIT_NO_ATOMIC_INTRINSIC` on purpose: the two
/// families are emitted by different code and a bisect that cannot tell them
/// apart is not a bisect. Default off.
pub(crate) fn atomic_long_intrinsic_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        false
    })
}

/// Matcher for the ATOMIC_LONG region — the 64-bit twin of
/// [`try_resolve_atomic_intrinsic`], with the same contract: returns the
/// intrinsic entry, the parameter count excluding the receiver, the return
/// type tag, and the receiver class id to guard on.
///
/// The guard is ALWAYS emitted for this family, for the same reason:
/// `AtomicLong` is not final, so a receiver could be a subclass that overrides
/// `incrementAndGet`, and only an exact class-id match may take the inline
/// path.
///
/// `num_params` is 1 for `getAndAdd(J)J` / `addAndGet(J)J` and not 2: this
/// JIT's operand stack is one 64-bit slot per value, not JVMS category-2
/// pairs — the same counting `LONG_VALUE_OF_DIRECT_FN`'s bind documents.
pub fn try_resolve_atomic_long_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    try_resolve_atomic_long_intrinsic_for_site(class, None, name, descriptor, guard_class_id)
}

/// The 64-bit twin of [`try_resolve_atomic_intrinsic_for_site`], on the same
/// terms: a subclass site matches only through a resolved declaring class of
/// `AtomicLong` and a method `final` there — which excludes `longValue()`.
pub fn try_resolve_atomic_long_intrinsic_for_site(
    class: &str,
    resolved_declaring_class: Option<&str>,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    if !atomic_intrinsic_site_class_matches(
        "java/util/concurrent/atomic/AtomicLong",
        class,
        resolved_declaring_class,
        name,
        descriptor,
    ) {
        return None;
    }
    if atomic_long_intrinsic_disabled() {
        return None;
    }
    // Derived from the SITE's declared class id and field slot 0 — the same
    // two inputs the codegen re-derives it from, and the same slot the
    // registered natives address (`get_field_volatile(this, 0)` /
    // `atomic_fetch_add_long(this, 0, ..)`). `AtomicLongFieldLayout::new`
    // returns `None` when the compact storage width is not exactly 8 bytes, so
    // a layout this 64-bit `LOCK XADD` could not address never reaches codegen.
    let layout = AtomicLongFieldLayout::new(0, guard_class_id)?;
    if layout.class_id == 0 {
        return None;
    }
    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
    let hit: Option<(JitIntrinsic, usize)> = match (name, descriptor) {
        ("get", "()J") => Some((JitIntrinsic::AtomicLongGet, 0)),
        ("getPlain", "()J") => Some((JitIntrinsic::AtomicLongGet, 0)),
        ("getAcquire", "()J") => Some((JitIntrinsic::AtomicLongGet, 0)),
        ("longValue", "()J") => Some((JitIntrinsic::AtomicLongGet, 0)),
        ("getAndIncrement", "()J") => Some((JitIntrinsic::AtomicLongGetAndIncrement, 0)),
        ("getAndDecrement", "()J") => Some((JitIntrinsic::AtomicLongGetAndDecrement, 0)),
        ("incrementAndGet", "()J") => Some((JitIntrinsic::AtomicLongIncrementAndGet, 0)),
        ("decrementAndGet", "()J") => Some((JitIntrinsic::AtomicLongDecrementAndGet, 0)),
        ("getAndAdd", "(J)J") => Some((JitIntrinsic::AtomicLongGetAndAdd, 1)),
        ("compareAndSet", "(JJ)Z") => Some((JitIntrinsic::AtomicLongCompareAndSet, 2)),
        ("weakCompareAndSet", "(JJ)Z") => Some((JitIntrinsic::AtomicLongCompareAndSet, 2)),
        ("addAndGet", "(J)J") => Some((JitIntrinsic::AtomicLongAddAndGet, 1)),
        _ => None,
    };
    // ===== INTRINSIC REGION END: ATOMIC_LONG =====
    let (intrinsic, num_params) = hit?;
    if false {
        eprintln!(
            "[atomic-long-intrinsic] {}.{}{} class_id={} compact_off={} legacy_off={}",
            class,
            name,
            descriptor,
            layout.class_id,
            layout.value_compact_offset,
            layout.value_legacy_offset,
        );
    }
    // See the `Z` note on the 32-bit twin above.
    let ret = if matches!(intrinsic, JitIntrinsic::AtomicLongCompareAndSet) {
        b'Z'
    } else {
        b'J'
    };
    Some((intrinsic.as_entry(), num_params, ret, layout.class_id))
}

/// Sites the single-pass backend has EMITTED as BOX_UNBOX intrinsics this
/// process, split by class — counted at emission, not in the matcher
/// (review #80); see [`ATOMIC_INTRINSIC_SITES`].
///
/// The acceptance criterion, and not the ns/op — the same rule the two
/// `Atomic*` counters beside this one state: "the intrinsic answering the same
/// values as the native it replaced proves nothing about whether it actually
/// ran." A boxing perf claim quoting a ns/op without a non-zero site count here
/// is quoting a number from a run where nothing was intrinsified.
pub static LONG_LONG_VALUE_INTRINSIC_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// See [`LONG_LONG_VALUE_INTRINSIC_SITES`].
pub static INTEGER_INT_VALUE_INTRINSIC_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `(Long.longValue, Integer.intValue)` sites emitted inline.
pub fn box_unbox_intrinsic_sites() -> (usize, usize) {
    (
        LONG_LONG_VALUE_INTRINSIC_SITES.load(std::sync::atomic::Ordering::Relaxed),
        INTEGER_INT_VALUE_INTRINSIC_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` — keep every `Long.longValue` /
/// `Integer.intValue` call on its thin direct bind (or, failing that, ordinary
/// native dispatch).
///
/// Separate from the two `Atomic*` switches for the reason they are separate
/// from each other: a bisect that cannot tell two families apart cannot
/// attribute a regression to either. It is also the A/B lever for this family
/// on ONE binary, which is the only kind of A/B this tree accepts for a perf
/// claim — a control built from a different commit has manufactured a
/// double-digit "regression" on phases containing neither call.
/// # It was DEFAULT-OFF for two days, and the crash was not its fault
///
/// `org.h2.test.store.TestRandomMapOps --Xmx 256m` with this family on died of
/// `SIGSEGV` at a fault address that was always a page boundary, `rdi` equal to
/// it and the fault pc inside libc -- 11/11 when the page was written, and 3/3
/// in 7 s once an unrelated wrong-answer defect stopped ending the run first.
/// Two switches each removed it: `CRATONVM_ZGC_RELOCATE=0` and this family off.
/// The page read that pair as "it takes BOTH relocation and this intrinsic",
/// and concluded the inline sequence held a receiver across a relocation.
///
/// **It did not.** A third switch removes it too, and names the mechanism:
/// `CRATONVM_GC_RESERVE=0`, 3/3 clean against a 3/3 positive control in the
/// same batch. gdb puts the fault in `ZgcRealHeap::compact_high_region`'s
/// `memmove`. The collector's relocation slides pick a destination inside FREE
/// space and copy into it without going through `Arena::hand_out`, and
/// `Arena::decommit_free_blocks` had already returned those granules to the OS
/// -- so the first slide after a give-back wrote into `PROT_NONE`. Both slides
/// now commit first (`Arena::commit_for_relocation`).
///
/// This family's part was to change the allocation shape enough to make the
/// high slide run. That is why turning it off hid the crash, and why turning it
/// off was never a fix. The three-way switch table is the lesson: a pair of
/// switches that each remove a crash does not identify a mechanism, it
/// identifies two ingredients -- and a third switch can turn out to be under
/// both of them.
///
/// Default ON again since 2026-09-04, with the fix, measured at zero SIGSEGV
/// over six runs of the workload that crashed 11/11.
/// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` still forces it off, so a script
/// that already sets it keeps working and keeps meaning the same thing.
pub(crate) fn box_unbox_intrinsic_disabled() -> bool {
    // Test-only force, consulted BEFORE the cache. The family was opt-in for
    // two days after it was found to SIGSEGV under relocation, so the matcher's
    // own tests --
    // which assert the POSITIVE case and say outright that every negative
    // below it is vacuous without it -- cannot reach it through the
    // environment: `OnceLock` fixes the answer at the first read, whichever
    // test in the binary got there first. This is the same shape
    // `ir_lower::ls_forced` uses, and it is thread-local so parallel tests
    // cannot see each other's setting.
    #[cfg(test)]
    {
        if let Some(forced) = box_unbox_forced() {
            return !forced;
        }
    }
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        if false {
            return true;
        }
        // DEFAULT ON AGAIN (2026-09-04). The mitigation this replaced existed
        // for exactly one reason -- the SIGSEGV recorded in `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`
        // -- and that crash was not this intrinsic's. It was
        // `ZgcRealHeap`'s relocation slides writing into arena granules
        // `Arena::decommit_free_blocks` had already returned to the OS; the
        // slides now commit their destination first
        // (`Arena::commit_for_relocation`). The intrinsic's part was to change
        // the allocation shape enough to make the high slide run, which is why
        // turning it off hid the crash and why turning it off was never a fix.
        //
        // Measured after that fix, on the workload that crashed 11/11 and then
        // 3/3 in 7 s: `CRATONVM_JIT=box-unbox-intrinsic`, SIX runs (three to a
        // 1200 s cap, three to 400 s), **zero SIGSEGV**. What those runs end on
        // instead -- a `NullPointerException` at a later seed -- appears
        // identically with the family OFF, and is the pre-existing failure
        // `known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829-RETIRED-20260909.md`
        // records.
        //
        // `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` still forces it off and still
        // means the same thing, so any script that sets it is unaffected.
        false
    })
}

pub fn try_resolve_box_unbox_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    if box_unbox_intrinsic_disabled() {
        return None;
    }
    box_unbox_intrinsic_shape(class, name, descriptor, guard_class_id)
}

/// Which triple maps to which intrinsic, and with what operand width —
/// WITHOUT the enable gate.
///
/// Split out on 2026-09-02, when flipping the family's default to opt-in turned
/// three matcher tests red: they assert that `Long.longValue()J` matches and
/// that a dozen near-miss triples do not, and with the gate inside the matcher
/// every one of those answers became `None`, so the positive case failed and
/// every negative became vacuous.
///
/// Whether the family is ENABLED and whether a triple is one of the two it
/// serves are separate questions, and only the second is what those tests are
/// about. Keeping them separate meant the tests went on guarding the match
/// across the default's two flips — off on 2026-09-02 for a crash that was the
/// collector's, and on again on 2026-09-04 once
/// `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`
/// closed it.
pub(crate) fn box_unbox_intrinsic_shape(
    class: &str,
    name: &str,
    descriptor: &str,
    guard_class_id: u32,
) -> Option<(usize, usize, u8, u32)> {
    // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
    match (class, name, descriptor) {
        ("java/lang/Long", "longValue", "()J") => {
            let layout = AtomicLongFieldLayout::new(0, guard_class_id)?;
            if layout.class_id == 0 {
                return None;
            }
            if false {
                eprintln!(
                    "[box-unbox-intrinsic] java/lang/Long.longValue()J class_id={} compact_off={} legacy_off={}",
                    layout.class_id, layout.value_compact_offset, layout.value_legacy_offset,
                );
            }
            Some((
                JitIntrinsic::LongLongValue.as_entry(),
                0,
                b'J',
                layout.class_id,
            ))
        }
        ("java/lang/Integer", "intValue", "()I") => {
            let layout = AtomicIntFieldLayout::new(0, guard_class_id)?;
            if layout.class_id == 0 {
                return None;
            }
            if false {
                eprintln!(
                    "[box-unbox-intrinsic] java/lang/Integer.intValue()I class_id={} compact_off={} legacy_off={}",
                    layout.class_id, layout.value_compact_offset, layout.value_legacy_offset,
                );
            }
            Some((
                JitIntrinsic::IntegerIntValue.as_entry(),
                0,
                b'I',
                layout.class_id,
            ))
        }
        _ => None,
    }
    // ===== INTRINSIC REGION END: BOX_UNBOX =====
}



/// Speculate a `java/lang/String` receiver at a `java/lang/CharSequence`-declared
/// `length`/`charAt`/`isEmpty` call site WITHOUT positive receiver evidence
/// (`CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC`, **default-OFF as of 2026-08-24;
/// `=1` restores the old unconditional speculation**). Read-once cached.
///
/// A `java/lang/String` site is `final` and needs no guard, so this knob does
/// not touch it, and neither does any of the receiver-de-spec machinery. A
/// `CharSequence` site is different in kind: the emitted inline body decodes the
/// String layout, so it is valid only behind an exact class-id compare, and the
/// MISS EDGE IS A DEOPT -- not a fall-through to dispatch. That makes the
/// speculation unusually asymmetric, and the asymmetry is measured
/// (`probes/CharSeqStringIntrinsicProbe.java`, one binary, four interleaved
/// readings per arm, Azure host `vm1`):
///
/// | | guard on | guard off |
/// |---|---:|---:|
/// | `length()` on a `String` | 7.85-8.37 ns | 28.50-31.36 ns |
/// | `length()` on a `StringBuilder` | 416.82-460.71 ns | 282.56-320.55 ns |
/// | `charAt()` on a `String` | 9.04-10.26 ns | 53.83-66.35 ns |
/// | `charAt()` on a `StringBuilder` | 652.36-716.12 ns | 310.99-399.96 ns |
///
/// A hit is worth 22 ns (`length`) / 47 ns (`charAt`); a miss costs 140 ns /
/// 320 ns, plus a whole-method `MakeNotCompilable` once the deopts add up. The
/// break-even is therefore ~87% `String` receivers, not 50% -- which is where
/// [`MIN_GUARDED_RECEIVER_PCT`] comes from -- and until 2026-08-24 the compiler
/// took the bet with NO evidence at all, because `CRATONVM_TIER_PGO` is
/// default-OFF so there is no receiver profile to read on an ordinary run.
///
/// On netty's `HttpHeaderValidationUtilTest`, whose `CharSequence.length()` is
/// reached only with `AsciiString` and an anonymous `CharSequence`, refusing the
/// unevidenced guard is worth **6.5x on the value loop and 3.8x on the name
/// loop** (`probes/io/netty/.../HeaderValidationLoopRate.java`, ABBA, one
/// binary), and takes the run's deopt census from 263 502 to **18** with the
/// `UnreachedCode` traps at 85 193 -> **0**.
pub fn charseq_blind_guard_enabled() -> bool {
    // The hand-rolled table this replaced (unset and empty off, `0|false|off|no`
    // off, anything else on) is `runtime_flag_on`'s. No `static` (j27 LATCHES).
    false
}

/// Minimum share of a call site's recorded receivers that must be the class a
/// speculative receiver guard is about, before the guard is worth emitting.
///
/// A receiver-guarded call-site intrinsic does not FALL BACK on a miss -- it
/// DEOPTS, at roughly a microsecond a time. So the break-even is nowhere near
/// 50%: a site that is 60% the guarded class still pays 0.4 deopts per call and
/// is far worse than the ordinary dispatch it replaced. 90 is deliberately
/// conservative about the other direction too -- a site the profile says is
/// overwhelmingly `String` keeps the intrinsic exactly as before.
pub const MIN_GUARDED_RECEIVER_PCT: u32 = 90;

/// Minimum recorded receivers at a call site before [`MIN_GUARDED_RECEIVER_PCT`]
/// is allowed to decide anything. Below this the profile is noise, and refusing
/// on it would turn a cold-start artifact into a permanently un-intrinsified
/// one.
pub const MIN_GUARDED_RECEIVER_EVIDENCE: u32 = 32;

/// `true` when `profile` has enough evidence at `pc` to say that a receiver
/// guard on `class_id` would hold at least [`MIN_GUARDED_RECEIVER_PCT`] of the
/// time -- the POSITIVE evidence a trap-on-miss guard needs before it is worth
/// emitting. Fail-CLOSED: no profile, no entry, too few samples -> `false`.
///
/// This is the counterpart of [`receiver_profile_rejects_guard`], and the two
/// are deliberately not each other's negation: "the profile says no" and "the
/// profile says nothing" are different answers, and only one of them may leave
/// an unevidenced trap-on-miss guard standing (see `charseq_blind_guard_enabled`
/// for the measurement that settles which).
pub fn receiver_profile_supports_guard(
    profile: Option<&profile::MethodProfile>,
    pc: usize,
    class_id: u32,
) -> bool {
    let Some(counts) = profile.and_then(|p| p.receivers.get(&pc)) else {
        return false;
    };
    // The trap-on-miss guard policy of `profile::ReceiverEvidence` (tier
    // proposal R12-5): `Silent` below the floor, shares in `u64`.
    profile::ReceiverEvidence::of(counts).guard_evidence(
        class_id,
        MIN_GUARDED_RECEIVER_EVIDENCE,
        MIN_GUARDED_RECEIVER_PCT,
    ) == profile::GuardEvidence::Supports
}

/// `true` when `profile` has enough evidence at `pc` to say that a receiver
/// guard on `class_id` would fail most of the time.
///
/// This is the COMPILE-TIME half of receiver de-speculation, and the half that
/// costs nothing: the per-bci de-spec registry can only act after a site has
/// already deopted `PER_BCI_DESPEC_LIMIT` times and been recompiled, whereas
/// the interpreter has usually already recorded thousands of receivers at the
/// same bci before the method is compiled at all. `bytecode_walk.rs`'s own
/// header comment describes exactly this trap for a different guard, and moves
/// the screen to compile time to avoid it.
///
/// Fail-open in every uncertain case -- no profile, no entry for this pc, too
/// few samples -- so a site with no evidence speculates exactly as it did.
pub fn receiver_profile_rejects_guard(
    profile: Option<&profile::MethodProfile>,
    pc: usize,
    class_id: u32,
) -> bool {
    let Some(counts) = profile.and_then(|p| p.receivers.get(&pc)) else {
        return false;
    };
    profile::ReceiverEvidence::of(counts).guard_evidence(
        class_id,
        MIN_GUARDED_RECEIVER_EVIDENCE,
        MIN_GUARDED_RECEIVER_PCT,
    ) == profile::GuardEvidence::Rejects
}

/// `CRATONVM_JIT_OBJECT_HASHCODE_STRING` (default ON; `0` = off): let an
/// `Object.hashCode()I` / `CharSequence.hashCode()I` site whose receiver
/// profile is String-dominant take the inline `String.hashCode` intrinsic
/// behind a String class-id guard. See [`object_hashcode_string_guard`].
pub fn object_hashcode_string_guard_enabled() -> bool {
    true
}

/// Sites registered by [`object_hashcode_string_guard`] (single-pass door), so
/// "did this land" is answerable without a timing run.
pub static OBJECT_HASHCODE_STRING_GUARD_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The guard class id under which an `Object.hashCode()I` (or
/// `CharSequence.hashCode()I`) call site may be served by the inline
/// `JitIntrinsic::StringHashCode` body, or `None` (round 9 wave 3,
/// `object-hashcode-on-a-string-receiver-pays-the-mic-native-helper-20260918.md`).
///
/// `String.hashCode` is a VM native here, so such a site -- `map.get(k).hashCode()`,
/// every `Objects.hashCode`/`HashMap.hash`-shaped caller -- reached the MIC
/// helper's site-cached native path on EVERY call (~435 cycles; the inline
/// cascade never hits because a native is never published into the slot),
/// while the identical call through a `String`-typed site reads the cached
/// `hash` field inline.
///
/// `String` overrides `hashCode`, so for a receiver whose class id IS
/// `java/lang/String` the intrinsic computes exactly what virtual dispatch
/// would. The emitter's STRING_ACCESS arm already guards a non-zero
/// `guard_class_id` (`CMP [recv+0], id; JNE deopt`) -- but that miss is a
/// DEOPT, and `Object.hashCode` sites are overwhelmingly NOT String-typed, so
/// this is fail-CLOSED on POSITIVE evidence only: the receiver profile must
/// show at least [`MIN_GUARDED_RECEIVER_PCT`] String receivers over at least
/// [`MIN_GUARDED_RECEIVER_EVIDENCE`] samples. No blind mode, whatever
/// `CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC` or `CRATONVM_JIT_RECEIVER_DESPEC`
/// say; the caller additionally honours the per-bci de-spec registry.
///
/// Deliberately NOT part of [`try_resolve_string_intrinsic`]: that function is
/// also the "does this method contain a String-intrinsic site" predicate for
/// scan admission and for the optimizing tier's hand-back-to-single-pass rule,
/// and every `Object.hashCode()` site in the program would match it.
pub fn object_hashcode_string_guard(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    string_layout: Option<StringFieldLayout>,
    profile: Option<&profile::MethodProfile>,
    pc: usize,
) -> Option<u32> {
    if method_name != "hashCode" || descriptor != "()I" {
        return None;
    }
    if class_name != "java/lang/Object" && class_name != "java/lang/CharSequence" {
        return None;
    }
    if !object_hashcode_string_guard_enabled() {
        return None;
    }
    let layout = string_layout?;
    if !layout.has_coder || layout.string_class_id == 0 {
        return None;
    }
    receiver_profile_supports_guard(profile, pc, layout.string_class_id)
        .then_some(layout.string_class_id)
}

/// Does this String-intrinsic entry DECLINE into an ordinary native dispatch
/// rather than into an uncommon trap?
///
/// True for the `java/lang/StringBuilder` members and false for every
/// String/CharSequence one -- except `String.codePointAt` (JIT r23 wave 1,
/// lane L5), the one unguarded String member whose misses are calls -- and
/// both halves of that answer are load-bearing at every compile door:
///
/// * **It needs a `JitInvokeInfo`.** A site the resolver registers as an
///   intrinsic never reaches the generic `invoke_info.push` — every such arm
///   `continue`s above it — so a declining intrinsic must register one of its
///   own, exactly as the FFM region does. Without it the emitter cannot emit
///   the decline edge, refuses the site, and the refusal fails the whole
///   method: measured at 404 696 interpreted dispatches against 8 695, with
///   `StringBuilder.append(char)` at 1 091 ns against 309 ns, because every
///   method containing one dropped to the interpreter.
/// * **The receiver de-spec screen does not apply to it.** That screen exists
///   because a `CharSequence` site's class-id guard DEOPTS on a miss, so a bci
///   the profile says is rarely a `String` must not get one. A guard whose miss
///   is a predicted branch and a call has nothing to protect against — and a
///   `StringBuilder` guard cannot miss anyway, the class being final.
///
/// Asked by BOTH doors rather than hand-copied into each, because the record of
/// this area is that a door grows its own subset and the two drift — see
/// `compile_gate`'s module doc, and the fact that the method-entry door was
/// fixed first here and the OSR door still failed every method.
pub fn string_intrinsic_declines_to_a_call(entry: usize) -> bool {
    entry == JitIntrinsic::StringBuilderLength.as_entry()
        || entry == JitIntrinsic::StringBuilderAppendChar.as_entry()
        || entry == JitIntrinsic::StringBuilderCharAt.as_entry()
        || entry == JitIntrinsic::StringBuilderAppendString.as_entry()
        || entry == JitIntrinsic::StringBuilderAppendInt.as_entry()
        || entry == JitIntrinsic::StringBuilderToString.as_entry()
        // JIT r23 wave 1, lane L5 (INTRINSICS22-1): both code-point rows
        // decline to the call (with `codePointBefore` / `codePointCount`
        // below, the only `java/lang/String` members that do).
        || entry == JitIntrinsic::StringBuilderAppendCodePoint.as_entry()
        || entry == JitIntrinsic::StringCodePointAt.as_entry()
        // JIT r23 wave 2, lane L5.
        || entry == JitIntrinsic::StringBuilderCodePointAt.as_entry()
        || entry == JitIntrinsic::StringCodePointBefore.as_entry()
        || entry == JitIntrinsic::StringCodePointCount.as_entry()
}

/// `CRATONVM_JIT_SP_SB_CODE_POINT_AT` (JIT r23 wave 2, lane L5, R23W1L5-3;
/// perf, default ON, `=0` off): register `StringBuilder.codePointAt(int)` as
/// the STRINGBUILDER_ACCESS call-site intrinsic
/// [`JitIntrinsic::StringBuilderCodePointAt`] (`x64/string_code_point.rs`),
/// declining to the call on any miss. A pin candidate, released by the
/// `SbCodePointAt` prefix kind. Compile time only.
pub fn sp_sb_code_point_at_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_STRING_CODE_POINT_BEFORE_COUNT` (JIT r23 wave 2, lane L5,
/// R23W1L5-2; perf, default ON, `=0` off): register `String.codePointBefore(int)`
/// and `codePointCount(int, int)` as declining STRING_ACCESS call-site
/// intrinsics ([`JitIntrinsic::StringCodePointBefore`] /
/// [`JitIntrinsic::StringCodePointCount`]) for the methods the single-pass
/// backend keeps. Pin candidates, released by the `CodePointBefore` /
/// `CodePointCount` prefix kinds. Compile time only.
pub fn sp_string_code_point_before_count_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_STRING_CODE_POINT_AT` (JIT r23 wave 1, lane L5,
/// INTRINSICS22-1 / STRINGS21-3's single-pass half; perf, default ON, `=0`
/// off): register `String.codePointAt(int)` as the STRING_ACCESS call-site
/// intrinsic [`JitIntrinsic::StringCodePointAt`], so a method the single-pass
/// backend keeps (a STRINGBUILDER_ACCESS site pins it, or the optimizing tier
/// refused it) answers the code point inline (`x64/string_code_point.rs`)
/// instead of the native funnel (~200 ns). Every miss declines to the site's
/// ordinary dispatch, which throws exactly what the call throws.
///
/// The row is also a pin candidate (`has_string_intrinsic_site`): the
/// optimizing tier's `CodePointAt` prefix kind
/// (`CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX`, default ON) names the same
/// site in `every_string_intrinsic_site_ir_expands`, so a method is pinned
/// for it only when that prefix is off (where the planning gate already bails
/// it to this tier). Read at compile time, not cached.
pub fn sp_string_code_point_at_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_SB_APPEND_CODE_POINT` (JIT r23 wave 1, lane L5,
/// INTRINSICS22-1; perf, default ON, `=0` off): register
/// `StringBuilder.appendCodePoint(int)` as the STRINGBUILDER_ACCESS call-site
/// intrinsic [`JitIntrinsic::StringBuilderAppendCodePoint`] (the
/// `SbAppendCodePoint` prefix body in the single-pass tier), declining to the
/// call on a growth, an inflation or an invalid code point. A pin candidate
/// like its siblings, released by the `SbAppendCodePoint` prefix kind
/// (`CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_PREFIX`, default ON). Read at
/// compile time, not cached.
pub fn sp_sb_append_code_point_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SB_TOSTRING_INLINE` (JIT round 17 wave 1, lane strings,
/// STR16-P1; default ON since strings round 2, `str17`, 2026-10-06, `=0`
/// restores the call): register `StringBuilder.toString()` as a
/// STRINGBUILDER_ACCESS call-site intrinsic, so the single-pass backend builds
/// the LATIN1 answer inline (`x64/op_invoke.rs`,
/// `emit_sb_to_string_latin1_body`) and declines everything else into the
/// ordinary call -- the JDK body under `--real-jdk`.
///
/// Read at compile time, once per resolved site (registration and the
/// devirtualisation / pin predicates all ask the resolver); never on a run-time
/// path, and not cached, like the other compile-time levers beside it. Like
/// the four other builder members, a method containing such a site is pinned to
/// the single-pass backend (`has_string_intrinsic_site`), so the A/B prices
/// that too.
///
/// Round 17 kept it OFF on wall medians taken while a refused C2 publication
/// made one run in five 2.5x slower (`r16-strings-kernel-residual-after-str17`,
/// term 1). Re-measured on the fixed build, CPU and wall, seven interleaved runs:
/// `R16Bench strings` 335-588 -> 227-393 ms at ZGC and 366-603 -> 241-410 at
/// Generational, `R16StringsSplit` `toStr` 56-93 -> 23-40 ns.
pub fn jit_sb_to_string_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SB_TOSTRING_UTF16` (JIT round 17 wave 2, lane sbzgc; default
/// OFF): the inline `toString()` (which needs
/// `CRATONVM_JIT_SB_TOSTRING_INLINE` to be registered at all) also serves a
/// UTF16 builder holding at least one char above `0xFF`, as a fresh
/// `byte[2 * count]` with `coder = UTF16`. An all-LATIN1 UTF16 builder still
/// declines (the JDK may compress it). Read at compile time, once per emitted
/// site; the registration gate does not read it.
pub fn jit_sb_to_string_utf16_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_SB_TOSTRING_COMPRESS` (JIT round 17 wave 6, lane sbzgc2,
/// SBZGC17-2's second half; default OFF, and only meaningful on top of
/// `CRATONVM_JIT_SB_TOSTRING_UTF16`): the UTF16 arm answers a builder whose
/// `maybeLatin1` is set and whose scan found no char above `0xFF` as the JDK
/// constructor does under `COMPACT_STRINGS` -- a fresh LATIN1 String of
/// `count` bytes, narrowed from the UTF16 payload -- instead of declining.
/// Needs the builder layout's `maybeLatin1` slot
/// ([`StringBuilderFieldLayout::with_maybe_latin1`]); without it the switch
/// changes nothing. Read at compile time, once per emitted site.
pub fn jit_sb_to_string_compress_enabled() -> bool {
    false
}


pub fn try_resolve_string_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    string_layout: Option<StringFieldLayout>,
) -> Option<(usize, usize, u8, u32)> {
    // Receiver-guard mode by the *declared* class of the call site:
    //   * java/lang/String      — final, monomorphic → no guard (0).
    //   * java/lang/CharSequence — the receiver may be any CharSequence, so
    //     the String-layout decode is only valid behind a runtime class-id
    //     guard against the real String class id.
    // Hole 1 of `gc/src/compressed_oops.rs`'s "two correctness holes" used to
    // be closed here, by refusing every String intrinsic under narrow oops:
    // all of them reach `emit_load_string_value_ptr` (`jit/src/x64/objects.rs`),
    // which emitted an unconditional 64-bit load of the `String.value`
    // reference field and so, under narrow oops, loaded 4 bytes of narrow oop
    // plus 4 bytes of the adjacent coder/hash field and dereferenced the
    // result.
    //
    // That emitter now has a proper narrow arm (`emit_load_narrow_ref_field`),
    // selected per call site by `StringFieldLayout::value_compact_is_narrow`,
    // so the refusal that used to stand here — and the throughput it cost —
    // is gone. The intrinsics are admitted under both widths.
    //
    // Hole 2 — the conservative 8-byte-word rescan in `gen_heap`'s
    // `mark_young_to_old_refs` / `rewrite_stretch_conservatively` — is closed
    // too (both go through `for_each_conservative_ref_slot`, which visits
    // narrow slots at 4 bytes as well). The gate still defaults off; see
    // `gc/src/compressed_oops.rs` for why, which is no longer "a known
    // wrong-width slot access on this backend".
    // ===== INTRINSIC REGION BEGIN: STRINGBUILDER_ACCESS =====
    // Answered before the String/CharSequence screen below, because a
    // `java/lang/StringBuilder` site is neither and would be refused by it.
    //
    // The guard is ALWAYS the StringBuilder class id, never 0: `StringBuilder`
    // is final, so the compare is exact, and it is what keeps a `StringBuffer`
    // receiver — `synchronized` methods, a `toStringCache` to invalidate — off
    // a path that emits neither obligation. `StringBuilderFieldLayout::new`
    // refuses a zero class id, so a layout that exists has a usable guard.
    if class == "java/lang/StringBuilder" {
        if let Some(b) = string_layout.and_then(|l| l.builder) {
            let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
                ("length", "()I") => Some((JitIntrinsic::StringBuilderLength, 0, b'I')),
                ("append", "(C)Ljava/lang/StringBuilder;") => {
                    Some((JitIntrinsic::StringBuilderAppendChar, 1, b'L'))
                }
                ("charAt", "(I)C") => Some((JitIntrinsic::StringBuilderCharAt, 1, b'C')),
                // The argument is `java/lang/String`, read through the
                // OUTER layout (`string_layout`, not `b`) — a different
                // class's fields at a different offset. Gated on
                // `has_coder` because the copy body branches on the
                // source's coder value; without it there is nothing to
                // branch on and the site stays on ordinary dispatch, same
                // as every other coder-dependent String member below.
                ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;")
                    if string_layout.is_some_and(|l| l.has_coder) =>
                {
                    Some((JitIntrinsic::StringBuilderAppendString, 1, b'L'))
                }
                ("append", "(I)Ljava/lang/StringBuilder;") => {
                    Some((JitIntrinsic::StringBuilderAppendInt, 1, b'L'))
                }
                // JIT round 17 wave 1, lane strings (STR16-P1,
                // `CRATONVM_JIT_SB_TOSTRING_INLINE`, default on): the inline
                // allocation builds a `java/lang/String` through the OUTER
                // layout, so it needs that layout's `coder` too.
                ("toString", "()Ljava/lang/String;")
                    if string_layout.is_some_and(|l| l.has_coder)
                        && jit_sb_to_string_inline_enabled() =>
                {
                    Some((JitIntrinsic::StringBuilderToString, 0, b'L'))
                }
                // JIT r23 wave 1, lane L5 (INTRINSICS22-1,
                // `CRATONVM_JIT_SP_SB_APPEND_CODE_POINT`, default ON).
                // Reads only the builder's own fields.
                ("appendCodePoint", "(I)Ljava/lang/StringBuilder;")
                    if sp_sb_append_code_point_enabled() =>
                {
                    Some((JitIntrinsic::StringBuilderAppendCodePoint, 1, b'L'))
                }
                // JIT r23 wave 2, lane L5 (R23W1L5-3,
                // `CRATONVM_JIT_SP_SB_CODE_POINT_AT`, default ON).
                ("codePointAt", "(I)I") if sp_sb_code_point_at_enabled() => {
                    Some((JitIntrinsic::StringBuilderCodePointAt, 1, b'I'))
                }
                _ => None,
            };
            if let Some((intrinsic, num_params, ret)) = hit {
                return Some((intrinsic.as_entry(), num_params, ret, b.class_id));
            }
        }
        return None;
    }
    // ===== INTRINSIC REGION END: STRINGBUILDER_ACCESS =====

    let is_string = class == "java/lang/String";
    let is_charseq = class == "java/lang/CharSequence";
    if !is_string && !is_charseq {
        return None;
    }
    // Every inlined String intrinsic reads the `coder` byte to pick the
    // LATIN1/UTF16 decode path. A layout without `coder` (legacy `char[]`
    // String) cannot be inlined — bail to native dispatch.
    let layout = string_layout?;
    if !layout.has_coder {
        return None;
    }
    let guard: u32 = if is_string { 0 } else { layout.string_class_id };
    // A CharSequence site needs a real String class id; without one the
    // inline decode would be unguarded — bail to native dispatch.
    //
    // WHETHER a guarded site is worth taking is decided by the CALLERS that
    // have a receiver profile (the method-entry door in `try_compile_inner`
    // and the OSR door in `vm/.../jit_bridge.rs`), not here: this function is
    // also the "does this method contain a String-intrinsic site" predicate for
    // scan admission and for the registration path, and those must keep seeing
    // the site. See `charseq_blind_guard_enabled`.
    if is_charseq && guard == 0 {
        return None;
    }

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // `charAt`/`length`/`isEmpty` are declared on CharSequence and so are
    // eligible for both receiver kinds; `hashCode` is String-only.
    let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
        ("length", "()I") => Some((JitIntrinsic::StringLength, 0, b'I')),
        ("isEmpty", "()Z") => Some((JitIntrinsic::StringIsEmpty, 0, b'Z')),
        ("charAt", "(I)C") => Some((JitIntrinsic::StringCharAt, 1, b'C')),
        ("hashCode", "()I") if is_string => Some((JitIntrinsic::StringHashCode, 0, b'I')),
        // JIT r23 wave 1, lane L5 (INTRINSICS22-1,
        // `CRATONVM_JIT_SP_STRING_CODE_POINT_AT`, default ON): declared on
        // `String` only (never a guarded `CharSequence` site), and a member
        // that DECLINES to the call (`string_intrinsic_declines_to_a_call`).
        ("codePointAt", "(I)I") if is_string && sp_string_code_point_at_enabled() => {
            Some((JitIntrinsic::StringCodePointAt, 1, b'I'))
        }
        // JIT r23 wave 2, lane L5 (R23W1L5-2,
        // `CRATONVM_JIT_SP_STRING_CODE_POINT_BEFORE_COUNT`, default ON).
        ("codePointBefore", "(I)I") if is_string && sp_string_code_point_before_count_enabled() => {
            Some((JitIntrinsic::StringCodePointBefore, 1, b'I'))
        }
        ("codePointCount", "(II)I") if is_string && sp_string_code_point_before_count_enabled() => {
            Some((JitIntrinsic::StringCodePointCount, 2, b'I'))
        }
        _ => None,
    };
    if let Some((intrinsic, num_params, ret)) = hit {
        return Some((intrinsic.as_entry(), num_params, ret, guard));
    }
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // `equals`, `compareTo` and `indexOf(String)` are inlined. The codegen
    // ladder decodes every character through the receiver's / argument's own
    // `coder` byte, so all LATIN1/UTF16 combinations are handled inline. These
    // signatures are declared on `java/lang/String` (not CharSequence), so they
    // never reach a guarded (CharSequence) call site.
    //
    //   * equals(Object)      — the argument is NOT assumed to be a String.
    //     The emitted body (`x64/bytecode_walk.rs`) tests the argument's
    //     ObjectHeader class id against the receiver's and routes a mismatch
    //     to the deopt stub, which is what makes it agree with
    //     `String.equals`'s `anObject instanceof String` guard: String is
    //     final, so equal class ids IS the instanceof. That test is
    //     load-bearing, not an optimisation — everything below it reaches the
    //     argument's `value` array by SLOT INDEX, and CratonVM's synthetic
    //     `StringBuilder` also has `char[] value` at slot 0. Measured on
    //     OpenJDK 25.0.3+9: `"abc".equals(new StringBuilder(3).append("abc"))`
    //     is `false` (the builder really does have capacity 3, so even the
    //     length matches) while `contentEquals` is `true`. See E18-1 §3, which
    //     fixed the same hole on the native side; this door already had the
    //     class-id compare, so both tiers now answer `false`.
    //   * compareTo(String)   — lexicographic decoded-char compare; the
    //     unsigned-char difference at the first mismatch, else len1-len2.
    //   * indexOf(String)     — naive O(n*m) substring search from 0; an
    //     empty needle returns 0.
    //
    // `indexOf(I)` is recognised only for a constant BMP needle — see below.
    if is_string {
        let search_hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("equals", "(Ljava/lang/Object;)Z") => Some((JitIntrinsic::StringEquals, 1, b'Z')),
            ("compareTo", "(Ljava/lang/String;)I") => {
                Some((JitIntrinsic::StringCompareTo, 1, b'I'))
            }
            // `indexOf(I)` — intrinsified again as of E27-1 N2b (2026-08-18),
            // but ONLY where the backend can prove the needle is a
            // compile-time constant in `0..=0xFFFF`. That screen lives in
            // `x64/bytecode_walk.rs` (`prev_insn_int_const`), not here: this
            // function sees a name and a descriptor, never an operand.
            //
            // Why the range is the whole question. The inline body scans for
            // ONE UTF-16 code unit. The JDK does not narrow `ch` — it gates on
            // `Character.isValidCodePoint` FIRST, then scans for one code unit
            // if `ch <= 0xFFFF` and for the SURROGATE PAIR if `ch >= 0x10000`.
            // Measured on OpenJDK 25.0.3+9:
            //
            //     "abc".indexOf(0x10061)   -1     masking finds 'a' at 0
            //     "￿q".indexOf(-1)    -1     masking finds U+FFFF at 0
            //     mixed.indexOf(0x10437)    3     the pair, not its low half at 1
            //
            // The `indexOf(-1)` row rejects the plausible wrong fix: `(char) -1`
            // IS `0xFFFF` and the receiver DOES hold `0xFFFF`, yet HotSpot
            // answers -1 — so the rule is the validity gate, not a narrowing
            // cast. Inside `0..=0xFFFF` all three rows are vacuous and the
            // single-code-unit scan IS `code_point_needle`'s answer, including
            // for a lone surrogate.
            //
            // The retirement this replaces was right to remove the unscreened
            // form: E18-1 had reduced the rule to ONE predicate in
            // `native-builtins`, and `jit/src/lib.rs` is below that crate in
            // the graph and cannot call it. A screen that admits only the
            // range where no rule is needed is not a fifth copy of it.
            //
            // A declined site is not deoptimised — it is simply not
            // intrinsified, and dispatches exactly as it does today. The
            // runtime-screen-plus-deopt design the first draft of N2b proposed
            // would have invalidated the enclosing compiled METHOD on every
            // out-of-range needle and then barred it from compilation; see
            // `prev_insn_int_const` and the page's N2b section.
            ("indexOf", "(I)I") => Some((JitIntrinsic::StringIndexOfChar, 1, b'I')),
            ("indexOf", "(Ljava/lang/String;)I") => Some((JitIntrinsic::StringIndexOfStr, 1, b'I')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = search_hit {
            return Some((intrinsic.as_entry(), num_params, ret, 0));
        }
    }
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    None
}

// A resolved direct-call target.
// `Clone` so one resolved single-pass plan can be handed to the backend
// more than once — see `SinglePassTables`. Every field is a plain `Copy`
// scalar, so the clone is a memcpy and carries no ownership.
#[derive(Clone)]
pub struct JitDirectCall {
    pub entry: usize,
    pub needs_context: bool,
    pub num_params: usize,
    pub return_type: u8,
    /// Receiver class id for a call-site intrinsic that needs a runtime
    /// receiver-class guard before its inline body is correct.
    ///
    /// The CRC32/CRC32C `update` intrinsics are `invokevirtual` sites: the
    /// inline code reads/writes the receiver's `int crc` field at slot 0,
    /// which is only sound if the receiver's *dynamic* class is exactly the
    /// declared `java/util/zip/CRC32` / `CRC32C` (a subclass could override
    /// `update`). The codegen emits `CMP [receiver+0], guard_class_id` and
    /// deopts to normal dispatch on mismatch.
    ///
    /// `0` means "no guard class id available" — the sentinel class id 0 is
    /// never a real CRC32/CRC32C class, so a CRC32 intrinsic whose
    /// `guard_class_id` is 0 bails to normal dispatch. Every non-intrinsic
    /// `JitDirectCall` leaves this 0 (unused).
    pub guard_class_id: u32,
}

/// Bit 0 of every inline-cache entry word ([`JitMICSlot::cached_entry_word`],
/// [`JitPICSlot::entry_words`] and the PIC's hashed table): set when the cached
/// target takes the hidden VM context pointer as its first argument.
///
/// The target and its calling convention used to live in two fields that
/// generated code loaded separately, so a reader could load the address of one
/// publication and the flag of the next and call a body with the other body's
/// ABI — or pass a non-zero check on the address and then load a zero. Folded
/// into one word they are one load. Every entry that can be published is the
/// start of an executable buffer (page-aligned), so bit 0 is always free, and
/// `jit_entry_publishable` refuses any address for which it is not.
pub const JIT_IC_NEEDS_CONTEXT_TAG: u64 = 1;

/// Encode `(entry, needs_context)` as an inline-cache entry word. A zero entry
/// is the "no target" word whatever the flag says.
#[inline]
pub(crate) fn jit_ic_entry_word(entry: u64, needs_context: bool) -> u64 {
    if entry == 0 {
        0
    } else {
        entry | u64::from(needs_context)
    }
}

/// Decode an inline-cache entry word into `(entry, needs_context)`.
#[inline]
pub(crate) fn jit_ic_entry_decode(word: u64) -> (u64, bool) {
    (
        word & !JIT_IC_NEEDS_CONTEXT_TAG,
        word & JIT_IC_NEEDS_CONTEXT_TAG != 0,
    )
}

/// Whether `owner` names a body an invalidation has already withdrawn
/// ([`CompiledMethod::retired`]).
#[inline]
pub(crate) fn owner_is_retired(owner: &Option<Arc<CompiledMethod>>) -> bool {
    owner
        .as_ref()
        .is_some_and(|o| o.retired.load(std::sync::atomic::Ordering::SeqCst))
}

/// [`JitDirectCall::guard_class_id`] value that marks a row as a
/// [`JitCycleEdgeCell`] site rather than a direct call (round 9 wave 7, lane
/// `mutrec7`,
/// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch-20260918.md`).
///
/// Such a row's `entry` is the CELL's address, not a callable entry, so the
/// single-pass `invokestatic` arm (`x64/op_invoke.rs`) takes it out of the
/// ordinary direct-call ladder before anything can `CALL` it. Only ever planned
/// on an `invokestatic` (0xb8) site, where `guard_class_id` has no other
/// meaning (the receiver guard is a virtual-intrinsic concept), so no real
/// class id is ever compared against it.
pub const JIT_CYCLE_EDGE_CELL_GUARD: u32 = 0xFFFF_FFF0;

/// Byte offset of [`JitCycleEdgeCell`]'s entry word; generated code loads
/// `[cell + JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET]`.
pub const JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET: i32 = 0;

/// `CRATONVM_JIT_CYCLE_EDGE_CELL` (default ON since round 9 wave 7;
/// `=0` is the kill switch): plan a compile-time recursion cycle's statically
/// bound CLOSING edge as a guarded call through a [`JitCycleEdgeCell`] instead
/// of `jit_invoke_dispatch`. Read at compile time only. See
/// [`JitCycleEdgeCell`].
///
/// Measured on the wave-7 binary (2 interleaved rounds): `MutualRec mutual
/// 300000` 1006/932 -> 160/166 ms, `MutRec3 three` 717/752 -> 182/214 ms,
/// `MutRec3 evenodd` 1377/1394 -> 193/172 ms, identical answers; `MutualSoe`
/// still catches 6/6 StackOverflowErrors; `CratonBench` within noise (3
/// rounds); probes and the 95-vector regression suite green with it on.
pub fn jit_cycle_edge_cell_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_LATE_STATIC_CELL` (JIT round 20 wave 11, lane pLate; book
/// `docs/feature-designs/jit-ir-optimizations.md` P1 =
/// `jit-r21-nio-proposals-w18-L4.md` W18L4-3). Default ON, `=0` restores the
/// closing-edge-only plan. Read at compile time only.
///
/// An `invokestatic` site whose callee `callee_compiler` could not give at
/// compile time (the callee compiles AFTER its caller, the common order for a
/// background-compiled caller) used to be bound to `jit_invoke_dispatch` for
/// the life of the body: the ~250 ns funnel per call instead of a ~5 ns raw
/// CALL. With this on it gets the [`JitCycleEdgeCell`] a cycle's closing edge
/// gets, on exactly the same gates (the VM's `cycle_edge_target_admitted`,
/// the cell's own `admits`), so [`JitCache::put`] binds it once the callee
/// publishes. The cell is the late bind HotSpot does by patching the call at
/// its first execution.
///
/// Soundness is the cell's (see [`JitCycleEdgeCell`]): the class
/// initialization check is the VM admission's (the declaring class must be
/// initialized at bake time, so the raw CALL can never skip a `<clinit>`);
/// invalidation / redefinition / retirement clear the cell through the
/// existing `JitCache` paths and the site dispatches again; every crossing
/// runs the stack-floor guard, so a recursion made of late cells still
/// throws `StackOverflowError`.
pub fn jit_late_static_cell_enabled() -> bool {
    true
}

/// Whether a statically bound `invokestatic` site that is NOT a closing edge
/// takes a late-bind [`JitCycleEdgeCell`] ([`jit_late_static_cell_enabled`]):
/// only when the direct bind asked `callee_compiler` and got nothing
/// (`compiler_declined`). The remaining gates (static `0xb8`, not an
/// inherited-name edge, not sticky, both helpers wired, the VM's admission)
/// are the closing edge's and are checked by the caller.
pub(crate) fn late_static_cell_site(closes_cycle: bool, compiler_declined: bool) -> bool {
    !closes_cycle && compiler_declined && jit_late_static_cell_enabled()
}

/// `CRATONVM_JIT_OSR_LATE_STATIC_CELL` (t1 wave 2, lane jit2; B5 of
/// `docs/feature-designs/native-dispatch-architecture.md`;
/// perf, **default ON since j27 2026-10-28**, `=0` kills). Read at compile time only.
///
/// The single-pass OSR door (vm `jit_bridge::compile_osr_body`) gives an
/// `invokestatic` whose callee had no compiled body at the OSR compile
/// (`callee-not-yet-compiled`) the late-bind [`JitCycleEdgeCell`] the
/// method-entry doors give it under [`jit_late_static_cell_enabled`], so
/// [`JitCache::put`] binds the site once the callee publishes. Without it the
/// site stays on `jit_invoke_dispatch` for the life of that one long
/// activation (`R23SegMono order zero`: ~280-500 ns per call against 12 for
/// the same callee entered from a site that was bound).
///
/// Soundness is the late cell's: the same VM admission
/// (`cycle_edge_target_admitted`, which requires the declaring class to be
/// initialized at bake time), the cell's own `admits`, the stack-floor guard
/// on every crossing, and invalidation / redefinition / retirement clearing
/// the cell through `JitCache::invalidate_cycle_cells`. [`JitCache::put_osr`]
/// registers such an OSR body's key-waiting cells
/// ([`JitCache::publish_osr_body_late_cells`]); an OSR body is never itself a
/// cell's target. Implies the late cell's own switches
/// ([`jit_late_static_cell_enabled`], [`jit_cycle_edge_cell_enabled`],
/// [`jit_cycle_direct_bind_enabled`], [`direct_jit_callee_calls_enabled`]).
pub fn jit_osr_late_static_cell_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    jit_late_static_cell_enabled()
        && jit_cycle_edge_cell_enabled()
        && jit_cycle_direct_bind_enabled()
        && direct_jit_callee_calls_enabled()
}

/// `CRATONVM_JIT_IR_OSR_CELL_REGISTER` (default ON; `=0` is the kill switch,
/// which [`jit_osr_late_static_cell_enabled`] overrides -- and that switch is
/// default ON since j27 2026-10-28, so the kill also needs
/// `CRATONVM_JIT_OSR_LATE_STATIC_CELL=0`): register the late /
/// cycle-edge cells the IR table builder plants in an optimizing OSR body
/// (vm `jit_bridge::build_osr_optimizing_artifact`, through
/// [`JitCache::register_unpublished_body_late_cells`]). That body is never
/// published, so before t1 wave 3 (lane X;
/// `docs/feature-designs/jit-osr-exit-and-recompile.md`)
/// only the opt-in switch registered them and every such site stayed on
/// `jit_invoke_dispatch` for the life of the body. The planting has its own
/// gates (the late cell's and the cycle-edge cell's switches, the VM's
/// `cycle_edge_admission`); this one only decides whether a planted cell may
/// ever be filled. Soundness is the method-entry doors' since r20 wave 11: the
/// fill runs under `JitCache::mutation`, and a registered cell is cleared by
/// every invalidation of its target.
pub fn jit_ir_osr_cell_registration_enabled() -> bool {
    jit_osr_late_static_cell_enabled()
        || true
}

/// [`JitDirectCall::guard_class_id`] value that marks a row as a baked direct
/// call made through a RETIRE cell (round 9 wave 9, lane `evict9`,
/// `evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
///
/// Like [`JIT_CYCLE_EDGE_CELL_GUARD`], the row's `entry` is the cell's ADDRESS;
/// the entry it guards is [`JitCycleEdgeCell::pinned_entry`]. Only ever planned
/// on an `invokestatic` (0xb8) site ([`retirable_direct_call`]).
pub const JIT_RETIRE_CELL_GUARD: u32 = 0xFFFF_FFF1;

/// [`JitDirectCall::guard_class_id`] value that marks a row as a statically
/// bound INSTANCE self-call (round 9 wave 10, lane `call10`;
/// `perf-private-instance-self-recursion-dispatches-every-call-FIXED-20260918.md`).
///
/// The row's `entry` is `0` and unused: the target is this compile's own
/// entry, which the single-pass backend reaches the way it reaches a static
/// self-call -- `x64/op_invoke.rs::walk_invoke_instance` null-checks the
/// receiver (JVMS 6.5: NPE at the invoke) and hands the site to
/// `walk_invokestatic`'s self-recursive arm (stack guard, raw `CALL` patched
/// to offset 0, sentinel check). `num_params` excludes the receiver, as on
/// every instance row. Planned only by [`instance_self_call_goes_direct`]'s
/// callers, on an `invokevirtual`/`invokespecial` (never `invokeinterface`,
/// whose 5-byte form the static arm does not advance over).
///
/// t1 wave 3, lane X (jit3): also the POSITIVE marker of a STATIC self-call
/// ([`static_self_call_marker`]). Every single-pass door registers one at each
/// `invokestatic` it leaves to the raw self-recursive `CALL`, and under
/// [`static_self_call_marker_enabled`] the emitter refuses an `invokestatic`
/// that reaches that arm with neither a row nor an info nor this marker,
/// instead of reading the absence of registration as "self"
/// (`docs/feature-designs/jit-ir-optimizations.md`).
pub const JIT_SELF_CALL_GUARD: u32 = 0xFFFF_FFF2;

/// t1 wave 3, lane X (jit3): the [`JIT_SELF_CALL_GUARD`] marker row a
/// single-pass door registers at a STATIC self-call site it leaves to the raw
/// self-recursive `CALL` (`num_params` = the method's own compact parameter
/// count, `return_type` its own). `entry` is `0` and never read: the target is
/// the compile's own entry. The row is lifted through the bytecode loop
/// rewriter like every other `direct_calls` row, so each image of the site
/// keeps it.
pub fn static_self_call_marker(num_params: usize, return_type: u8) -> JitDirectCall {
    JitDirectCall {
        entry: 0,
        needs_context: true,
        num_params,
        return_type,
        guard_class_id: JIT_SELF_CALL_GUARD,
    }
}

/// `CRATONVM_JIT_STATIC_SELF_CALL_MARKER` (default ON, correctness; `=0` is the
/// kill switch): a door that registers [`static_self_call_marker`] rows asks
/// the single-pass emitter to require one (`BackendRequest::static_self_calls_registered`),
/// so an `invokestatic` with no row, no `JitInvokeInfo` and no marker refuses
/// the compile (`singlepass-codegen/static-invoke-without-registration`)
/// rather than `CALL`ing the body's own entry with another callee's operands
/// (the SBR4 `binarySort` shape). `=0` restores the inference: the markers are
/// still registered (they lower exactly as the unmarked site did), only the
/// refusal is withdrawn.
pub fn static_self_call_marker_enabled() -> bool {
    true
}

/// [`JitDirectCall::guard_class_id`] value that marks a row as a caller-held
/// synchronized direct CALL (round 11 wave 13, lane `spsync`; page
/// `r11w12-calls-single-pass-sync-direct-patch-FIXED-20260924`): `entry` is the
/// published, MONITOR-FREE body of a `static synchronized` callee, which only
/// `x64/op_invoke.rs`'s sync arm may CALL, with the class monitor held around
/// it. The class is the site's `ldc_class_info` row at the same pc. Only ever
/// planned on an `invokestatic` (0xb8) site, always together with the site's
/// `JitInvokeInfo`: a pc the codegen cannot serve with the monitor held (one a
/// handler of the caller protects, or one with no spill room for the mirror)
/// drops the row and dispatches by name through that info instead.
pub const JIT_SYNC_DIRECT_GUARD: u32 = 0xFFFF_FFF3;

/// The tag bit of a [`JitDirectCall::guard_class_id`] that marks a row as the
/// INSTANCE twin of [`JIT_SYNC_DIRECT_GUARD`] (round 13 wave 1, lane `sync`;
/// page `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow`,
/// option 1): a caller-held synchronized direct CALL of a `synchronized`
/// instance method, whose monitor is the RECEIVER (JVMS 2.11.10). The low bits
/// are the exact receiver class the site is guarded on, or `0` for a statically
/// bound site (`invokespecial`, or an `invokevirtual` the VM pinned). `entry` is
/// the callee's MONITOR-FREE body, which only `x64/op_invoke.rs`'s sync arm may
/// CALL; `num_params` excludes the receiver, as on every instance row. Always
/// planned together with the site's `JitInvokeInfo`, which the guard's miss
/// edge (and a pc the arm cannot serve) dispatches through. Built and read only
/// through [`sync_direct_instance_guard`] / [`sync_direct_instance_guard_class`],
/// which keep it clear of the `0xFFFF_FFFx` marker words above.
pub(crate) const JIT_SYNC_DIRECT_INSTANCE_TAG: u32 = 0x8000_0000;

/// The guard word of an instance caller-held synchronized row guarded on
/// `exact_class_id` (`0`: statically bound, no class guard), or `None` when the
/// id cannot be encoded below the tag and the marker words (never a real id).
pub fn sync_direct_instance_guard(exact_class_id: u32) -> Option<u32> {
    let word = JIT_SYNC_DIRECT_INSTANCE_TAG | exact_class_id;
    ((exact_class_id & JIT_SYNC_DIRECT_INSTANCE_TAG) == 0 && word < JIT_CYCLE_EDGE_CELL_GUARD)
        .then_some(word)
}

/// The exact receiver class (`0`: none) of a row [`sync_direct_instance_guard`]
/// built, or `None` for every other guard word: `0`, an intrinsic's receiver
/// class id, and the `0xFFFF_FFFx` markers.
pub(crate) fn sync_direct_instance_guard_class(guard_class_id: u32) -> Option<u32> {
    ((guard_class_id & JIT_SYNC_DIRECT_INSTANCE_TAG) != 0
        && guard_class_id < JIT_CYCLE_EDGE_CELL_GUARD)
        .then_some(guard_class_id & !JIT_SYNC_DIRECT_INSTANCE_TAG)
}

/// `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE` (default ON; `0|false|off|no` is the
/// kill switch): plan the single-pass INSTANCE caller-held synchronized direct
/// CALL ([`JIT_SYNC_DIRECT_INSTANCE_TAG`]). Off leaves every such site on the
/// dispatch helper and the monitored bytecode-callee door, the pre-round-13
/// route. Read at planning (compile time only), like `CRATONVM_JIT_SYNC_DIRECT`,
/// which must be on as well.
pub fn sp_sync_direct_instance_enabled() -> bool {
    true
}


/// `CRATONVM_JIT_INSTANCE_SELF_CALL` (default ON; `=0` opts out): lower a
/// statically bound instance self-call as a direct self-call instead of a
/// `jit_invoke_dispatch` round trip. See [`instance_self_call_goes_direct`].
pub fn jit_instance_self_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL` (JIT round 23 wave 3d, lane L1,
/// proposal 4e of `jit-r23-w2-proxy-proposals.md`; **default OFF** until an A/B
/// decides it): keep a method with a statically bound instance self-call in
/// the OPTIMIZING tier, lowering the self-call as a receiver-carrying direct
/// self-call (`invoke_kind == 4`, receiver in argument 0).
///
/// Off, such a method bails whole to single-pass
/// ([`instance_self_call_goes_direct`]'s planner exit), which is what refused
/// the IR for `SplitConstantPool.findEntry` / `tryFindUtf8` /
/// `tryFindClassEntry` (14 refusals on the `W2Proxy` define row). On:
///
/// * only a site [`instance_self_call_goes_direct`] admits: `invokevirtual`
///   of a private or `final` method (pinned by the owner resolver) or a
///   non-redirected `invokespecial`, of THIS method, in a non-`synchronized`
///   method, with a stable loader identity and the stack-guard helper. Such a
///   target is the only possible one for every receiver -- no subclass can
///   override a private or final method -- so no class guard and no CHA
///   dependency is needed;
/// * the receiver is null-checked before the call (JVMS 6.5) unless it is
///   this method's own `this` ([`crate::ir::Graph::receiver_param`]); the
///   check deopts to the invoke, which the interpreter re-executes and throws
///   the canonical NPE from;
/// * the call is the static self-call's route otherwise
///   (`ir_lower::emit_self_recursive_call`: sampled stack guard, raw `CALL`
///   to offset 0, sentinel check, callee-deopt service), with the VM handed a
///   kind-1 twin of the row ([`crate::ir::Graph::self_call_service_infos`]).
///
/// Read at planning (compile time only).
pub fn ir_direct_instance_self_call_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_COLD_INSTANCE_SELF_CALL_DISPATCH` (JIT round 23 wave 9,
/// lane L1, proxy-define page; perf, default ON, `=0` restores the bail): an
/// instance self-call [`instance_self_call_goes_direct`] admits but the
/// optimizing tier cannot take direct ([`ir_instance_self_call_direct_admits`]
/// refuses: the switch off, or more declared arguments than the entry
/// registers hold -- `SplitConstantPool.findEntry(int, T, U)` on Win64) used to
/// send the WHOLE method to single-pass, because a hot recursion dispatched
/// through `jit_invoke_dispatch` is far slower than single-pass's direct
/// self-call. When the profile proves the call never ran (call-site counts,
/// or the branch profile; [`site_unreached_by_branch_profile`]) there is no
/// recursion to protect:
/// the site is planned as the ordinary statically bound call and the method
/// keeps its optimizing body. The ClassFile API's constant-pool lookups
/// (`SplitConstantPool.findEntry` x2, `tryFindUtf8`, `tryFindClassEntry`) are
/// the shape: their only self-call is the retry after `fullScan()`, behind
/// `doneFullScan`, which a pool built from scratch never takes. Read at
/// planning (compile time only).
pub(crate) fn ir_cold_instance_self_call_dispatch_enabled() -> bool {
    true
}

/// The call-site evidence floor of [`site_unreached_by_branch_profile`]: the
/// method's busiest OTHER invoke site must have run at least this often while
/// the self-call site never ran. Far below [`ir::MIN_OBSERVATIONS_TO_PRUNE`]
/// because only the interpreted warm-up records call sites, and because a
/// wrong answer costs a slower (dispatched) call, never a wrong one.
pub(crate) const COLD_SELF_CALL_MIN_SITE_OBSERVATIONS: u32 = 256;

/// Does the profile prove that the call at `site_pc` never ran? Two kinds of
/// evidence, either one enough:
///
/// * **call sites** (recorded by default, `CRATONVM_TIER_PGO_RECEIVERS`): the
///   site has no count while another invoke site of the same method ran at
///   least [`COLD_SELF_CALL_MIN_SITE_OBSERVATIONS`] times;
/// * **branches** (`CRATONVM_TIER_PGO`): removing every branch edge the
///   profile never saw over at least [`ir::MIN_OBSERVATIONS_TO_PRUNE`]
///   executions of the branch (the fall-through of an always-taken
///   conditional branch, the target of a never-taken one) leaves `site_pc`
///   unreachable from pc 0 along normal flow ([`pcs_reachable_without`]).
///
/// `false` without a profile, for a pc outside `code[..code_len]`, and (branch
/// arm) for code no normal flow reaches even with every edge (handler-only
/// code): the conservative answer, which keeps the caller's old route. A
/// heuristic for a planning choice, never a correctness claim: the site is
/// still compiled as a working call.
pub(crate) fn site_unreached_by_branch_profile(
    profile: Option<&profile::MethodProfile>,
    code: &[u8],
    code_len: usize,
    site_pc: usize,
) -> bool {
    let Some(prof) = profile else {
        return false;
    };
    let code_len = code_len.min(code.len());
    if site_pc >= code_len {
        return false;
    }
    let site_count = prof.call_sites.get(&site_pc).copied().unwrap_or(0);
    let busiest_other = prof
        .call_sites
        .iter()
        .filter(|&(&pc, _)| pc != site_pc && pc < code_len)
        .map(|(_, &n)| n)
        .max()
        .unwrap_or(0);
    if site_count == 0 && busiest_other >= COLD_SELF_CALL_MIN_SITE_OBSERVATIONS {
        return true;
    }
    let is_conditional = |op: u8| matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7);
    let mut removed: Vec<(usize, usize)> = Vec::new();
    for (&pc, c) in prof.branches.iter() {
        if pc >= code_len || !is_conditional(code[pc]) {
            continue;
        }
        let Some(target) = bytecode_analysis::offset_branch_target(code, pc) else {
            continue;
        };
        if c.not_taken == 0 && c.taken >= ir::MIN_OBSERVATIONS_TO_PRUNE {
            removed.push((pc, pc + 3));
        }
        if c.taken == 0 && c.not_taken >= ir::MIN_OBSERVATIONS_TO_PRUNE {
            removed.push((pc, target));
        }
    }
    !removed.is_empty() && !pcs_reachable_without(code, code_len, &removed, &[site_pc])
}

/// Whether an instance self-call [`instance_self_call_goes_direct`] admitted
/// stays in the optimizing tier as a receiver-carrying direct self-call
/// (wave 3d) rather than bailing the method to single-pass. The static
/// self-call's own conditions, with one more argument register: the marshal
/// puts the context in `abi[0]`, the receiver in `abi[1]` and the `desc_args`
/// declared arguments after it, all in registers
/// (`ir_lower::emit_self_call_marshal_and_call` has no stack-argument path).
pub(crate) fn ir_instance_self_call_direct_admits(
    switch_on: bool,
    selfrec_direct: bool,
    ret: u8,
    void_admitted: bool,
    desc_args: usize,
    entry_regs: usize,
) -> bool {
    switch_on && selfrec_direct && (ret != b'V' || void_admitted) && desc_args + 2 <= entry_regs
}

/// Does the kind-4 row `info` carry a receiver (a wave-3d instance self-call)?
/// `num_jit_args` counts the receiver on every instance row
/// (`num_args = desc_args + has_receiver`), so it is one more than the
/// descriptor's argument count exactly for those. `false` for every other
/// kind and for a descriptor that does not parse.
pub fn self_call_info_has_receiver(info: &JitInvokeInfo) -> bool {
    info.invoke_kind == 4
        && crate::descriptor_shapes::static_call_shape(info.descriptor)
            .is_some_and(|(desc_args, _)| info.num_jit_args == desc_args + 1)
}




/// Whether the invoke of `(owner, method_name, descriptor)` by `opcode`, from
/// `cached`, is an instance self-call the single-pass backend lowers as a
/// DIRECT self-call ([`JIT_SELF_CALL_GUARD`]; round 9 wave 10, `call10`).
///
/// `owner` is the site's RESOLVED class: the private/final pin's owner for an
/// `invokevirtual`, the JVMS 6.5 selection start for an `invokespecial`.
/// `statically_bound` is the planner's own verdict (`invoke_kind == 1`): only
/// then is "same class, name and descriptor" the one possible target. Refused
/// for:
///
/// * a static method, a constructor, `invokeinterface` (a private interface
///   method pinned by `cha9b` keeps its bind; the self arm is 3-byte only);
/// * a `synchronized` method: the compiled entry does not take the monitor,
///   and a self-call on ANOTHER receiver must lock that receiver;
/// * an unstable loader identity (`self_call_identity_stable`, the same proof
///   the static self-call needs) or no stack-guard helper (the guard is what
///   turns runaway recursion into a catchable `StackOverflowError`);
/// * `CRATONVM_JIT_INSTANCE_SELF_CALL=0`.
#[allow(clippy::too_many_arguments)]
pub fn instance_self_call_goes_direct(
    cached: &CachedBytecodeMethod,
    opcode: u8,
    statically_bound: bool,
    owner: &str,
    method_name: &str,
    descriptor: &str,
    self_call_identity_stable: bool,
    self_call_stack_guard: usize,
) -> bool {
    !cached.is_synchronized
        && instance_self_call_shape(
            cached,
            opcode,
            statically_bound,
            owner,
            method_name,
            descriptor,
            self_call_identity_stable,
            self_call_stack_guard,
        )
}

/// `CRATONVM_JIT_SELF_LOCK_SELF_CALL` (JIT round 19 wave 5, lane sync5,
/// SYNCDOOR4-2 of `jit-r19-syncdoor4-proposals.md`; **default ON** since round 19, `0`
/// disables): plan a synchronized instance method's statically bound self-call
/// as a direct self-call ([`sync_self_call_goes_direct`]) when the compile may
/// be a self-locking body. Read at planning (compile time only).
pub fn self_lock_self_call_enabled() -> bool {
    true
}

/// JIT round 19 wave 5 (lane sync5, SYNCDOOR4-2): [`instance_self_call_goes_direct`]
/// for a `synchronized` instance method, under
/// [`self_lock_self_call_enabled`]. The planner asks it only for a compile that
/// may be a SELF-LOCKING body (`build_single_pass_tables`'
/// `sync_self_call_candidate`), and stages a dispatch fallback beside the
/// marker: only a body whose entry takes the receiver's monitor may be CALLed
/// for another activation of the method (a self-call may name ANOTHER
/// receiver, whose monitor the callee activation must take), so every run that
/// does not self-lock swaps the marker for that fallback
/// (`SinglePassTables::unbind_sync_self_calls`).
#[allow(clippy::too_many_arguments)]
pub fn sync_self_call_goes_direct(
    cached: &CachedBytecodeMethod,
    opcode: u8,
    statically_bound: bool,
    owner: &str,
    method_name: &str,
    descriptor: &str,
    self_call_identity_stable: bool,
    self_call_stack_guard: usize,
) -> bool {
    cached.is_synchronized
        && self_lock_self_call_enabled()
        && instance_self_call_shape(
            cached,
            opcode,
            statically_bound,
            owner,
            method_name,
            descriptor,
            self_call_identity_stable,
            self_call_stack_guard,
        )
}

/// The shape half of [`instance_self_call_goes_direct`] and
/// [`sync_self_call_goes_direct`]: everything but the `synchronized` rule.
#[allow(clippy::too_many_arguments)]
pub(crate) fn instance_self_call_shape(
    cached: &CachedBytecodeMethod,
    opcode: u8,
    statically_bound: bool,
    owner: &str,
    method_name: &str,
    descriptor: &str,
    self_call_identity_stable: bool,
    self_call_stack_guard: usize,
) -> bool {
    statically_bound
        && matches!(opcode, 0xb6 | 0xb7)
        && !cached.is_static
        && method_name != "<init>"
        && owner == &*cached.class_name
        && method_name == &*cached.method_name
        && descriptor == &*cached.method_descriptor
        && self_call_identity_stable
        && self_call_stack_guard != 0
        && jit_instance_self_call_enabled()
}

/// `CRATONVM_JIT_RETIRE_CELL` (default ON since round 9 wave 11; `=0` opts
/// out): make a baked
/// `invokestatic` direct call that sits inside a loop go through a retire cell
/// ([`retirable_direct_call`]). Read at compile time only.
///
/// # Why
///
/// A running compiled frame keeps its baked direct calls for as long as it
/// runs. When the callee is withdrawn (a deopt eviction, `MakeNotCompilable`,
/// a sweep), the caller is retired too -- but a caller that is still looping
/// (an OSR'd `main`) re-enters the withdrawn body on every iteration.
/// `TryLoop throw` on the w8b binary: 2 000 calls into a body whose failing
/// loop-header guard traps at once, each one then precise-resumed and
/// INTERPRETED for 65 541 iterations -- 16.4 s against HotSpot's 16 ms, while
/// the de-spec'd recompiles the page's per-bci fix produces were never reached.
///
/// Through the cell, the withdrawal clears the word and the next call takes
/// `jit_invoke_dispatch`, which enters the newest body (or the interpreter).
pub fn jit_retire_cell_enabled() -> bool {
    // MEASURED (w10, interleaved, 3 reps): `TryLoop throw` 8 500 -> 2 400 ms;
    // probes and the 95-vector regression suite unchanged with it on.
    true
}

/// `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT` (opt-in, default OFF until measured;
/// t1 wave 5, lane X2, page `t1-X2-ir-direct-binds-plant-no-retire-cell-20261026`):
/// a dependency invalidation (`retire_reason::INVALIDATED`: a class change, a
/// new subtype, a finalizable subtype, an inlined class) also makes every
/// method-entry body it withdrew not entrant, as a deoptimization's eviction
/// already does (`CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT`). The retire cells
/// ([`jit_retire_cell_enabled`]) only cover single-pass `invokestatic` binds;
/// an optimizing caller's baked `CALL` -- above all a running optimizing OSR
/// loop -- otherwise keeps entering the withdrawn body. HotSpot makes every
/// dependent nmethod not entrant. Effective once the VM armed the hook
/// (`JitCache::arm_not_entrant`); unarmed, nothing is patched. Since t1 wave 6
/// (lane J6) `Vm::new` arms it at startup whenever this switch is on
/// (`vm::jit::helpers::arm_not_entrant_for_dependency_invalidation`).
pub fn jit_invalidate_not_entrant_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT_SUPERSEDED` (j27 wave 5, lane X2LCAP;
/// default ON, `=0` kill; acts only under the opt-in
/// [`jit_invalidate_not_entrant_enabled`]): the dependency invalidation's
/// not-entrant pass also patches the SUPERSEDED method-entry bodies the same
/// sweep retired ([`doomed_superseded_entry_bodies`]). A C1 body a tier-up
/// displaced stays enterable through every caller that baked it whenever its
/// supersede forward was refused (a context-ABI mismatch,
/// `not_entrant::supersede_forward_refusal`); retiring it stopped no such
/// caller. The t1 wave-9 reason for leaving them alone -- the superseded list
/// was process-wide, so this VM's hook could patch another VM's body -- is
/// gone since the list is per cache (j27 wave 4, lane GLOBALS,
/// `superseded_bodies`).
pub fn jit_invalidate_not_entrant_superseded_enabled() -> bool {
    true
}

/// The bodies of `superseded` (this cache's live superseded list) that
/// `doomed` (the sweep's withdrawn entries) retired and that are entered at
/// METHOD ENTRY. An OSR body is entered only through the OSR door and its
/// memo, which refuse a retired body, so it needs no patch. Empty under the
/// `=0` kill of [`jit_invalidate_not_entrant_superseded_enabled`].
pub(crate) fn doomed_superseded_entry_bodies(
    superseded: &[Arc<CompiledMethod>],
    doomed: &std::collections::HashSet<usize>,
) -> Vec<Arc<CompiledMethod>> {
    if !jit_invalidate_not_entrant_superseded_enabled() {
        return Vec::new();
    }
    superseded
        .iter()
        .filter(|cm| !cm.compiled_via_osr && doomed.contains(&(cm.entry_ptr() as usize)))
        .cloned()
        .collect()
}

/// Whether bytecode `pc` lies inside a loop: between the target and the source
/// of some backward branch (`if*`, `goto`, `ifnull`/`ifnonnull`, `goto_w`).
/// A switch back-edge is not recognised (it answers `false`, the conservative
/// direction for [`retirable_direct_call`]).
pub fn bytecode_pc_in_loop(code: &[u8], code_len: usize, pc: usize) -> bool {
    let len = code_len.min(code.len());
    let mut p = 0usize;
    while p < len {
        if loop_back_edge_header(code, len, p).is_some_and(|t| t <= pc && pc <= p) {
            return true;
        }
        p += bytecode_analysis::step(code, p);
    }
    false
}

/// The direct-call row a compile door should push for a baked call to a
/// compiled callee at `pc` (round 9 wave 9, `evict9`).
///
/// Answers the plain row (`guard_class_id: 0`, `entry` = the callee entry)
/// unless ALL of these hold, in which case it plans a RETIRE cell, pushes it
/// onto `cells` (the caller's artifact must own them:
/// `CompiledMethod::_jit_cycle_cells`) and answers a
/// [`JIT_RETIRE_CELL_GUARD`] row carrying the cell's address:
///
/// * [`jit_retire_cell_enabled`];
/// * an `invokestatic` site (`invoke_kind == 3`, opcode `0xb8` at `pc`), the
///   only arm (`x64/op_invoke.rs::walk_invokestatic`) that lowers the row;
/// * (NOT a requirement since round 9 wave 10, `call10`: the callee's ABI.
///   The dispatch fallback takes the CALLER's VM context -- the emitter checks
///   the caller's `needs_heap` and keeps the plain CALL without it -- and reads
///   the arguments from the service range, so a context-free callee is
///   retirable too. `TryLoop.sumThrow` is one: a leaf array loop, so its OSR'd
///   `main` never got a cell and kept re-entering the trapping body.)
/// * a real entry (not `0`, not an intrinsic sentinel);
/// * the site is inside a loop ([`bytecode_pc_in_loop`]) -- the only place a
///   still-running METHOD-ENTRY caller re-enters its binding. An OSR body's
///   door plans with [`retirable_osr_direct_call`], which drops this clause.
///
/// The door must still (a) register the site's `JitInvokeInfo` (the fallback
/// dispatches through it) and (b) record `entry` itself -- not the cell --
/// as a baked callee (`_direct_callee_entries`), which is what roots the body
/// and what [`JitCache::put`] / [`JitCache::put_osr`] fill the cell from.
#[allow(clippy::too_many_arguments)]
pub fn retirable_direct_call(
    code: &[u8],
    code_len: usize,
    pc: usize,
    invoke_kind: u8,
    entry: usize,
    needs_context: bool,
    num_params: usize,
    return_type: u8,
    callee_class: &str,
    callee_method: &str,
    callee_descriptor: &str,
    cells: &mut Vec<Arc<JitCycleEdgeCell>>,
) -> JitDirectCall {
    plan_retirable_direct_call(
        false,
        code,
        code_len,
        pc,
        invoke_kind,
        entry,
        needs_context,
        num_params,
        return_type,
        callee_class,
        callee_method,
        callee_descriptor,
        cells,
    )
}

/// [`retirable_direct_call`] for a site of an OSR body (interpreter round i1
/// wave 21, lane L1): every `invokestatic` site gets the cell, not only one
/// inside a loop.
///
/// An OSR body exists for one long-running activation, and it runs every site
/// after its entry in that activation: the code after the loop too. A site
/// there bound as a plain CALL kept entering its callee's withdrawn body
/// after the callee's class was redefined while the loop ran -- the OSR'd
/// `main` of `RedefineCompiledOldConstantsProbe` ran the OLD `Target.parked`
/// for its `after=` call
/// (`interpreter-L0-a-running-compiled-caller-keeps-calling-a-redefined-callees-old-body-FIXED`).
/// Through the cell, the withdrawal clears the word and the call dispatches.
///
/// Cost: a site outside every loop runs at most once per OSR entry, so its
/// load, test and taken branch are not measurable; in-loop sites are planned
/// exactly as by [`retirable_direct_call`].
#[allow(clippy::too_many_arguments)]
pub fn retirable_osr_direct_call(
    code: &[u8],
    code_len: usize,
    pc: usize,
    invoke_kind: u8,
    entry: usize,
    needs_context: bool,
    num_params: usize,
    return_type: u8,
    callee_class: &str,
    callee_method: &str,
    callee_descriptor: &str,
    cells: &mut Vec<Arc<JitCycleEdgeCell>>,
) -> JitDirectCall {
    plan_retirable_direct_call(
        true,
        code,
        code_len,
        pc,
        invoke_kind,
        entry,
        needs_context,
        num_params,
        return_type,
        callee_class,
        callee_method,
        callee_descriptor,
        cells,
    )
}

/// The planner behind [`retirable_direct_call`] (`every_site == false`: only
/// an in-loop site) and [`retirable_osr_direct_call`] (`true`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_retirable_direct_call(
    every_site: bool,
    code: &[u8],
    code_len: usize,
    pc: usize,
    invoke_kind: u8,
    entry: usize,
    needs_context: bool,
    num_params: usize,
    return_type: u8,
    callee_class: &str,
    callee_method: &str,
    callee_descriptor: &str,
    cells: &mut Vec<Arc<JitCycleEdgeCell>>,
) -> JitDirectCall {
    let plain = JitDirectCall {
        entry,
        needs_context,
        num_params,
        return_type,
        guard_class_id: 0,
    };
    if !jit_retire_cell_enabled()
        || invoke_kind != 3
        || code.get(pc) != Some(&0xb8)
        || entry == 0
        || JitIntrinsic::is_sentinel_entry(entry)
        || !(every_site || bytecode_pc_in_loop(code, code_len, pc))
    {
        return plain;
    }
    // The cell admits only a body of the pinned ABI (`admits` compares
    // `needs_context`), which the pinned body is by construction.
    let cell = JitCycleEdgeCell::new_retire(
        entry,
        callee_class,
        callee_method,
        callee_descriptor,
        needs_context,
    );
    let row = JitDirectCall {
        entry: Arc::as_ptr(&cell) as usize,
        needs_context,
        num_params,
        return_type,
        guard_class_id: JIT_RETIRE_CELL_GUARD,
    };
    cells.push(cell);
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] retire-cell @pc={pc} -> {callee_class}.{callee_method}{callee_descriptor} entry={entry:#x}"
        );
    }
    row
}

// ---------------------------------------------------------------------------
// The single-pass OSR door's splices (round 13 wave 12, lane osrdoor, W5-1)
// ---------------------------------------------------------------------------
//
// `compile_osr_body` (vm `jit_bridge.rs`) reaches `x64::compile_with_param_slots`
// directly and used to hand it an EMPTY `inline_sites`, so every static or
// `invokespecial` helper an OSR loop calls was a full compiled call per
// iteration while the method-entry compile of the same method spliced it
// (`r12w5-mega4-megamorphic-call-time-accounting`, finding 1: about a quarter of
// a megamorphic iteration). These two functions give that door the same
// statically bound plan `build_single_pass_tables` makes, from the same policy
// (`plan_inline`, the same budgets and tiers). Guarded (PGO-02) variants are not
// planned here.
//
// Why a splice is sound in an OSR body, stated where the plan is made:
//  * nothing is published from inside a splice (`x64/inlining.rs`'s
//    postcondition rolls back a splice whose points do not name the callee under
//    a caller scope), so no OSR exit or trap resumes inside one; a point that
//    does carry a caller chain is transferred by
//    `transfer_osr_exit_chain_into_live_frame`, and `validate_osr_entry` refuses
//    an artifact with any unresumable point at admission;
//  * a method with an exception table compiles with `precise_exception_frames`,
//    and the interlock (`plan_inline`'s `PreciseExceptionFrames`, the clear
//    below, the emitter's own refusal) keeps every such body splice-free --
//    except, since round nio20 wave 3 (lane L3, W2L2-3), (a) a site OUTSIDE
//    every protected range, which the table does not concern, and (b) at a
//    protected site, a STATIC callee whose body cannot throw at all
//    ([`inline_site_body_cannot_throw`]: no invoke, no field or array access,
//    no allocation, no integer division, no `athrow`, no backward branch).
//    The interlock exists because the splice walk's two precise publishers
//    (`emit_post_invoke_exception_check`'s nested-invoke arm,
//    `emit_precise_null_check_field_store`) would file an ENCLOSING pc under
//    the callee's name. Both arm only at a protected pc (and
//    `may_file_by_bci` keeps them frame-less inside any splice), so at (a) the
//    body lowers as without precise frames and a throw in it propagates to the
//    caller, as the table says; at (b) neither is reachable, nothing in the
//    body can reach a handler, and a static callee has no receiver null check
//    in front of the splice. The emitter asks the same two questions before it
//    lifts its own refusal, and its deopt-metadata postcondition still rolls
//    back any splice that publishes a point.
//    `CRATONVM_JIT_OSR_SPLICE_PRECISE_NOTHROW=0` restores the empty plan;
//  * the callee's `<clinit>`, native shadows, size and shape are the resolver's
//    (`resolve_inline_site`), exactly as for a method-entry splice, and the
//    dependencies go on the artifact's `inlined_methods`, which `put_osr`
//    records and checks against the invalidation log like `put`;
//  * a redefinition of a spliced class takes the whole-cache path (the resolver
//    marks it copied), which withdraws the OSR body and sends a running frame
//    out at its next back-edge poll.
//
// Round 15 wave 1 (lane calls): `validate_osr_entry`'s `osr-entry-inlined-scope`
// rule ("inlined methods + points + no caller chain") no longer refuses a
// single-pass body this plan splices into (the first bullet above makes it
// vacuous for this emitter; `CRATONVM_JIT_OSR_SP_SPLICE_ENTRY=0` restores the
// refusal, under which such a loop ran interpreted at ~500 ns per iteration).

/// `CRATONVM_JIT_OSR_SPLICE` (**default ON**; `0` restores the empty plan the
/// single-pass OSR door compiled with before round 13 wave 12). Read once per
/// OSR compile, live (no cache static).
pub fn osr_door_splices_enabled() -> bool {
    true
}



/// The loop(s) an OSR compile at `entry_pc` was triggered by, as
/// `[header, back_edge]` ranges in the shape [`hot_loop_ranges`] answers.
///
/// The OSR trigger IS back-edge evidence: the interpreter counted this loop's
/// back edges past the OSR threshold before the door ran, whether or not branch
/// profiling was armed for the method (`MethodProfile::loops` is empty for an
/// unprofiled `main`, which priced exactly its hottest loop as cold). A
/// backward branch whose target or own pc is `entry_pc` names that loop; when
/// none does (an entry pc that is neither), the innermost backward-branch range
/// containing `entry_pc` does. Empty when `entry_pc` is in no loop at all.
pub(crate) fn osr_entry_loop_ranges(code: &[u8], code_len: usize, entry_pc: usize) -> Vec<(usize, usize)> {
    let len = code_len.min(code.len());
    let mut exact: Vec<(usize, usize)> = Vec::new();
    let mut innermost: Option<(usize, usize)> = None;
    let mut p = 0usize;
    while p < len {
        if let Some(t) = loop_back_edge_header(code, len, p) {
            let range = (t, p);
            if range.0 == entry_pc || range.1 == entry_pc {
                if !exact.contains(&range) {
                    exact.push(range);
                }
            } else if range.0 <= entry_pc
                && entry_pc <= range.1
                && innermost.is_none_or(|(h, b)| range.1 - range.0 < b - h)
            {
                innermost = Some(range);
            }
        }
        p += bytecode_analysis::step(code, p).max(1);
    }
    if exact.is_empty() {
        exact.extend(innermost);
    }
    exact
}

/// One statically bound call site of an OSR body offered to
/// [`plan_osr_door_splices`]: the names the door binds the site by and its
/// kind (`3` static, `1` special; anything else is not planned).
#[derive(Clone, Copy, Debug)]
pub struct OsrSpliceCandidate<'a> {
    pub pc: usize,
    pub invoke_kind: u8,
    pub class_name: &'a str,
    pub method_name: &'a str,
    pub descriptor: &'a str,
}

/// What [`plan_osr_door_splices`] admitted, in the shapes the backend and the
/// artifact take: the sites for `compile_with_param_slots`, the invalidation
/// triples for `CompiledMethod::inlined_methods`, whether a spliced body needs
/// the frame's VM context, and the decision totals for
/// `CompiledMethod::inline_tally`.
#[derive(Default)]
pub struct OsrSplicePlan {
    pub inline_sites: HashMap<usize, InlineSite>,
    pub inlined_methods: Vec<(String, String, String)>,
    pub needs_heap: bool,
    pub tally: InlineDecisionTally,
    /// Round 16 wave 13 (lane mega): the guarded variants of the virtual and
    /// interface sites [`plan_osr_door_guarded_splices`] planned, in the shape
    /// `compile_with_param_slots`' `inline_guard_variants` takes: `(receiver
    /// class id, the body THAT class dispatches to)`, best class first. Element
    /// `[0]` is also the site's `inline_sites` entry, as the method-entry
    /// planner keeps them. Empty unless `CRATONVM_JIT_OSR_SPLICE_GUARDED=1`.
    pub guard_variants: HashMap<usize, Vec<(u32, InlineSite)>>,
    /// The per-method budget the plan has spent so far, so the guarded pass
    /// charges the same budget the statically bound pass drew on.
    pub budget_spent: usize,
    /// Round nio20 wave 3 (lane L3, L3W2-6): what is left of the hot-loop
    /// overdraft ([`OSR_DOOR_HOT_LOOP_OVERDRAFT`]) a site inside the OSR
    /// door's hot loop may draw on once the per-method budget is spent, while
    /// the plan holds fewer than `IR_INLINE_MAX_SITES_HOT_LOOP` sites. Set by
    /// [`plan_osr_door_splices`] under [`osr_splice_hot_loop_cap_enabled`];
    /// `0` (the default) is the budget alone, as before.
    pub hot_loop_overdraft_left: usize,
    /// JIT round 18 wave 11 (lane osrfollow, OSRFOLLOW18-4): the sites
    /// [`plan_osr_door_guarded_splices`] planned through its POLYMORPHIC arm
    /// (`how=polymorphic`), in plan order. Read by
    /// [`osr_door_single_pass_polymorphic_splice_pc`].
    pub polymorphic_pcs: Vec<usize>,
}

/// Plan the splices of a single-pass OSR body (see the section comment above).
///
/// `enclosing` is the compiled method's `(class, name, descriptor)`, for the
/// recursion count; `resolve` is the VM's single-pass resolver. A candidate is
/// planned only at the opcode whose single-pass arm splices it unguarded:
/// `invokestatic` for kind 3, `invokespecial` for kind 1 (a private
/// `invokevirtual` the door pinned to kind 1 would need a guard variant, which
/// this door does not plan), and only inside a loop ([`bytecode_pc_in_loop`]).
/// With `precise_exception_frames` (the method has an exception table), under
/// [`osr_splice_precise_nothrow_enabled`], a site outside every protected range
/// plans as without a table, and a protected site only a static callee whose
/// body cannot throw ([`inline_site_body_cannot_throw`]); with the switch off
/// the plan is empty, as `build_single_pass_tables` clears it.
///
/// `osr_entry_pc` (round 14 wave 3, OD-1): the pc the OSR trigger fired at.
/// Its loop ([`osr_entry_loop_ranges`]) is priced hot like a profiled hot loop
/// (`FreqInlineSize` and the hot method budget) under
/// [`osr_splice_entry_loop_hot_enabled`]; `None` prices from `profile` alone.
///
/// Round nio20 wave 3 (L3W2-6): with a hot loop and
/// [`osr_splice_hot_loop_cap_enabled`], a site inside that loop may still be
/// planned once the budget is spent, from [`OSR_DOOR_HOT_LOOP_OVERDRAFT`],
/// while the plan holds fewer than `IR_INLINE_MAX_SITES_HOT_LOOP` sites -- the
/// optimizing door's hot-loop cap, priced in this door's budget units.
#[allow(clippy::too_many_arguments)]
pub fn plan_osr_door_splices(
    code: &[u8],
    code_len: usize,
    enclosing: (&str, &str, &str),
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    precise_exception_frames: bool,
    profile: Option<&profile::MethodProfile>,
    osr_entry_pc: Option<usize>,
    candidates: &[OsrSpliceCandidate<'_>],
    resolve: &dyn Fn(&str, &str, &str) -> Option<InlineSite>,
) -> OsrSplicePlan {
    let mut plan = OsrSplicePlan::default();
    // Round nio20 wave 3 (lane L3, W2L2-3): a method with an exception table
    // no longer clears its whole plan. At a protected site only a static body
    // that cannot throw is admitted (checked per site below, after the
    // resolver answered); an unprotected site plans as without a table.
    let nothrow_only = precise_exception_frames;
    if nothrow_only && !osr_splice_precise_nothrow_enabled() {
        return plan;
    }
    let (enclosing_class, enclosing_method, enclosing_desc) = enclosing;
    let hot_loops = osr_door_hot_loops(code, code_len, profile, osr_entry_pc);
    let mut budget = if hot_loops.is_empty() {
        MAX_INLINE_BUDGET
    } else {
        MAX_INLINE_BUDGET_HOT
    };
    if !hot_loops.is_empty() && osr_splice_hot_loop_cap_enabled() {
        plan.hot_loop_overdraft_left = OSR_DOOR_HOT_LOOP_OVERDRAFT;
    }
    // Round nio20 wave 2, lane L3 (proposal NIO20-L5-1 of
    // `jit-r20-nio-proposals-L5.md`): visit the candidates in the method-entry
    // planner's profile order (`ir_splice_order::profile_plan_order`), so a
    // site the branch profile shows unreached (the UTF-8 loops' `malformed` /
    // `xflow` exits) no longer spends the budget ahead of the in-loop helpers
    // behind it. Visit order only: every site is still planned on its own and
    // keyed by its pc.
    let ordered: Vec<&OsrSpliceCandidate<'_>> =
        osr_door_visit_order(code, code_len, profile, candidates);
    for c in ordered {
        let unguarded_arm = c.pc < code_len
            && matches!(
                (c.invoke_kind, code.get(c.pc).copied()),
                (3, Some(0xb8)) | (1, Some(0xb7))
            );
        // A site outside every loop runs at most once per OSR entry, so a
        // splice there buys nothing, and it keeps its retire-celled direct
        // call (`retirable_osr_direct_call`): the code after the loop runs in
        // the same long-lived activation, and the cell is what sends it to a
        // redefined callee's new body.
        if !unguarded_arm || !bytecode_pc_in_loop(code, code_len, c.pc) {
            continue;
        }
        if jit_force_interpret(c.class_name, c.method_name) {
            plan.tally.record_refusal(&InlineRefusal::ForceInterpreted);
            continue;
        }
        // L3W2-6: the hot-loop overdraft this site may draw on (zero outside
        // the hot loop, past the hot-loop site cap, or with the switch off).
        let overdraft = osr_door_overdraft_for(&plan, &hot_loops, c.pc);
        let available = budget.saturating_add(overdraft);
        if available == 0 {
            plan.tally
                .record_refusal(&InlineRefusal::BudgetAlreadySpent);
            continue;
        }
        // W2L2-3: under precise exception frames a site OUTSIDE every
        // protected range is unaffected by the table: both precise publishers
        // the splice walk reaches arm only at a protected pc, and anywhere
        // else (and inside any splice, `may_file_by_bci`) they take the
        // frame-less exit every unprotected invoke of the method takes, which
        // propagates to the caller exactly as the table says. A PROTECTED
        // site admits only a static callee (an `invokespecial` splice is
        // preceded by the receiver's null check, a throwing instruction)
        // whose body cannot throw (below).
        let protected_site = nothrow_only && pc_in_protected_range(exception_table, c.pc);
        if protected_site && c.invoke_kind != 3 {
            plan.tally
                .record_refusal(&InlineRefusal::PreciseExceptionFrames);
            continue;
        }
        // The call-site intrinsic ladder outranks a splice, as at the
        // method-entry door (`CRATONVM_JIT_STATIC_INTRINSIC_FIRST`).
        if c.invoke_kind == 3
            && try_resolve_intrinsic(c.class_name, c.method_name, c.descriptor).is_some()
            && static_intrinsic_first_enabled()
        {
            continue;
        }
        let Some(site) = resolve(c.class_name, c.method_name, c.descriptor) else {
            plan.tally.record_refusal(&InlineRefusal::CalleeUnresolved {
                speculated_class_id: None,
            });
            continue;
        };
        // W2L2-3: the precise-frames interlock, answered per body. A body
        // that cannot throw reaches neither precise publisher of the splice
        // walk and can never reach this method's handlers, so the policy is
        // asked as for a method without a table (the emitter re-asks the same
        // two questions before it lifts its own refusal).
        if protected_site && !inline_site_body_cannot_throw(&site) {
            plan.tally
                .record_refusal(&InlineRefusal::PreciseExceptionFrames);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] osr-splice-refused {}.{}{} @pc={}: precise exception \
                     frames and a body that can throw",
                    c.class_name, c.method_name, c.descriptor, c.pc
                );
            }
            continue;
        }
        let recursive_copies = usize::from(
            site.class_name == enclosing_class
                && c.method_name == enclosing_method
                && c.descriptor == enclosing_desc,
        );
        let decided = plan_inline(&InlineRequest {
            pc: c.pc,
            invoke_kind: c.invoke_kind,
            site_method_name: c.method_name,
            site_descriptor: c.descriptor,
            site_class_name: c.class_name,
            site: Some(&site),
            site_is_hot: call_site_is_hot(c.pc, &hot_loops, profile),
            receivers: profile.and_then(|p| p.receivers.get(&c.pc)),
            call_site_evidence: profile
                .map(|p| p.call_site_count(c.pc))
                .unwrap_or(profile::CallSiteEvidence::None),
            depth: 1,
            recursive_copies,
            budget_remaining: available,
            pc_in_protected_range: pc_in_protected_range(exception_table, c.pc),
            // `false` under `nothrow_only` as well: the interlock was
            // answered for this body above (`inline_site_body_cannot_throw`).
            precise_exception_frames: false,
            // No guarded arm: statically bound sites only.
            caps: InlineBackendCaps::single_pass_x64(),
            receiver_class_namer: None,
            receiver_callee_resolver: None,
            unique_concrete_resolver: None,
        });
        plan.tally.record(&decided);
        if !decided.is_admitted() || decided.is_speculative() {
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] osr-splice-refused {}.{}{} @pc={}: {:?}",
                    c.class_name, c.method_name, c.descriptor, c.pc, decided.verdict
                );
            }
            continue;
        }
        // The budget pays first; only the part it cannot pay comes out of the
        // hot-loop overdraft (which `available` admitted only for this site).
        let from_budget = decided.expansion_cost.min(budget);
        budget -= from_budget;
        plan.hot_loop_overdraft_left = plan
            .hot_loop_overdraft_left
            .saturating_sub(decided.expansion_cost - from_budget);
        plan.budget_spent = plan.budget_spent.saturating_add(decided.expansion_cost);
        if site.needs_heap {
            plan.needs_heap = true;
        }
        for dependency in decided.invalidation_triples() {
            if !plan.inlined_methods.contains(&dependency) {
                plan.inlined_methods.push(dependency);
            }
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] osr-splice-planned {}.{}{} @pc={} cost={} budget_left={budget}",
                c.class_name, c.method_name, c.descriptor, c.pc, decided.expansion_cost
            );
        }
        plan.inline_sites.insert(c.pc, site);
    }
    plan
}

/// Round nio20 wave 3 (lane L3, W2L2-3): `CRATONVM_JIT_OSR_SPLICE_PRECISE_NOTHROW`
/// (perf, **default ON**; `0` restores the empty plan). The single-pass OSR
/// door of a method with an exception table (compiled with
/// `precise_exception_frames`) plans its sites outside every protected range
/// as without a table, and at a protected site the static callees whose
/// bodies cannot throw ([`inline_site_body_cannot_throw`]). A benchmark's or a server's
/// `main` usually has a `try`, so before this every helper its hot loop called
/// was a full compiled CALL (`R17NioRows fence` under `--real-jdk`: the
/// compiled empty `Reference.reachabilityFence` body, ~42 ns against
/// HotSpot's 7). Read once per OSR compile, live.
pub fn osr_splice_precise_nothrow_enabled() -> bool {
    true
}

/// Round nio20 wave 4 (lane L3, W3L3-1 / W3L3-2):
/// `CRATONVM_JIT_SP_SPLICE_PRECISE_PER_SITE` (perf, **default ON**; `0`
/// restores the wave-3 behaviour). Takes the per-site precise-frames
/// interlock wave 3 gave the single-pass OSR door's statically bound pass
/// ([`plan_osr_door_splices`]) to the two places that still refused every
/// site of a `precise_exception_frames` compile:
///
/// * the METHOD-ENTRY single-pass compile (`build_single_pass_tables`): a site
///   outside every protected range plans as without a table (statically bound
///   and guarded alike), a protected one only a static body that cannot throw
///   ([`inline_site_body_cannot_throw`]), and the post-plan clear keeps exactly
///   those sites;
/// * the OSR door's GUARDED pass ([`plan_osr_door_guarded_splices`]): its
///   virtual / interface sites outside every protected range.
///
/// The soundness argument is the wave-3 one, unchanged: both precise
/// publishers the splice walk reaches (`emit_post_invoke_exception_check`,
/// `emit_precise_null_check_field_store`) arm only at a protected pc, and
/// `Compiler::may_file_by_bci` keeps them frame-less inside any splice, so at
/// an unprotected site the body lowers exactly as in a compile without
/// precise frames and a throw in it propagates to the caller, which is what
/// the table says for that pc. The emitter (`try_emit_inline_site_with`) asks
/// the same per-site question before it splices. Read per compile, live.
pub fn sp_splice_precise_per_site_enabled() -> bool {
    true
}

/// The precise-frames interlock of ONE method-entry single-pass site, as the
/// `precise_exception_frames` argument [`plan_inline`] takes: `true` (refuse
/// with `InlineRefusal::PreciseExceptionFrames`) when the compile has precise
/// frames and the site is neither outside every protected range nor a
/// statically bound `invokestatic` whose resolved body cannot throw. Without
/// precise frames, or with [`sp_splice_precise_per_site_enabled`] off, the
/// compile-wide answer (`precise_exception_frames` itself), as before.
pub fn sp_entry_precise_interlock_holds(
    precise_exception_frames: bool,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    pc: usize,
    invoke_kind: u8,
    cp_site: Option<&InlineSite>,
) -> bool {
    if !precise_exception_frames || !sp_splice_precise_per_site_enabled() {
        return precise_exception_frames;
    }
    if !pc_in_protected_range(exception_table, pc) {
        return false;
    }
    !(invoke_kind == 3 && cp_site.is_some_and(inline_site_body_cannot_throw))
}

/// Round nio20 wave 3 (lane L3, L3W2-6): `CRATONVM_JIT_OSR_SPLICE_HOT_LOOP_CAP`
/// (perf, **default ON**; `0` keeps the budget alone). The single-pass OSR
/// door takes the optimizing door's hot-loop cap: a site inside the hot loop
/// may draw on [`OSR_DOOR_HOT_LOOP_OVERDRAFT`] once the per-method budget is
/// spent, while the plan holds fewer than `IR_INLINE_MAX_SITES_HOT_LOOP`
/// sites. Also off under `CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP=0`, so one switch
/// A/Bs the cap at both doors. Read once per OSR compile.
pub(crate) fn osr_splice_hot_loop_cap_enabled() -> bool {
    ir_splice_order::ir_splice_hot_loop_cap_enabled()
}

/// The extra budget [`osr_splice_hot_loop_cap_enabled`] gives the OSR door's
/// hot-loop sites: [`MAX_INLINE_BUDGET_HOT`] scaled by the share the
/// optimizing door's hot-loop cap adds to its flat cap
/// (`IR_INLINE_MAX_SITES_HOT_LOOP` 40 over `IR_INLINE_MAX_SITES` 24: 1333).
/// The budget bounds the code buffer and the frame the backend sizes from the
/// plan, so the overdraft is bounded the same way (about 85 KiB of buffer
/// estimate more in the worst case).
pub const OSR_DOOR_HOT_LOOP_OVERDRAFT: usize = MAX_INLINE_BUDGET_HOT
    * (ir_splice_order::IR_INLINE_MAX_SITES_HOT_LOOP - IR_INLINE_MAX_SITES)
    / IR_INLINE_MAX_SITES;

/// The overdraft the OSR-door site at `pc` may draw on: what is left of it,
/// when `pc` is inside one of `hot_loops` and the plan holds fewer than
/// `IR_INLINE_MAX_SITES_HOT_LOOP` sites; `0` otherwise.
pub(crate) fn osr_door_overdraft_for(plan: &OsrSplicePlan, hot_loops: &[(usize, usize)], pc: usize) -> usize {
    let in_hot_loop = hot_loops
        .iter()
        .any(|&(header, back)| pc >= header && pc <= back);
    if !in_hot_loop || plan.inline_sites.len() >= ir_splice_order::IR_INLINE_MAX_SITES_HOT_LOOP {
        return 0;
    }
    plan.hot_loop_overdraft_left
}

/// Round nio20 wave 3 (lane L3, W2L2-3): can the spliced body of `site` run
/// without throwing anything and without reaching a deopt publisher? The
/// predicate both halves of the precise-frames interlock ask
/// ([`plan_osr_door_splices`] and `x64::Compiler::try_emit_inline_site_with`)
/// before a splice is admitted into a `precise_exception_frames` compile.
///
/// True only for a STATIC, unsynchronized callee (no receiver null check in
/// front of the splice, no monitor on its exits) whose resolver output names
/// nothing it touches (no field, static, invoke, nested, elided or masked
/// row) and whose bytecode ([`bytecode_body_cannot_throw`]) is local, constant,
/// stack, arithmetic (no integer `div`/`rem`), conversion, compare, forward
/// branch and return instructions only. Such a body reaches neither precise
/// publisher of the splice walk (`emit_post_invoke_exception_check`'s
/// nested-invoke arm, `emit_precise_null_check_field_store`), has no
/// safepoint poll (no back edge), and cannot transfer control to any handler
/// of the enclosing method.
pub fn inline_site_body_cannot_throw(site: &InlineSite) -> bool {
    if !site.callee_is_static
        || site.callee_is_synchronized
        || site.callee_original_code.is_some()
        || !site.field_info.is_empty()
        || !site.compact_field_info.is_empty()
        || !site.static_field_info.is_empty()
        || !site.invoke_targets.is_empty()
        || !site.resolved_invoke_infos.is_empty()
        || !site.nested_sites.is_empty()
        || !site.elided_invoke_pcs.is_empty()
        || !site.unloaded_static_call_pcs.is_empty()
    {
        return false;
    }
    let len = site.callee_code_len;
    len != 0
        && len <= site.callee_code.len()
        && bytecode_body_cannot_throw(&site.callee_code[..len])
}

/// The bytecode half of [`inline_site_body_cannot_throw`]: every instruction
/// of `code` (exactly `code`, no padding) is one that cannot throw, every
/// branch is FORWARD to a pc inside `code`, and the body returns. `ldc` is
/// refused (a `String` / `Class` constant resolves and may allocate), as are
/// `wide`, the switches, `jsr`/`ret` and anything unknown.
pub fn bytecode_body_cannot_throw(code: &[u8]) -> bool {
    let len = code.len();
    let mut pc = 0usize;
    let mut returns = false;
    while pc < len {
        let width = match code[pc] {
            // nop, aconst_null, iconst_*, lconst_*, fconst_*, dconst_*
            0x00..=0x0f => 1,
            0x10 => 2, // bipush
            0x11 => 3, // sipush
            // iload..aload, istore..astore with an index byte
            0x15..=0x19 | 0x36..=0x3a => 2,
            // iload_n..aload_n, istore_n..astore_n
            0x1a..=0x2d | 0x3b..=0x4e => 1,
            // pop, pop2, dup*, swap
            0x57..=0x5f => 1,
            // add/sub/mul (all types), fdiv/ddiv, frem/drem, neg, shifts,
            // and/or/xor -- idiv/ldiv/irem/lrem (0x6c, 0x6d, 0x70, 0x71) only
            // by a non-zero constant, below
            0x60..=0x6b | 0x6e | 0x6f | 0x72..=0x83 => 1,
            0x84 => 3, // iinc
            // conversions, lcmp, fcmp*, dcmp*
            0x85..=0x98 => 1,
            // nio21 wave 12 (lane L4, W10L2-3): idiv / ldiv / irem / lrem by a
            // non-zero immediate pushed right before it, on no branch's
            // target -- it cannot throw (`ir::splice_division_cannot_trap`;
            // `ldc` is refused above, so no divisor here is an `ldc`). The
            // single-pass splice walk lowers exactly that shape with no
            // publisher (`x64::inlining`'s `inline_const_divisor` arm).
            0x6c | 0x6d | 0x70 | 0x71
                if ir::splice_division_cannot_trap(code, len, pc, |_| None) =>
            {
                1
            }
            // if*, if_icmp*, if_acmp*, goto, ifnull, ifnonnull
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) else {
                    return false;
                };
                let offset = i16::from_be_bytes([hi, lo]);
                if offset <= 0 {
                    return false;
                }
                // Cast: a positive i16 widens losslessly.
                if pc + offset as usize >= len {
                    return false;
                }
                3
            }
            // ireturn..return
            0xac..=0xb1 => {
                returns = true;
                1
            }
            _ => return false,
        };
        pc += width;
    }
    returns && pc == len
}

/// `CRATONVM_JIT_OSR_SPLICE_PROFILE_ORDER` (perf, **default ON**; `0` keeps
/// the bytecode order): [`plan_osr_door_splices`] visits its candidates in
/// [`ir_splice_order::profile_plan_order`]'s order. Also off when
/// `CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER=0`, so one switch A/Bs the ranking at
/// both doors. Read once per OSR compile.
pub(crate) fn osr_splice_profile_order_enabled() -> bool {
    ir_splice_order::ir_splice_profile_order_enabled()
        && true
}

/// The order [`plan_osr_door_splices`] visits `candidates` in: profile order
/// ([`osr_splice_profile_order_enabled`]), else as given (bytecode order).
/// Every candidate appears exactly once; one the ranking does not name (it
/// cannot happen, the ranking is a permutation of its input) is appended.
pub(crate) fn osr_door_visit_order<'c, 'a>(
    code: &[u8],
    code_len: usize,
    profile: Option<&profile::MethodProfile>,
    candidates: &'c [OsrSpliceCandidate<'a>],
) -> Vec<&'c OsrSpliceCandidate<'a>> {
    if candidates.len() < 2 || !osr_splice_profile_order_enabled() {
        return candidates.iter().collect();
    }
    let ops: Vec<(usize, u16, u8)> = candidates
        .iter()
        .map(|c| (c.pc, 0u16, code.get(c.pc).copied().unwrap_or(0)))
        .collect();
    let order = ir_splice_order::profile_plan_order(code, code_len, &ops, profile);
    let mut taken = vec![false; candidates.len()];
    let mut out: Vec<&OsrSpliceCandidate<'a>> = Vec::with_capacity(candidates.len());
    for site in &order.sites {
        if let Some(i) = (0..candidates.len()).find(|&i| !taken[i] && candidates[i].pc == site.op.0)
        {
            taken[i] = true;
            out.push(&candidates[i]);
        }
    }
    for (i, c) in candidates.iter().enumerate() {
        if !taken[i] {
            out.push(c);
        }
    }
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] osr-splice-profile-order sites={} cold={} order=[{}]",
            order.sites.len(),
            order.count_of(ir_splice_order::SiteClass::Cold),
            order.describe()
        );
    }
    out
}

/// Intern every planned body's own call targets into the OSR compile's arenas,
/// as `build_single_pass_tables` does for its plan: the strings and infos go to
/// the artifact (`_jit_strings` / `_jit_invoke_infos`) and every baked direct
/// entry to `direct_callee_entries` (`_direct_callee_entries`, the keep-alive a
/// raw `CALL` inside a splice needs as much as a top-level one).
pub fn intern_osr_door_splice_targets(
    plan: &mut OsrSplicePlan,
    owned_strings: &mut Vec<Box<str>>,
    owned_invoke_infos: &mut Vec<Box<JitInvokeInfo>>,
    direct_callee_entries: &mut Vec<(usize, u64)>,
) {
    for site in plan.inline_sites.values_mut() {
        intern_inline_invoke_targets(
            site,
            owned_strings,
            owned_invoke_infos,
            direct_callee_entries,
        );
    }
    // Round 16 wave 13 (lane mega): the guard chain splices the VARIANTS'
    // bodies (`x64/op_invoke.rs`, `try_emit_inline_site(pc, site)` per
    // variant), not the `inline_sites` copy of `[0]`, so every variant needs
    // its own interned targets, as `build_single_pass_tables` interns them.
    for variants in plan.guard_variants.values_mut() {
        for (_, site) in variants.iter_mut() {
            intern_inline_invoke_targets(
                site,
                owned_strings,
                owned_invoke_infos,
                direct_callee_entries,
            );
        }
    }
}

/// The hot loops the single-pass OSR door prices its sites by: the profile's
/// ([`hot_loop_ranges`]) plus, under OD-1, the loop the OSR trigger fired in
/// ([`osr_entry_loop_ranges`]). Shared by both OSR-door planners so the two
/// passes price a site alike.
pub(crate) fn osr_door_hot_loops(
    code: &[u8],
    code_len: usize,
    profile: Option<&profile::MethodProfile>,
    osr_entry_pc: Option<usize>,
) -> Vec<(usize, usize)> {
    let mut hot_loops = hot_loop_ranges(code, code_len, profile);
    if let Some(entry_pc) = osr_entry_pc.filter(|_| true) {
        for range in osr_entry_loop_ranges(code, code_len, entry_pc) {
            if !hot_loops.contains(&range) {
                hot_loops.push(range);
            }
        }
    }
    hot_loops
}

// ---------------------------------------------------------------------------
// The single-pass OSR door's GUARDED splices (round 16 wave 13, lane mega)
// ---------------------------------------------------------------------------
//
// `R16Bench poly` runs `s += sh[i & 3].area()` over three final classes (49 /
// 25 / 25 %) in ONE call of `poly`, so the loop lives in an OSR body. The
// optimizing OSR door refuses that body by default (`osr optimizing (bg) ...
// refused -- the lowerer emitted no entry stub at this header`: a memory phi is
// live across the header and no local names it, which only the opt-in
// `CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS` / `_DATAFLOW_LIVE_IN` excuse), so
// the loop runs `osr/sp`. The single-pass OSR door planned statically bound
// splices only (round 13 wave 12: "Guarded (PGO-02) variants are not planned
// here"), so the interface site paid a PIC hit and a real call into a 464-byte
// optimizing frame on every iteration, and the optimizing tier's polymorphic
// guarded splice (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`) could never
// engage on it: that is why it measured no gain
// (`jit-r16-polyclear-proposals-RETIRED-20261005` PC-3).
//
// This pass gives the door the guarded lowering the method-entry single-pass
// compile already has (`x64/op_invoke.rs`'s PGO-02 guard chain, which takes any
// number of variants): a profiled, in-loop `invokevirtual` / `invokeinterface`
// is planned by `plan_inline` with the guarded capability (Monomorphic,
// Bimorphic), and, under `CRATONVM_JIT_OSR_SPLICE_POLYMORPHIC`, a site the
// policy refuses as `ReceiverNotDominant` whose profile splits over three or
// four classes ([`ir::profile_polymorphic_receiver_classes`], the optimizing
// tier's own bars) gets one guarded body per class.
//
// Soundness, beyond the round-13 argument for the statically bound pass (on the
// section comment above `osr_door_splices_enabled`):
//  * every guard is an exact class-id compare whose miss edge is the site's
//    UNCHANGED dispatch (MIC / PIC / hashed stub / helper), never a deopt, so a
//    receiver of any other class, a null and (at `invokeinterface`) an array
//    reach the dispatch that resolves them or throws (NPE, ICCE, AME);
//  * each body is the one its OWN class dispatches to
//    (`resolve_receiver_inline_site`, the method-entry resolver: JVMS §5.4.6
//    selection, default methods included; an abstract or conflicting answer
//    resolves nothing and the class is not planned);
//  * the dependencies are `plan_inline`'s (`SpeculatedReceiver` and
//    `InlinedCallee` per body) and go on the artifact's `inlined_methods`, so a
//    redefinition withdraws the OSR body as it does a statically bound splice;
//  * a body the emitter cannot splice is rolled back variant by variant, and a
//    backend refusal is asked again with no splices at all (`compile_osr_body`).

/// `CRATONVM_JIT_OSR_SPLICE_GUARDED` (round 16 wave 13, lane mega; default ON
/// since wave 14 together with the polymorphic arm below: `R16Bench poly` 1221
/// -> 533 ms, HotSpot 419; alone it measured 1411, so the pair flips as one;
/// `=0` off): the single-pass OSR door also plans PGO-02 guarded
/// splices at its in-loop virtual and interface sites
/// ([`plan_osr_door_guarded_splices`]). Needs `CRATONVM_JIT_OSR_SPLICE`. Read
/// once per OSR compile, live (no cache static).
pub fn osr_door_guarded_splices_enabled() -> bool {
    osr_door_splices_enabled()
        && true
}



/// JIT round 18 wave 9 (lane osrpoly;
/// `r18-osr-door-opaque-entry-drops-the-polymorphic-splice-FIXED-20261006.md`):
/// `CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS` (**default ON**, `=0` off).
/// The optimizing OSR door declines to compile a method whose in-loop
/// virtual / interface site the single-pass OSR door would splice
/// polymorphically ([`osr_door_single_pass_polymorphic_splice_pc`], the
/// single-pass planner's own answer since round 18 wave 11) while the
/// optimizing tier's own polymorphic splice is off, so the loop runs the
/// spliced single-pass OSR body. Read once per optimizing OSR-door verdict.
///
/// Default ON as a regression fix, not a speculation: it restores which body
/// such a loop ran before every OSR-door compile took opaque entry merges
/// (commit 44df838b5f, `osr_opaque_all` in `ir_tier_attempt`; `R16Bench poly`
/// 948 -> ~1950 ms, HotSpot ~730). It only chooses between two bodies that are
/// each correct, and never changes a method-entry compile.
pub(crate) fn osr_door_prefer_spliced_single_pass_enabled() -> bool {
    true
}

/// JIT round 18 wave 9 (lane osrpoly;
/// `r18-osr-door-opaque-refusal-poisons-the-method-entry-memo-FIXED-20261006.md`):
/// `CRATONVM_JIT_OSR_DOOR_OWN_REFUSAL_MEMO` (**default ON**, `=0` off). An
/// acceptance refusal of an OSR-door body built with door-only opaque entry
/// arms is not written into the per-method IR refusal memo the method-entry
/// compile reads (`ir_tier_attempt`'s `osr_handler_arms_handed`). Since
/// commit 44df838b5f every OSR-door build has such arms, so a refusal earned by
/// the opaque merges' lost header facts kept the method-entry body off the
/// optimizing tier. Read once per OSR-door IR build, live.
pub(crate) fn osr_door_own_refusal_memo_enabled() -> bool {
    true
}

/// ACCEPT21-3 (t1 wave 7 lane NBA, `docs/feature-designs/wire-tiered-manager.md`;
/// `CRATONVM_JIT_OSR_ROUTE_OWN_REFUSAL_KEY`, perf,
/// **default ON since j27 2026-10-28**, `=0` kills): the
/// optimizing OSR route reads and writes the IR refusal memo under its own key
/// ([`ir_refusal_memo_route_key`]), so a method-entry acceptance refusal no
/// longer skips every OSR-route IR attempt (that route runs passes -- loop field
/// forward, promotion -- the method-entry build does not, and may earn evidence
/// the other did not), and an OSR-route refusal never skips a method-entry
/// attempt. A sticky site-trap ban on the shared key still holds on the route.
/// Read once per IR attempt of an OSR-route compile.
pub(crate) fn osr_route_own_refusal_key_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// JIT round 21 (lane accept;
/// `jr21-osr-route-refusal-poisons-the-method-entry-memo-FIXED-20261015.md`):
/// `CRATONVM_JIT_OSR_ROUTE_OWN_REFUSAL_MEMO` (perf, **default ON**, `=0`
/// restores the shared write). An acceptance refusal of ANY optimizing
/// OSR-route build (`CompileRequest::osr_optimizing_route`) stays out of the
/// per-method IR refusal memo the method-entry compile reads, not only one
/// built with door-only opaque arms ([`osr_door_own_refusal_memo_enabled`]).
/// The route runs another pass set (`ir_optimize::OptimizeContext::osr_route`:
/// no early mask predicate, no pre-header `getstatic` hoist unless its OSR
/// switches say so, the OSR-only field forward), so its evidence is not the
/// method-entry build's; a narrowed (`CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR`) or
/// arm-less OSR build's refusal used to skip the method-entry body's IR
/// attempt. The route memoises its own refusal
/// (`build_osr_optimizing_artifact`), so no OSR rebuild is added. Under
/// [`osr_route_own_refusal_key_enabled`] (ACCEPT21-3) the route writes its OWN
/// key instead, which no method-entry compile reads, so this switch then has
/// nothing to skip. Read once per refused OSR-route build.
pub(crate) fn osr_route_own_refusal_memo_enabled() -> bool {
    true
}

/// Does an acceptance refusal of this build write the IR refusal memo? Not for
/// a build with door-only opaque arms (round 18), and not for an optimizing
/// OSR-route build writing the SHARED key under
/// [`osr_route_own_refusal_memo_enabled`] (round 21, lane accept).
pub(crate) fn ir_acceptance_refusal_writes_memo(osr_handler_arms_handed: bool, osr_route: bool) -> bool {
    if osr_handler_arms_handed {
        return false;
    }
    !(osr_route && !osr_route_own_refusal_key_enabled() && osr_route_own_refusal_memo_enabled())
}


/// The IR refusal memo key the compile `osr_route` reads and writes:
/// `shared_key` itself, or (under [`osr_route_own_refusal_key_enabled`], for the
/// optimizing OSR route) `shared_key` re-hashed with a route tag.
pub(crate) fn ir_refusal_memo_route_key(shared_key: u64, osr_route: bool) -> u64 {
    if !osr_route || !osr_route_own_refusal_key_enabled() {
        return shared_key;
    }
    let mut h = FxHasher::default();
    h.write_u64(shared_key);
    h.write_u64(0x4F53_525F_524F_5554); // "OSR_ROUT"
    h.finish()
}



/// JIT round 18 wave 9 (lane osrpoly); since round 18 wave 11 (lane osrfollow)
/// only the cheap SUPERSET prefilter of [`osr_door_single_pass_polymorphic_splice_pc`],
/// which asks the single-pass planner itself. The first in-loop site of the
/// compiling method at which the single-pass OSR door plans a POLYMORPHIC
/// guarded splice ([`plan_osr_door_guarded_splices`]'s polymorphic arm) that
/// the optimizing tier would not, or `None`.
///
/// Mirrors the single-pass door's gates: no precise exception frames, the
/// guarded and polymorphic OSR-door switches, an `invokevirtual` /
/// `invokeinterface` of the method's own code inside a loop, and a receiver
/// profile [`ir::profile_polymorphic_receiver_classes`] splits over three or
/// four classes (the same reader the single-pass arm asks). With a receiver
/// resolver, at least two of those classes must resolve to a guarded body, as
/// either planner needs. `None` whenever the optimizing tier's own
/// polymorphic splice is on (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`):
/// that body then carries the splice itself and competes on evidence.
pub(crate) fn osr_door_lost_polymorphic_splice_pc(
    code: &[u8],
    code_len: usize,
    invoke_ops: &[(usize, u16, u8)],
    precise_exception_frames: bool,
    profile: Option<&profile::MethodProfile>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    receiver_resolve: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
) -> Option<usize> {
    if precise_exception_frames
        || !osr_door_guarded_splices_enabled()
        || ir::ir_polymorphic_guarded_splice_enabled()
        // JIT round 19 wave 1 (lane osrpoly19): the door-scoped form of the
        // same switch; this question is asked only for a door compile.
        || ir::osr_door_ir_polymorphic_splice_enabled()
    {
        return None;
    }
    if profile.is_none() {
        return None;
    }
    let len = code_len.min(code.len());
    for &(pc, cp_idx, opcode) in invoke_ops {
        if pc >= len
            || !matches!(opcode, 0xb6 | 0xb9)
            || code.get(pc).copied() != Some(opcode)
            || !bytecode_pc_in_loop(code, code_len, pc)
        {
            continue;
        }
        let Some(classes) = ir::profile_polymorphic_receiver_classes(profile, pc) else {
            continue;
        };
        if let Some(resolve) = receiver_resolve {
            let Some((cn, mn, desc)) = cp_invoke_resolver.and_then(|r| r(cp_idx)) else {
                continue;
            };
            let resolved = classes
                .iter()
                .take_while(|&&cid| {
                    ir_profile_guarded_site(Some(resolve), cid, &cn, &mn, &desc).is_some()
                })
                .count();
            if resolved < 2 {
                continue;
            }
        }
        return Some(pc);
    }
    None
}

/// JIT round 18 wave 11 (lane osrfollow): `CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`'s
/// one decision (OSRPOLY18-4 moved it before the compile; OSRFOLLOW18-4,
/// `r18-osr-prefer-spliced-predicate-skips-the-single-pass-planner-gates-FIXED-20261006.md`,
/// made it the single-pass planner's own answer). `Some(pc)`: the first site
/// the single-pass OSR door's planner would splice through its POLYMORPHIC arm
/// ([`OsrSplicePlan::polymorphic_pcs`]), so the VM's optimizing OSR door
/// (`jit_bridge.rs` `osr_optimizing_build_declined`) declines to compile and
/// the spliced single-pass OSR body runs the loop. `None` with the switch off,
/// with the optimizing tier's own polymorphic splice on
/// (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`: that body splices the site
/// itself and competes on evidence), with the OSR-door splices off, or when
/// the planner splices no site polymorphically.
///
/// The answer IS the planner's: [`plan_osr_door_splices`] over
/// `static_candidates` (whose cost the guarded pass's budget is charged with),
/// then [`plan_osr_door_guarded_splices`] over `guarded_candidates`, with the
/// single-pass door's precise-frame rule (`!exception_table.is_empty()`), its
/// entry pc (hot pricing, `FreqInlineSize`), its resolvers and `plan_inline`'s
/// size tiers, budget, protected-range, recursion and force-interpret
/// refusals. So a site whose bodies the single-pass door would refuse no
/// longer declines the optimizing body. [`osr_door_lost_polymorphic_splice_pc`]
/// (profile and loop only, a superset) runs first and skips the planning for
/// every method with no candidate site. Pure apart from the planners'
/// `CRATONVM_DBG_JITC` lines.
#[allow(clippy::too_many_arguments)]
pub fn osr_door_single_pass_polymorphic_splice_pc(
    code: &[u8],
    code_len: usize,
    enclosing: (&str, &str, &str),
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    profile: Option<&profile::MethodProfile>,
    osr_entry_pc: Option<usize>,
    static_candidates: &[OsrSpliceCandidate<'_>],
    guarded_candidates: &[OsrSpliceCandidate<'_>],
    static_resolve: &dyn Fn(&str, &str, &str) -> Option<InlineSite>,
    class_namer: &dyn Fn(u32) -> Option<String>,
    receiver_resolve: &dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>,
) -> Option<usize> {
    if !osr_door_prefer_spliced_single_pass_enabled()
        || !osr_door_splices_enabled()
        || guarded_candidates.is_empty()
        || profile.is_none()
    {
        return None;
    }
    let precise = !exception_table.is_empty();
    // The cheap superset first: an own virtual / interface site in a loop with
    // a three-or-four-class profile (no resolver: the profile alone).
    let len = code_len.min(code.len());
    let ops: Vec<(usize, u16, u8)> = guarded_candidates
        .iter()
        .filter_map(|c| {
            let op = code.get(c.pc).copied().filter(|_| c.pc < len)?;
            matches!(op, 0xb6 | 0xb9).then_some((c.pc, 0u16, op))
        })
        .collect();
    osr_door_lost_polymorphic_splice_pc(code, code_len, &ops, precise, profile, None, None)?;
    let mut plan = plan_osr_door_splices(
        code,
        code_len,
        enclosing,
        exception_table,
        precise,
        profile,
        osr_entry_pc,
        static_candidates,
        static_resolve,
    );
    plan_osr_door_guarded_splices(
        &mut plan,
        code,
        code_len,
        enclosing,
        exception_table,
        precise,
        profile,
        osr_entry_pc,
        guarded_candidates,
        class_namer,
        receiver_resolve,
    );
    // JIT round 19 wave 2 (lane osr2, OSR19-1): only a site in the loop the
    // door is entered at declines that entry; the VM memoises the answer per
    // header under the same switch.
    if let Some(entry_pc) = osr_entry_pc.filter(|_| osr_poly_decline_per_header_enabled()) {
        return osr_door_first_pc_in_entry_loop(code, code_len, entry_pc, &plan.polymorphic_pcs);
    }
    plan.polymorphic_pcs.first().copied()
}

/// JIT round 19 proposal wave 2 (lane osr2, OSR19-1 of
/// `jit-r19-osr19-proposals.md`): `CRATONVM_JIT_OSR_POLY_DECLINE_PER_HEADER=1`.
/// The polymorphic-loop decline of `CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`
/// becomes a verdict about ONE loop header instead of the method:
///
/// * [`osr_door_single_pass_polymorphic_splice_pc`] answers only a
///   polymorphic site inside the loop(s) the entry pc names
///   ([`osr_door_first_pc_in_entry_loop`]), so a method whose first OSR
///   trigger fires in a loop WITHOUT the site builds its optimizing body;
/// * the VM (`jit_bridge.rs` `osr_optimizing_build_declined`) memoises a
///   positive decline under the entry's own `(method, pc)` key, not the
///   method-wide one, so a trigger in another loop asks again;
/// * a body remembered under a header it was NOT built for (the build's
///   "every other header" loop) re-asks the decline once, at that header's
///   first lookup (`osr_optimizing_cached`), with the profile as it is then,
///   which is the answer a first trigger at that header would have got.
///
/// Why: with the method-wide memo the body choice depended on which loop
/// triggered first and on the profile at that moment (OSR19-1). Both bodies
/// are correct; this only chooses between them. **default on**. Read live
/// (no `static`), once per decline question.
pub fn osr_poly_decline_per_header_enabled() -> bool {
    true
}

/// The first of `pcs` that lies inside a loop an OSR entry at `entry_pc`
/// enters ([`osr_entry_loop_ranges`]: the backward branches whose header or
/// own pc is `entry_pc`, else the innermost one containing it), nested loops
/// included (a range is `[header, back edge]`, which contains every inner
/// loop's code). `None` when `entry_pc` is in no loop or no pc is inside.
pub(crate) fn osr_door_first_pc_in_entry_loop(
    code: &[u8],
    code_len: usize,
    entry_pc: usize,
    pcs: &[usize],
) -> Option<usize> {
    let ranges = osr_entry_loop_ranges(code, code_len, entry_pc);
    pcs.iter().copied().find(|&pc| {
        ranges
            .iter()
            .any(|&(header, back)| header <= pc && pc <= back)
    })
}

/// JIT round 19 proposal wave 3 (lane megaosr, MEGAOSR19-1 of
/// `jit-r19-megaosr-proposals.md`; page
/// `r19-optimizing-osr-body-loses-5x-at-blind-megamorphic-sites-CLOSED-20261012.md`):
/// `CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_SPLICE=1`. The optimizing OSR door
/// declines to compile a loop whose statically bound callee carries a
/// virtual / interface site that the callee's OWN receiver profile calls
/// megamorphic ([`osr_door_blind_megamorphic_splice_site`]), so the single-pass
/// OSR body runs that loop, as under `CRATONVM_JIT_OSR_OPTIMIZING=0`.
///
/// Why: spliced into the OSR body, such a site is BLIND. The optimizing
/// planner gives every surviving virtual call of a spliced body an inline cache
/// with no profile seed (`try_compile_inner`, the `sub_vcalls` loop: "receiver
/// counts are recorded against the bci of the method that was EXECUTING, and a
/// relocated pc is not one of those"), so neither the dominant-receiver MIC seed
/// nor the megamorphic seed (`JitPICSlot::seed_megamorphic_from_profile`) that
/// the callee's own compile gives the same site reaches it, and no guarded
/// splice is planned for it either. `R16MegaBlindSites` (`run`'s loop calls
/// `site`, whose one `invokeinterface` sees 24 classes) measured the optimizing
/// OSR body 2-5x slower than the single-pass one; `R16MegaOneSite`, whose
/// megamorphic sites are the loop method's OWN (profiled, seeded), does not.
///
/// It only chooses between two correct bodies, like the polymorphic-loop
/// decline beside it. **Default ON** since the round-19 proposal round
/// (R16MegaBlindSites warm 290 -> 52 ms; `=0` restores the optimizing body).
/// Read live (no `static`), once per optimizing OSR build verdict.
pub fn osr_decline_blind_mega_splice_enabled() -> bool {
    true
}

/// JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2 of
/// `jit-r19-megaosr-proposals.md`): `CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED=1`.
/// The planner-side fix for the blind site MEGAOSR19-1 declines around: a
/// virtual / interface call that survives inside a spliced body gets its
/// inline cache seeded from the CALLEE's own receiver profile at the callee's
/// bci (`InlineSite::callee_receiver_profile`, filled by the VM resolver under
/// this switch), exactly as the caller's own sites are seeded from the
/// caller's profile: the dominant receiver's class id in the MIC
/// (`JitMICSlot::prepopulate`, 80 % bar) and the megamorphic seed
/// (`JitPICSlot::seed_megamorphic_from_profile`, M13-9), so a site the callee
/// already calls megamorphic starts on the cell-first path and
/// `CRATONVM_JIT_MEGA_SEEDED_SKIP_INLINE` (MEGA19-4) applies to it too.
///
/// A seed only chooses which complete cache path runs (no entry is baked: the
/// first dispatch still rings the helper), so answers cannot change. **Default
/// OFF**. Read live (no `static`): once per resolved body (VM) and once per
/// planned site (here).
pub fn ir_splice_callee_profile_seed_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT` (JIT round 24 item B3 of
/// `jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`; perf, **default OFF**,
/// opt-in): a callee whose body makes a `MemorySegment` element access
/// (`getAtIndex` / `setAtIndex` / `get` / `set` of a value layout) may be
/// spliced by the optimizing tier, and the access is expanded inline inside
/// the splice from the CALLEE's own receiver profile
/// ([`plan_spliced_ffm_element_sites`]) -- the root-site expansion of JIT round
/// 23, with its guards resuming through the splice's chain frame. Without it
/// the VM's resolver refuses such a body (`ffm-accessor-keeps-its-own-fast-path`)
/// and the accessor loop stays a call: about 20 ns per call against HotSpot's
/// 0.7 (`R23SegMono heapGet` / `nativeGet` from a driver lambda). The VM reads
/// it too (the resolver's refusal and the callee receiver rows). Read per
/// compile.
///
/// Measured at the round-24 close (host 64.177.43.150): it engages (`PLANNED in
/// splice` for every driver lambda and for `order`), but `R23SegMono`'s rows do
/// not move (heapget 87.8 vs about 80 ns/op default) -- the call that costs is
/// the shared `time` driver's megamorphic interface call into each lambda,
/// which no splice removes -- and the warm-order rows regress badly
/// (order-zero-n64 36 us vs 150 ns): where the in-splice expansion declines,
/// the kernel is a call inside the splice instead of the separately compiled
/// body with its own expansion. It stays opt-in.
pub fn ir_splice_ffm_element_enabled() -> bool {
    false
}

/// JIT round 24 item B3 ([`ir_splice_ffm_element_enabled`]): the element
/// access plans of the spliced bodies' surviving `MemorySegment` accessor
/// calls, keyed by combined pc, and the layout facts of the spliced bodies'
/// reference `getstatic`s (the constant layouts those calls take).
///
/// A site is planned only from evidence: the callee's own receiver row at that
/// call (`IrInlineTables::spliced_receiver_seeds`, the CALLEE's profile at its
/// own bci) naming one segment class for the guarded-cache share, and a
/// segment shape the VM answers for that class. Every other site keeps the
/// call row the splice left (`bail_on_decline` is `false`: a spliced site
/// never refuses the method).
///
/// j27 wave 4 (lane SPLICEFFM): the evidence test is
/// `ffm_element_inline::spliced_ffm_element_admission`, the one the VM's
/// resolver asks before it lifts `ffm-accessor-keeps-its-own-fast-path`, so a
/// call the resolver admitted is planned here and one it did not admit was
/// never spliced. And a site whose call site in `method_key` (the compiling
/// method) the VM withdrew is not planned again
/// (`spliced_ffm_call_site_withdrawn`), as the root planner's
/// `ffm_element_guards_withdrawn` keeps a withdrawn root site from re-planting.
///
/// j27 wave 4 (lane SPLICEFFM2): the admission is asked with the callee body
/// holding the call (its bytes in `combined`) and this compile's static-field
/// rows, so the layout operand's `getstatic` and its filed facts are the
/// resolver's inputs too. A site the build still declines is the re-plan's
/// (`ffm_element_inline::spliced_ffm_declined_sites`).
pub(crate) fn plan_spliced_ffm_element_sites(
    tables: &ir::IrInlineTables,
    combined: &[u8],
    direct_helpers: &DirectHelperTable,
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
) -> (
    HashMap<usize, ffm_element_inline::FfmElementInlineSite>,
    Vec<((u32, u32), ffm_element_inline::FfmLayoutFacts)>,
) {
    let frames = ir::IrInlineFrameSites::from_sites(&tables.sites);
    let mut sites = HashMap::new();
    for (&pc, &(info_ptr, _, _)) in &tables.invoke_info {
        if info_ptr == 0 {
            continue;
        }
        let Some((base, body)) = spliced_body_holding(tables, combined, pc) else {
            continue;
        };
        // SAFETY: a non-zero `info_ptr` in `invoke_info` addresses a
        // `JitInvokeInfo` this compile interned (`owned_invoke_infos`), alive
        // for the compile and baked into the artifact.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let call = ffm_element_inline::SplicedFfmCall {
            target: (info.class_name, info.method_name, info.descriptor),
            code: body,
            pc: pc - base,
        };
        let Some(site) = ffm_element_inline::plan_spliced_ffm_element_site(
            direct_helpers,
            call,
            tables.spliced_receiver_seeds.get(&pc),
            |at| {
                let &(holder, index, _, _) = tables.static_field_info.get(&(base + at))?;
                Some((holder, index))
            },
        ) else {
            continue;
        };
        let call_site = frames.enclosing_bci_at(pc);
        if ffm_element_inline::spliced_ffm_call_site_withdrawn(despec, method_key, call_site) {
            if x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-ffm-element-inline WITHDRAWN in splice at combined pc={pc} \
                     (call site {call_site:?}) in {method_key}: the call site was de-speculated"
                );
            }
            continue;
        }
        sites.insert(pc, site);
    }
    let mut facts = Vec::new();
    if !sites.is_empty() {
        for &(class_id, field_index, type_tag, _) in tables.static_field_info.values() {
            if type_tag != b'L' {
                continue;
            }
            let Ok(fi) = u32::try_from(field_index) else {
                continue;
            };
            if let Some(f) = ffm_element_inline::ffm_layout_facts(direct_helpers, class_id, field_index) {
                facts.push(((class_id, fi), f));
            }
        }
    }
    (sites, facts)
}

/// j27 wave 4 (lane SPLICEFFM2): the spliced body whose OWN code holds the
/// combined pc `pc` (a nested body's bytes are appended after its parent's,
/// so the regions are disjoint): its base and its bytes in `combined`.
pub(crate) fn spliced_body_holding<'c>(
    tables: &ir::IrInlineTables,
    combined: &'c [u8],
    pc: usize,
) -> Option<(usize, &'c [u8])> {
    let site = tables
        .sites
        .values()
        .find(|s| s.base <= pc && pc < s.base.saturating_add(s.code_len))?;
    Some((site.base, combined.get(site.base..site.base + site.code_len)?))
}

/// j27 wave 4 (lane SPLICEFFM2): `ir_tier`'s one re-plan after a build
/// declined element calls inside the splices at `sites`: those caller pcs
/// join `skip` (each once), so the next attempt leaves each such callee a
/// call into its own compiled body. They count against the W5-2 rebuild
/// budget (`IR_SPLICE_REBUILD_MAX` reads `skip.len()`), which only ever
/// stops a later rebuild early.
pub(crate) fn ffm_splice_rebuild_skip(skip: &mut Vec<usize>, sites: Vec<usize>) {
    for site in sites {
        if !skip.contains(&site) {
            skip.push(site);
        }
    }
}


/// j27 wave 4 (lane SPLICEFFM2): every `Op::Call` the build left in `graph`,
/// with its (combined) pc and its target, for
/// `ffm_element_inline::spliced_ffm_declined_sites`.
pub(crate) fn ir_graph_call_targets(graph: &ir::Graph) -> Vec<(usize, (&'static str, &'static str, &'static str))> {
    graph
        .nodes
        .iter()
        .filter_map(|node| {
            let ir::Op::Call { info_ptr } = node.op else {
                return None;
            };
            let pc = node.bytecode_pc?;
            if info_ptr == 0 {
                return None;
            }
            // SAFETY: the builder takes only this compile's owned invoke rows
            // (`owned_invoke_infos`), alive for the compile.
            let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
            Some((pc, (info.class_name, info.method_name, info.descriptor)))
        })
        .collect()
}

