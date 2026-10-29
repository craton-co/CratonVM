// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ---------------------------------------------------------------------------
// The direct lane of the `invoke` / `invokeExact` doors (round 13 wave 10,
// lane callcost4; `r12w7-mh5-invoke-door-rows-time-is-mostly-throws-CLOSED-20261006.md`)
// ---------------------------------------------------------------------------
//
// `R13Irexc2MhFloor` measured a ~5 us floor on EVERY door shape, `exact-0`
// (`invokeExact ()int` on a `findStatic` handle) included, against ~0.13 us for
// a plain registered native. For a DIRECT handle called at its own exact type
// most of the door is checks that cannot fire and conversions that are the
// identity: the declared type rendered twice more (`direct_return_boxes`,
// `invoke_exact_inner_cast_refusal`), `MH_DESC` decoded three times, the
// argument list copied four times (`to_vec`, `collect_trailing_varargs`,
// `adapt_leaf_args` with its pins), and a primitive result boxed through
// `valueOf` only for the caller to unbox it again (`unbox_poly_return_checked`:
// a class-manager read and a name `String` per call).
//
// The lane takes a call only when the ordinary door provably does nothing but
// the leaf call: a `MH_KIND_STATIC` handle with no bound argument, or a
// `MH_KIND_VIRTUAL` handle (bound to its receiver or not), with no marking bit
// (no collector, no explicit-cast stamp, no cast chain, no exact invoker) and
// no `specialCaller`, whose declared `type` IS its own member type (receiver
// prepended for an unbound virtual one) and IS the call site's
// descriptor, called with raw primitives of the parameter's own `Value`
// variant, references at reference parameters and no array parameter (the
// varargs repair's territory). Every refusal the ordinary door could still
// raise for such a call is asked here too, by the SAME functions and in the
// SAME order, and returned as the door returns it; everything that could not
// fire is skipped. A shape it does not model is declined before anything is
// read that could have an effect, and the ordinary door runs as before.

/// `CRATONVM_MH_DIRECT_LANE` -- default ON (round 13 wave 10, lane
/// callcost4); `0`/`false`/`off`/`no` sends every `invoke` / `invokeExact`
/// through the ordinary doors again. Performance only, both modes. Read once.
pub(crate) fn mh_direct_lane_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DIRECT_LANE"))
}

/// `CRATONVM_MH_DIRECT_LANE_RAW_RETURN` -- default ON (round 13 wave 10, lane
/// callcost4); `0`/`false`/`off`/`no` makes the direct lane box a primitive
/// result as the ordinary doors do. With it on, a primitive of the call
/// site's own return type is handed back raw: every by-name route that reaches
/// these natives (the interpreter's stackless door, `invoke_or_native`'s
/// signature-polymorphic block, the JIT's native site cache) passes the result
/// through `unbox_poly_return_checked`, which keeps a raw primitive as is and
/// unboxes the box the door would have made to the same `Value`. Read once.
pub(crate) fn mh_direct_lane_raw_return_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DIRECT_LANE_RAW_RETURN")
    })
}

/// `CRATONVM_MH_DIRECT_LANE_RECORD` -- default ON (round 15 wave 1, lane mh;
/// proposal C14-1 of `jit-r14-calls-proposals-RETIRED-20260930.md`); `0`/`false`/`off`/`no`
/// makes [`mh_direct_lane_shape`] decode the handle on every call again
/// (three Java `String` decodes, the descriptor split and the declared `type`
/// render), and the `invoke` door's refusals re-read `MH_DESC` / `MH_CLASS`.
/// With it on, a per-thread memo ([`DIRECT_LANE_MEMO`]) serves the decoded
/// [`DirectLaneRecord`]. Performance only, both modes. Read once.
pub(crate) fn mh_direct_lane_record_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DIRECT_LANE_RECORD")
    })
}

/// `CRATONVM_MH_DIRECT_LANE_LEAN_VIRTUAL` -- default ON (round 15 wave 1, lane
/// mh); `0`/`false`/`off`/`no` restores the virtual arm of [`mh_direct_lane`]
/// as round 13 wrote it (a pin per reference argument and two argument
/// `Vec`s). With it on the arm pins only the receiver: nothing between the
/// lane's entry and the leaf call can collect, and `invoke_virtual_declared`
/// pins the receiver and every object argument itself for the call's
/// duration, so the argument pins protected nothing. The receiver pin is
/// re-read AFTER the call for [`neutralize_missing_serialization_hook`]'s
/// `readResolve` / `writeReplace` answer, which the old arm took from its
/// pre-call copy (a stale address after a collection inside the call).
/// Read once.
pub(crate) fn mh_direct_lane_lean_virtual_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DIRECT_LANE_LEAN_VIRTUAL")
    })
}

/// `CRATONVM_MH_DIRECT_LANE_INVOKE_REFUSAL_SKIP` -- default ON (round 15 wave
/// 4, lane mh2; proposal MH15-1 of `jit-r15-mh-proposals-RETIRED-20261003.md`);
/// `0`/`false`/`off`/`no` makes the direct lane's `invoke` door ask
/// [`invoke_reference_cast_refusal_decoded`] on every call again. `true` only
/// while `CRATONVM_MH_INVOKE_DECLARED_CAST` and
/// `CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED` are on as well (their
/// defaults): the argument on [`mh_direct_lane`] that the refusal is provably
/// `None` at the exact type rests on both. Performance only, both modes.
/// Read once.
pub(crate) fn mh_direct_lane_invoke_refusal_skip_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_MH_DIRECT_LANE_INVOKE_REFUSAL_SKIP",
        ) && invoke_declared_cast_enabled()
            && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED",
            )
    })
}

/// What the direct lane derives from a handle's `MH_CLASS`, `MH_NAME`,
/// `MH_DESC` and `type` objects (round 15 wave 1, lane mh; proposal C14-1).
/// A pure function of those four objects (and of whether the handle is an
/// UNBOUND virtual one): Java `String`s are immutable, and a `MethodType`'s
/// `rtype` / `ptypes` are never rewritten after it is built.
pub(crate) struct DirectLaneRecord {
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    pub(crate) params: Vec<String>,
    /// The handle's own member type: `desc`, with `L<class>;` prepended for an
    /// UNBOUND virtual handle (the caller supplies its receiver).
    pub(crate) own_type: String,
    /// The `type` field, rendered (`methodtype_to_descriptor`), when it reads
    /// (round 16, lane mh: the converting lane, [`mh_converting_lane`], hands
    /// it to the door's own refusals and conversions).
    pub(crate) declared: Option<String>,
    /// `mh_declared_descriptor` (the `type` field, rendered) is `own_type`.
    pub(crate) declared_is_own_type: bool,
    /// `own_type` splits into exactly `L<class>;` (an unbound virtual handle
    /// only) followed by `params`, so a call site that IS `own_type` names
    /// every receiver and parameter position with the very token the
    /// `invoke` door's refusal judges it against (round 15 wave 4, lane mh2;
    /// MH15-1, [`mh_direct_lane_invoke_refusal_skip_enabled`]). A class name
    /// no JVM mints (one holding `;`) misaligns the split and reads `false`.
    pub(crate) own_type_aligned: bool,
}

/// Decode a [`DirectLaneRecord`] from the four field values (as read by
/// `get_field(MH_CLASS / MH_NAME / MH_DESC)` and `get_field_by_name("type")`),
/// or `None` for a handle the lane does not model. With `site`, a site that is
/// not the own type declines before the declared type is rendered (the
/// round-13 order, kept for the switched-off path). Pure reads.
pub(crate) fn mh_direct_lane_decode(
    ctx: &mut dyn NativeContext,
    fields: [Value; 4],
    unbound_virtual: bool,
    site: Option<&str>,
) -> Option<DirectLaneRecord> {
    // `mh_read_desc` / `mh_read_class` / `mh_read_name`, on a value already read.
    fn read(ctx: &dyn NativeContext, v: Value) -> Option<String> {
        match v {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
    }
    let [class_v, name_v, desc_v, type_v] = fields;
    let desc = read(&*ctx, desc_v)?;
    let class = read(&*ctx, class_v)?;
    let name = read(&*ctx, name_v)?;
    if class.is_empty() || class.starts_with('[') || name.is_empty() {
        return None;
    }
    let (params, _) = split_descriptor_params(&desc)?;
    // The caller supplies the receiver of an UNBOUND virtual handle, and its
    // own type names it; a bound handle's type is the leaf descriptor.
    let own_type = if unbound_virtual {
        format!("(L{class};{}", &desc[1..])
    } else {
        desc.clone()
    };
    if site.is_some_and(|site| site != own_type.as_str()) {
        return None;
    }
    let declared = match type_v {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt),
        _ => None,
    };
    let declared_is_own_type = declared.as_deref() == Some(own_type.as_str());
    let own_type_aligned = split_descriptor_params(&own_type).is_some_and(|(own, _)| {
        let skip = usize::from(unbound_virtual);
        own.len() == params.len() + skip
            && (!unbound_virtual || own[0] == format!("L{class};"))
            && own[skip..] == params[..]
    });
    Some(DirectLaneRecord {
        class,
        name,
        desc,
        params,
        own_type,
        declared,
        declared_is_own_type,
        own_type_aligned,
    })
}

/// One direct-mapped line of [`DIRECT_LANE_MEMO`]: the VM, the collection
/// epoch and the ADDRESSES of the four objects a [`DirectLaneRecord`] was
/// decoded from, and the record.
pub(crate) struct DirectLaneMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) refs: [usize; 4],
    pub(crate) unbound_virtual: bool,
    pub(crate) record: Option<std::rc::Rc<DirectLaneRecord>>,
}

impl DirectLaneMemoLine {
    /// A line no lookup matches (`vm` 0 is never memoised).
    pub(crate) fn empty() -> Self {
        DirectLaneMemoLine {
            vm: 0,
            epoch: 0,
            refs: [0; 4],
            unbound_virtual: false,
            record: None,
        }
    }
}

/// Slots in [`DIRECT_LANE_MEMO`]; a power of two (the index is a mask).
pub(crate) const DIRECT_LANE_MEMO_SLOTS: usize = 16;

thread_local! {
    /// Per-thread memo of [`DirectLaneRecord`]s (round 15 wave 1, lane mh;
    /// proposal C14-1), indexed by the handle's address.
    ///
    /// A hit needs the same VM, the same collection count and the same four
    /// field objects at the same addresses. Nothing moves or is freed within
    /// one collection epoch (the rule `date_format_fast::zone_rules_for_id`
    /// memoises on, gc-common w34-a), so an equal address in an equal epoch is
    /// the same object, and the record is a pure function of those objects.
    /// The HANDLE's identity is never trusted: `type` is restamped IN PLACE on
    /// some paths (`mh_with_stamped_type` on a handle too narrow to copy, the
    /// `set_field_by_name(.., "type", ..)` sites after a mint) and a few rewrite
    /// `MH_DESC` / `MH_NAME`; each such rewrite names another object and
    /// misses. For an equal address to name a DIFFERENT object the first one
    /// must have died, which needs a collection, which moves the count. A
    /// context that reports VM identity `0` (the mocks, any wrapper that does
    /// not forward it) is never memoised.
    pub(crate) static DIRECT_LANE_MEMO: std::cell::RefCell<Vec<DirectLaneMemoLine>> =
        std::cell::RefCell::new(
            (0..DIRECT_LANE_MEMO_SLOTS).map(|_| DirectLaneMemoLine::empty()).collect(),
        );
}

/// The [`DirectLaneRecord`] of `this`, from [`DIRECT_LANE_MEMO`] or decoded
/// (and memoised). Pure reads; no GC point.
pub(crate) fn mh_direct_lane_record_memoised(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    unbound_virtual: bool,
) -> Option<std::rc::Rc<DirectLaneRecord>> {
    let fields = [
        ctx.get_field(this, MH_CLASS),
        ctx.get_field(this, MH_NAME),
        ctx.get_field(this, MH_DESC),
        ctx.get_field_by_name(this, "type"),
    ];
    let vm = ctx.vm_identity();
    if vm == 0 {
        return mh_direct_lane_decode(ctx, fields, unbound_virtual, None).map(std::rc::Rc::new);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = |v: &Value| match v {
        Value::Object(Some(o)) => o.as_ptr() as usize,
        _ => 0,
    };
    let refs = [
        addr(&fields[0]),
        addr(&fields[1]),
        addr(&fields[2]),
        addr(&fields[3]),
    ];
    let slot = ((this.as_ptr() as usize) >> 3) & (DIRECT_LANE_MEMO_SLOTS - 1);
    let hit = DIRECT_LANE_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm
            && line.epoch == epoch
            && line.refs == refs
            && line.unbound_virtual == unbound_virtual
        {
            line.record.clone()
        } else {
            None
        }
    });
    if hit.is_some() {
        return hit;
    }
    let record = std::rc::Rc::new(mh_direct_lane_decode(ctx, fields, unbound_virtual, None)?);
    DIRECT_LANE_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = DirectLaneMemoLine {
            vm,
            epoch,
            refs,
            unbound_virtual,
            record: Some(record.clone()),
        };
    });
    Some(record)
}

/// Which door a direct-lane call arrived through: `invoke` asks its own
/// argument refusals ([`invoke_reference_cast_refusal_with`], and for a static
/// handle [`mh_entry_adapt`]'s entry cast) before the leaf's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectLaneDoor {
    InvokeExact,
    Invoke,
}

/// What [`mh_direct_lane_shape`] read off an admitted handle.
pub(crate) struct DirectLaneShape {
    pub(crate) kind: i32,
    pub(crate) bound: Value,
    /// `class`, `name`, `desc` and `params` (round 15 wave 1: memoised,
    /// [`mh_direct_lane_record_enabled`]).
    pub(crate) record: std::rc::Rc<DirectLaneRecord>,
}

/// [`MH_EXPLICIT_SCOPE`] cleared for the extent of a direct-lane dispatch and
/// restored on every exit, as [`mh_dispatch_from_door`] does for the ordinary
/// one.
pub(crate) struct ExplicitScopeCleared(bool);

impl ExplicitScopeCleared {
    pub(crate) fn new() -> Self {
        Self(MH_EXPLICIT_SCOPE.with(|f| f.replace(false)))
    }
}

impl Drop for ExplicitScopeCleared {
    fn drop(&mut self) {
        let saved = self.0;
        MH_EXPLICIT_SCOPE.with(|f| f.set(saved));
    }
}

/// The admission of the direct lane: pure reads, no allocation, no GC point,
/// nothing a later ordinary door could observe. `None` declines.
pub(crate) fn mh_direct_lane_shape(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    site: &str,
) -> Option<DirectLaneShape> {
    // The dispatch diagnostics are the ordinary door's; keep them whole.
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // Width-guarded: a real JDK handle has no `MH_KIND` slot (wave 56, lane
    // L4: the unguarded read was a heap OOB warning per call).
    let kind = match mh_kind_width_guarded(&*ctx, this) {
        k if k == MH_KIND_STATIC || k == MH_KIND_VIRTUAL => k,
        _ => return None,
    };
    if mh_marking_bits(&*ctx, this) & MH_MARKS_DIVERT_DIRECT_LANE != 0 {
        return None;
    }
    // A bound value is a VIRTUAL handle's receiver (`bindTo`); a static
    // handle's leading argument is the repair paths' to place.
    let bound = ctx.get_field(this, MH_BOUND);
    let bound_receiver = matches!(bound, Value::Object(Some(_)));
    if bound_receiver && kind != MH_KIND_VIRTUAL {
        return None;
    }
    if crate::panama::is_downcall_handle(&*ctx, this) {
        return None;
    }
    // Never armed by these two doors; a stale entry is the ordinary door's to
    // take (`mh_dispatch_body` consumes it).
    if MH_ENTRY_CALL_SITE.with(|f| f.borrow().is_some()) {
        return None;
    }
    // The caller supplies the receiver of an UNBOUND virtual handle, and its
    // own type names it; a bound handle's type is the leaf descriptor.
    let unbound_virtual = kind == MH_KIND_VIRTUAL && !bound_receiver;
    if kind == MH_KIND_VIRTUAL
        && ctx.object_num_fields(this) > MH_SPECIAL_CALLER
        && !matches!(ctx.get_field(this, MH_SPECIAL_CALLER), Value::Object(None))
    {
        // A `specialCaller` narrows the leaf's receiver cast
        // (`mh_leaf_receiver_class`); only `findSpecial` records one.
        return None;
    }
    // `MH_DESC` / `MH_CLASS` / `MH_NAME` decoded, split, the own type built
    // and the declared `type` rendered: once per handle from the memo, or on
    // every call (round 13's order: the site is compared before the render).
    let record = if mh_direct_lane_record_enabled() {
        mh_direct_lane_record_memoised(ctx, this, unbound_virtual)?
    } else {
        let fields = [
            ctx.get_field(this, MH_CLASS),
            ctx.get_field(this, MH_NAME),
            ctx.get_field(this, MH_DESC),
            ctx.get_field_by_name(this, "type"),
        ];
        std::rc::Rc::new(mh_direct_lane_decode(
            ctx,
            fields,
            unbound_virtual,
            Some(site),
        )?)
    };
    // An unbound `VarHandle` access mode invoker judges its receiver's access
    // mode type first (`vh_invoker_call_refusal`, interpreter round i1 wave
    // 50): the ordinary door.
    if !bound_receiver && record.class == VAR_HANDLE_CLASS {
        return None;
    }
    if site != record.own_type.as_str() || !record.declared_is_own_type {
        return None;
    }
    // JIT round 17 wave 2 (lane mh2): equal strings over a name two loaders
    // define may be a `WrongMethodTypeException`; the ordinary door judges it
    // (`exact_call_site_refusal` -> `mh_exact_loader_refusal`).
    if mh_site_names_ambiguous_class(&*ctx, site) {
        return None;
    }
    let params = &record.params;
    let args = if unbound_virtual {
        match extra.first() {
            Some(Value::Object(Some(_))) => &extra[1..],
            _ => return None,
        }
    } else {
        extra
    };
    if args.len() != params.len() {
        return None;
    }
    let fits = args
        .iter()
        .zip(params.iter())
        .all(|(v, p)| match p.as_bytes().first() {
            Some(b'I' | b'S' | b'B' | b'C' | b'Z') => matches!(v, Value::Int(_)),
            Some(b'J') => matches!(v, Value::Long(_)),
            Some(b'F') => matches!(v, Value::Float(_)),
            Some(b'D') => matches!(v, Value::Double(_)),
            Some(b'L') => matches!(v, Value::Object(_)),
            _ => false,
        });
    if !fits {
        return None;
    }
    Some(DirectLaneShape {
        kind,
        bound,
        record,
    })
}

/// The direct lane: `None` when the call is not one it models (the caller then
/// runs the ordinary door, which nothing here has disturbed), otherwise
/// exactly what the ordinary door would have answered, a primitive result
/// possibly unboxed ([`mh_direct_lane_raw_return_enabled`]).
///
/// Equivalence, step by step, for an admitted call (`declared` = own type =
/// call site; `tail_ok` is `true`, as both registrations pass it):
/// * `exact_call_site_refusal` / `invoke_narrowing_arg_refusal`: the site IS
///   the declared type, so neither refuses;
/// * `null_receiver_refusal`: the receiver is not null;
/// * `invoke_exact_inner_cast_refusal`: the declared parameters ARE the leaf's
///   (no `specialCaller`), so nothing is judged;
/// * `invoke_reference_cast_refusal_with` (`invoke` only): asked, as the door
///   asks it -- or, since round 15 wave 4 (MH15-1), skipped where it is
///   provably `None` (the argument is at the skip below);
/// * `adapt_invoke_args` / `collect_trailing_varargs` / `adapt_leaf_arg`: the
///   identity for raw primitives of their own variant, references at
///   reference parameters and no array parameter;
/// * `mh_entry_adapt` (`invoke` on a static handle: its entry cast),
///   `leaf_receiver_refusal` and `leaf_reference_refusal`: asked, in the
///   ordinary order, with the explicit scope cleared as the door clears it;
/// * `direct_return_boxes`: `false` (the declared return is the leaf's);
/// * `return_cast_refusal` / `return_cast_may_apply`: no inner and no outer
///   target (declared, leaf and site returns agree), so the tail token is the
///   site's return, as the door computes it;
/// * `box_return_against_target`: `auto_box_return` against the leaf, which is
///   what this returns unless the raw return applies.
pub(crate) fn mh_direct_lane(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    door: DirectLaneDoor,
) -> Option<MethodCallResult> {
    let site = call_site?;
    let DirectLaneShape {
        kind,
        bound,
        record,
    } = mh_direct_lane_shape(ctx, this, extra, site)?;
    let class = record.class.as_str();
    let name = record.name.as_str();
    let desc = record.desc.as_str();
    let params = &record.params;
    let decoded = mh_direct_lane_record_enabled();
    // From here on the lane answers the call; every refusal below is the one
    // the ordinary door would have returned at the same point.
    let bound_receiver = match bound {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    // Round 15 wave 4 (lane mh2; MH15-1): at the exact type the refusal below
    // can only answer `None`. With `CRATONVM_MH_INVOKE_DECLARED_CAST` and
    // `..._SKIPS_TYPED` on, its declared pass (`invoke_declared_type_refusal`
    // with declared = site) refuses nothing: every positive mismatch is at a
    // position the site names with the declared token itself
    // (`call_site_names_declared_param`), so it `continue`s, and it answers
    // the site's parameters whenever `extra` is not empty (the shape admitted
    // one argument per own-type parameter). Its leaf pass then finds every
    // reference position judged (`declared_position_judged`): the receiver's
    // `L<class>;` (no `specialCaller`: the shape declined one) and each
    // parameter token are the own type's own tokens (`own_type_aligned`); and
    // each primitive position holds a raw primitive (`fits`), which it skips.
    // No marking bit, so no explicit-cast arm. What it skipped was a split of
    // the site twice, a class `String` and its `L..;` text, and a hierarchy
    // question per reference argument.
    let refusal_is_empty = record.own_type_aligned && mh_direct_lane_invoke_refusal_skip_enabled();
    if door == DirectLaneDoor::Invoke && !refusal_is_empty {
        // Round 15 wave 1 (lane mh): with the record on, the refusal is asked
        // with `MH_DESC`'s parameters and `MH_CLASS` as the record decoded them
        // from the same objects, instead of decoding both again.
        let refusal = if decoded {
            invoke_reference_cast_refusal_decoded(
                ctx,
                this,
                kind,
                kind == MH_KIND_VIRTUAL,
                bound_receiver.is_some(),
                extra,
                Some(site),
                Some(site),
                params,
                Some(class),
            )
        } else {
            invoke_reference_cast_refusal_with(
                ctx,
                this,
                kind,
                kind == MH_KIND_VIRTUAL,
                bound_receiver.is_some(),
                extra,
                Some(site),
                Some(site),
            )
        };
        if let Some(refusal) = refusal {
            return Some(Err(refusal));
        }
    }
    let tail = door_tail_token(return_type_desc(site));
    let _scope = ExplicitScopeCleared::new();
    // The virtual arm's receiver: the bound value, else the caller's first
    // argument (the shape admitted only a non-null one).
    let (receiver, args) = match (kind, bound_receiver, extra.first()) {
        (MH_KIND_VIRTUAL, Some(r), _) => (Some(r), extra),
        (MH_KIND_VIRTUAL, None, Some(Value::Object(Some(r)))) => (Some(*r), &extra[1..]),
        _ => (None, extra),
    };
    if kind == MH_KIND_STATIC && door == DirectLaneDoor::Invoke {
        if decoded {
            // `mh_entry_adapt` for this shape (round 15 wave 1, lane mh): not a
            // collector, and the shape admitted exactly one argument per
            // parameter, none an array, so its arity refusal cannot fire, its
            // re-parse of `desc` is `params`, and what remains is the entry
            // cast of each argument in order -- without the parse's and the
            // answer's `Vec`s, which this lane discarded.
            for (v, p) in args.iter().zip(params.iter()) {
                if let Err(e) = entry_cast_check(ctx, *v, p) {
                    return Some(Err(e));
                }
            }
        } else if let Err(e) = mh_entry_adapt(ctx, this, site, desc, false, args) {
            return Some(Err(e));
        }
    }
    // Only the caller's receiver is cast here; a bound one was cast by `bindTo`
    // (the ordinary bound arm casts none either).
    if let (Some(r), None) = (receiver, bound_receiver) {
        // JIT round 19 wave 1 (lane mhiface; `CRATONVM_MH_INTERFACE_RECEIVER_CHECK`):
        // an interface member's `Interface.checkReceiver` comes first.
        if let Some(e) = mh_interface_receiver_refusal_named(ctx, r, class) {
            return Some(Err(e));
        }
        if let Some(e) = leaf_receiver_refusal_owned(ctx, this, r, class, false) {
            return Some(Err(e));
        }
    }
    for (v, p) in args.iter().zip(params.iter()) {
        if let Some(e) = leaf_reference_refusal(ctx, *v, p, false) {
            return Some(Err(e));
        }
    }
    let result = match receiver {
        None => {
            // The `MH_KIND_STATIC` arm of `mh_dispatch_body`, settling included.
            let settle_slot = mh_has_static_settled_slot(&*ctx, this);
            let settled = if settle_slot {
                mh_static_settled(&*ctx, this)
            } else {
                None
            };
            let settle_pin = (settle_slot && settled.is_none()).then(|| ctx.pin_native_root(this));
            // The carrier's door-key slot (wave 66, lane L3), as the arm's.
            let door_key = mh_carrier_door_key(&*ctx, this, settled);
            let owner = mh_static_owner(bound);
            // `DirectMethodHandle`'s barrier, as the arm's (wave 59, lane L4).
            let initialized = if settled.is_none() {
                mh_static_method_initialize_copy(ctx, owner, class, settle_slot, name, desc, args)
            } else {
                Ok(None)
            };
            let refreshed = match initialized {
                Ok(refreshed) => refreshed,
                Err(e) => {
                    if let Some(pin) = settle_pin {
                        ctx.unpin_native_roots(pin);
                    }
                    return Some(Err(e));
                }
            };
            let args: &[Value] = refreshed.as_deref().unwrap_or(args);
            let transparent = transparent_leaf_call(&*ctx, tail, desc, None);
            let result = if settle_slot {
                let (result, now_settled) = carrier_invoke_static_settling(
                    ctx,
                    door_key.as_deref(),
                    owner,
                    settled,
                    class,
                    name,
                    desc,
                    args,
                );
                if let Some(pin) = settle_pin {
                    if let Some(settled_owner) = now_settled {
                        let mh = ctx.read_native_pin(pin, this);
                        mh_set_static_settled(&*ctx, mh, settled_owner);
                    }
                    ctx.unpin_native_roots(pin);
                }
                result
            } else {
                match owner {
                    Some(owner) => ctx.invoke_static_by_class_id(owner, class, name, desc, args),
                    None => ctx.invoke_static(class, name, desc, args),
                }
            };
            transparent_leaf_call_done(ctx, transparent, &result);
            neutralize_missing_serialization_hook(result, args)
        }
        Some(receiver) if mh_direct_lane_lean_virtual_enabled() => {
            // `mh_dispatch_body`'s virtual arm without its argument pins
            // (round 15 wave 1, lane mh; see
            // `mh_direct_lane_lean_virtual_enabled`): nothing since the lane's
            // entry could collect, so `args` and `receiver` are current, and
            // `invoke_virtual_declared` roots them for the call itself. The
            // receiver's pin is for the neutral hook answer only, read after
            // the call.
            let recv_pin = ctx.pin_native_root(receiver);
            let transparent = transparent_leaf_call(&*ctx, tail, desc, Some(receiver));
            let result = ctx.invoke_virtual_declared(class, receiver, name, desc, args);
            transparent_leaf_call_done(ctx, transparent, &result);
            let receiver_now = ctx.read_native_pin(recv_pin, receiver);
            ctx.unpin_native_roots(recv_pin);
            // The hook's only use of its arguments is the receiver
            // (`serialization_hook_neutral_result`).
            neutralize_missing_serialization_hook(result, &[Value::Object(Some(receiver_now))])
        }
        Some(receiver) => {
            // `mh_dispatch_body`'s virtual arm (bound and unbound halves run
            // the same steps from here).
            let recv_pin = ctx.pin_native_root(receiver);
            let (adapted_pin_base, adapted_handles) = pin_mh_args(ctx, args);
            let adapted: Vec<Value> = args
                .iter()
                .enumerate()
                .map(|(i, arg)| read_pinned_mh_arg(&*ctx, adapted_handles[i], *arg))
                .collect();
            let receiver = ctx.read_native_pin(recv_pin, receiver);
            let mut full_args = Vec::with_capacity(1 + adapted.len());
            full_args.push(Value::Object(Some(receiver)));
            full_args.extend_from_slice(&adapted);
            let transparent = transparent_leaf_call(&*ctx, tail, desc, Some(receiver));
            let result = ctx.invoke_virtual_declared(class, receiver, name, desc, &adapted);
            transparent_leaf_call_done(ctx, transparent, &result);
            if adapted_pin_base != usize::MAX {
                ctx.unpin_native_roots(adapted_pin_base);
            }
            ctx.unpin_native_roots(recv_pin);
            neutralize_missing_serialization_hook(result, &full_args)
        }
    };
    Some(mh_direct_lane_return(ctx, result, desc))
}

/// The door's `box_return_against_target` for an admitted call (declared and
/// leaf returns agree, so it is `auto_box_return` against the leaf), except
/// that a primitive of the return's own `Value` variant is handed back raw
/// when [`mh_direct_lane_raw_return_enabled`].
pub(crate) fn mh_direct_lane_return(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    desc: &str,
) -> MethodCallResult {
    if mh_direct_lane_raw_return_enabled()
        && matches!(
            (return_type_desc(desc), &result),
            ("I" | "Z" | "B" | "S" | "C", Ok(Some(Value::Int(_))))
                | ("J", Ok(Some(Value::Long(_))))
                | ("F", Ok(Some(Value::Float(_))))
                | ("D", Ok(Some(Value::Double(_))))
        )
    {
        return result;
    }
    auto_box_return(ctx, result, desc)
}

// ---------------------------------------------------------------------------
// The converting lane (JIT round 16, lane mh; proposal MH16-2)
// ---------------------------------------------------------------------------
//
// `R16MhFloor` (round 16): `exact-astyped` (`twice(int)int.asType((Object)Object)
// .invokeExact(Object)`) and `invoke-astype` (`twice.invoke((Object) Integer)`)
// cost ~4.5x the direct lane, because the lane takes a call only at the
// handle's own type and everything else pays the whole ordinary door. The
// converting lane takes the next shape: an UNBOUND `findStatic` handle with no
// marking bit (no collector, no explicit stamp, no cast-chain work, not an exact
// invoker) and no array anywhere (leaf, declared or call-site parameters), called
// through `invokeExact` at its declared (possibly `asType`-stamped) type, or
// through `invoke` at a call site of its arity. It runs THE DOOR'S OWN STEPS, by
// the same functions and in the same order (`mh_invoke_exact_door` /
// `mh_invoke_door`, then the `MH_KIND_STATIC` arm of `mh_dispatch_body`, which
// `mh_dispatch` reaches unchanged for a handle with no marking bit), so every
// conversion and refusal is the door's; what it skips is what the door pays
// again per call for this shape: `MH_DESC` / `MH_CLASS` / `MH_NAME` decoded three
// times, the declared `type` rendered (both are the direct lane's per-handle
// record, memoised), the downcall and collector reads, the entry-call-site and
// tail channels. A shape it does not model declines before anything with an
// effect, and the door runs as before.

/// `CRATONVM_MH_DIRECT_LANE_CONVERTING` -- default ON since round 16 (JIT
/// round 16, lane mh; MH16-2): `invoke` / `invokeExact` of an unbound
/// `findStatic` handle at a type that converts (an `asType` copy's, or an
/// `invoke` call site's) take [`mh_converting_lane`] when the direct lane
/// declines. Measured (`R16MhFloor`): exact-astyped 5422→3960 ns,
/// invoke-astype 6671→4837 ns. `0`/`false`/`off`/`no` turns it off. Read once.
pub(crate) fn mh_converting_lane_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DIRECT_LANE_CONVERTING")
    })
}

/// The admission of [`mh_converting_lane`]: pure reads, no allocation, no GC
/// point. `Some((record, declared, bound))` when the call is one it models.
pub(crate) fn mh_converting_lane_shape(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    site: &str,
    door: DirectLaneDoor,
) -> Option<(std::rc::Rc<DirectLaneRecord>, String, Value)> {
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // Width-guarded, as `mh_direct_lane_shape` (wave 56, lane L4).
    if mh_kind_width_guarded(&*ctx, this) != MH_KIND_STATIC {
        return None;
    }
    if mh_marking_bits(&*ctx, this) != 0 {
        return None;
    }
    // Unbound: `MH_BOUND` holds nothing or the recorded owner's `Int`.
    let bound = ctx.get_field(this, MH_BOUND);
    if matches!(bound, Value::Object(Some(_))) {
        return None;
    }
    if crate::panama::is_downcall_handle(&*ctx, this) {
        return None;
    }
    if MH_ENTRY_CALL_SITE.with(|f| f.borrow().is_some()) {
        return None;
    }
    // The `invoke` door adapts a STATIC handle's arguments with
    // `adapt_invoke_args` only when it is not an adapter kind; decline the
    // other branch rather than model it.
    if !kind_has_authoritative_type(MH_KIND_STATIC)
        && kind_has_authoritative_adapted_type(MH_KIND_STATIC)
    {
        return None;
    }
    let record = if mh_direct_lane_record_enabled() {
        mh_direct_lane_record_memoised(ctx, this, false)?
    } else {
        let fields = [
            ctx.get_field(this, MH_CLASS),
            ctx.get_field(this, MH_NAME),
            ctx.get_field(this, MH_DESC),
            ctx.get_field_by_name(this, "type"),
        ];
        std::rc::Rc::new(mh_direct_lane_decode(ctx, fields, false, None)?)
    };
    let declared = record.declared.clone()?;
    let (declared_params, _) = split_descriptor_params(&declared)?;
    let (site_params, _) = split_descriptor_params(site)?;
    let n = record.params.len();
    if extra.len() != n || declared_params.len() != n || site_params.len() != n {
        return None;
    }
    let no_array = |ps: &[String]| ps.iter().all(|p| !p.starts_with('['));
    if !no_array(&record.params) || !no_array(&declared_params) || !no_array(&site_params) {
        return None;
    }
    if door == DirectLaneDoor::InvokeExact && site != declared {
        // The door's `exact_call_site_refusal` territory.
        return None;
    }
    Some((record, declared, bound))
}

/// The `MH_KIND_STATIC` arm of `mh_dispatch_body` for an unbound handle with no
/// marking bit, `entry_call_site` being what the door armed (`invoke`) or
/// `None` (`invokeExact`), with the explicit scope cleared as
/// `mh_dispatch_from_door` clears it. Step for step the arm's; `this` is
/// rooted by the caller's pin `this_pin` and re-read here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_converting_static_dispatch(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    record: &DirectLaneRecord,
    bound: Value,
    args: &[Value],
    entry_call_site: Option<&str>,
    tail: u8,
) -> MethodCallResult {
    let class = record.class.as_str();
    let name = record.name.as_str();
    let desc = record.desc.as_str();
    let mh = ctx.read_native_pin(this_pin, this);
    let _scope = ExplicitScopeCleared::new();
    let full = args.to_vec();
    let boxes = direct_return_boxes(ctx, mh, desc);
    let settle_slot = mh_has_static_settled_slot(&*ctx, mh);
    let settled = if settle_slot {
        mh_static_settled(&*ctx, mh)
    } else {
        None
    };
    // The carrier's door-key slot (wave 66, lane L3), as the arm's.
    let door_key = mh_carrier_door_key(&*ctx, mh, settled);
    let owner = mh_static_owner_in(&*ctx, mh, bound);
    let full = match entry_call_site {
        Some(cs) => mh_entry_adapt(ctx, mh, cs, desc, false, &full)?,
        None => collect_trailing_varargs(ctx, class, name, desc, &full, false),
    };
    let site = leaf_site_params(entry_call_site, 0, full.len());
    let mut adapted = adapt_leaf_args(ctx, &full, desc, site.as_deref(), false)?;
    // `DirectMethodHandle`'s barrier, as the arm's (wave 59, lane L4); `this`
    // is the caller's pin.
    if settled.is_none() {
        mh_static_method_initialize(ctx, owner, class, settle_slot, name, desc, &mut adapted)?;
    }
    let transparent = transparent_leaf_call(&*ctx, tail, desc, None);
    let result = if settle_slot {
        let (result, now_settled) = carrier_invoke_static_settling(
            ctx,
            door_key.as_deref(),
            owner,
            settled,
            class,
            name,
            desc,
            &adapted,
        );
        if settled.is_none() {
            if let Some(settled_owner) = now_settled {
                let mh = ctx.read_native_pin(this_pin, this);
                mh_set_static_settled(&*ctx, mh, settled_owner);
            }
        }
        result
    } else {
        match owner {
            Some(owner) => ctx.invoke_static_by_class_id(owner, class, name, desc, &adapted),
            None => ctx.invoke_static(class, name, desc, &adapted),
        }
    };
    transparent_leaf_call_done(ctx, transparent, &result);
    box_direct_primitive_return_pre(
        ctx,
        boxes,
        neutralize_missing_serialization_hook(result, &adapted),
        desc,
    )
}

/// The converting lane (MH16-2): `None` when the call is not one it models
/// (the ordinary door then runs, undisturbed), otherwise the door's answer,
/// computed by the door's own steps (see the section comment).
pub(crate) fn mh_converting_lane(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    door: DirectLaneDoor,
) -> Option<MethodCallResult> {
    let site = call_site?;
    let (record, declared, bound) = mh_converting_lane_shape(ctx, this, extra, site, door)?;
    // The door pins nothing here; the lane roots `this` from the first step
    // that can allocate (a refusal's exception, `adapt_invoke_args`) to the
    // settle write, which reads it back.
    let this_pin = ctx.pin_native_root(this);
    let call = ConvertingCall {
        this_pin,
        this,
        record: &record,
        declared: &declared,
        bound,
        extra,
        site,
    };
    let answer = match door {
        DirectLaneDoor::InvokeExact => mh_converting_invoke_exact(ctx, &call),
        DirectLaneDoor::Invoke => mh_converting_invoke(ctx, &call),
    };
    ctx.unpin_native_roots(this_pin);
    Some(answer)
}

/// What [`mh_converting_lane`] admitted, for its two door bodies.
pub(crate) struct ConvertingCall<'a> {
    pub(crate) this_pin: usize,
    pub(crate) this: ObjectRef,
    pub(crate) record: &'a DirectLaneRecord,
    pub(crate) declared: &'a str,
    pub(crate) bound: Value,
    pub(crate) extra: &'a [Value],
    pub(crate) site: &'a str,
}

/// `mh_invoke_exact_door`'s steps, in order, for an admitted call.
pub(crate) fn mh_converting_invoke_exact(
    ctx: &mut dyn NativeContext,
    call: &ConvertingCall,
) -> MethodCallResult {
    let kind = MH_KIND_STATIC;
    let (this, extra, site) = (call.this, call.extra, call.site);
    let desc = call.record.desc.as_str();
    let declared = Some(call.declared);
    if let Some(r) = exact_call_site_refusal(ctx, this, kind, site, declared) {
        return Err(r);
    }
    if let Some(r) = null_receiver_refusal(ctx, this, kind, extra) {
        return Err(r);
    }
    if let Some(r) = invoke_exact_inner_cast_refusal(ctx, this, kind, declared, extra) {
        return Err(r);
    }
    let plan = DoorReturn::exact(kind, desc, declared, 0, false);
    let tail = plan.exact_tail(Some(site));
    let result = mh_converting_static_dispatch(
        ctx,
        call.this_pin,
        this,
        call.record,
        call.bound,
        extra,
        None,
        tail,
    );
    door_return_apply(ctx, result, &plan)
}

/// `mh_invoke_door`'s steps, in order, for an admitted call (a static
/// handle: no receiver, nothing bound, not an adapter kind).
pub(crate) fn mh_converting_invoke(ctx: &mut dyn NativeContext, call: &ConvertingCall) -> MethodCallResult {
    let kind = MH_KIND_STATIC;
    let (this, extra, site) = (call.this, call.extra, call.site);
    let desc = call.record.desc.as_str();
    let declared = Some(call.declared);
    let null_receiver_first = !mh_invoke_null_receiver_after_casts_enabled();
    if null_receiver_first {
        if let Some(r) = null_receiver_refusal(ctx, this, kind, extra) {
            return Err(r);
        }
    }
    if let Some(r) = bound_handle_arity_refusal(ctx, this, site, declared) {
        return Err(r);
    }
    if let Some(r) = invoke_narrowing_arg_refusal(ctx, this, kind, site, declared) {
        return Err(r);
    }
    if let Some(r) = invoke_reference_cast_refusal_with(
        ctx,
        this,
        kind,
        false,
        false,
        extra,
        Some(site),
        declared,
    ) {
        return Err(r);
    }
    if !null_receiver_first {
        if let Some(r) = null_receiver_refusal(ctx, this, kind, extra) {
            return Err(r);
        }
    }
    let adapted = adapt_invoke_args(ctx, extra, desc);
    let plan = DoorReturn::invoke(kind, desc, declared, Some(site), 0, false);
    let tail = plan.invoke_tail();
    let result = mh_converting_static_dispatch(
        ctx,
        call.this_pin,
        this,
        call.record,
        call.bound,
        &adapted,
        Some(site),
        tail,
    );
    door_return_apply(ctx, result, &plan)
}

// ---------------------------------------------------------------------------
// Constant handles in compiled code (JIT round 16, lane mh; MH15-5 / C14-2)
// ---------------------------------------------------------------------------
//
// A compiled `invokevirtual MethodHandle.invokeExact` whose handle is the same
// object call after call (a `static final` field, a loop-invariant local) is
// bound by the JIT's dispatch helpers (`vm/src/jit/helpers.rs`,
// `mh_constant_bind_redirect`) to a direct call of the handle's target, guarded
// on the handle's identity, with the ordinary door as the fallback. This is the
// admission: which handles may be called as their target, and as what call.

/// What a compiled `invokeExact` site bound to a constant handle calls instead
/// of the door ([`mh_compiled_bind_target`]): the target member and the
/// `JitInvokeInfo` invoke kind (`3` static, `0` virtual).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MhCompiledBind {
    pub invoke_kind: u8,
    /// The target class's `ClassId` (raw), the class the member is declared by.
    /// The helper's call info carries it as its `declaring_class_id`, so every
    /// by-name resolution of `class` on that path (the dispatch, a deopting
    /// target's callee resume) asks THIS class's loader, which answers the
    /// class itself (round 16, `r16-mh-constant-bind-callee-resume-refused`).
    pub class_id: u32,
    pub class: String,
    pub name: String,
    pub desc: String,
    /// The handle is a `bindTo` of a virtual member (round 16 wave 8, lane mh;
    /// [`mh_constant_bind_bound_enabled`]): the call's receiver is the handle's
    /// `MH_BOUND` value, not a caller argument. The helper reads it from the
    /// guard word at [`MH_COMPILED_BIND_BOUND_GUARD`]. JIT round 21 wave 17
    /// (lane mhbind; MH6-3): also a REAL `bindTo` result, whose bound value
    /// (`argL1`, [`mh_real_bound_receiver`]) the helper's fill writes there.
    /// j27 wave 4 (lane MHROWS; MH16-7): with `invoke_kind` `3`, a REAL
    /// `bindTo` of a STATIC member: the bound value is the member's first
    /// argument, spliced in the handle's word as a receiver is. Every consumer
    /// that is not the helper's bound route must refuse a `bound` answer (the
    /// compile doors and the indy direct leaf do: `!t.bound`).
    pub bound: bool,
}

/// The index, in [`MH_COMPILED_BIND_GUARD_SLOTS`], of `MH_BOUND`: a bound
/// site's guard word there IS its receiver's address (round 16 wave 8).
pub const MH_COMPILED_BIND_BOUND_GUARD: usize = 4;

/// `CRATONVM_JIT_MH_CONSTANT_BIND_INVOKE` -- default ON since its A/B, `=0`
/// turns it off (JIT round 16 wave 8, lane mh; the default was flipped
/// without this comment, corrected in round 17 wave 2, lane mh2): a compiled
/// `MethodHandle.invoke` site binds like an
/// `invokeExact` one (`vm/src/jit/helpers.rs`, `mh_constant_bind_redirect`).
/// The admission is unchanged: the site's descriptor must BE the handle's own
/// exact type, and `invoke` at a handle's own type is `asType(type)` -- which
/// answers the handle itself (HotSpot's `MethodHandle.asType` `newType == type`
/// fast path) -- followed by `invokeExact`: the very call an `invokeExact` site
/// binds. `1`/`true`/`on`/`yes` turns it on. Read once.
pub fn mh_constant_bind_invoke_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_INVOKE")
    })
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_BOUND` -- default ON since its A/B, `=0`
/// turns it off (JIT round 16 wave 8, lane mh; comment corrected in round 17
/// wave 2): [`mh_compiled_bind_target`] also admits a `bindTo` of a
/// virtual member (`MhCompiledBind::bound`), and the dispatch helper calls the
/// member with the bound receiver spliced in place of the handle
/// (`vm/src/jit/helpers.rs`, `mh_constant_bind_call`). Rows that decide it:
/// `R16MhFloor` `exact-bound` (expected near `exact-virtual`).
/// `1`/`true`/`on`/`yes` turns it on. Read once.
pub fn mh_constant_bind_bound_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_BOUND")
    })
}

/// The handle slots whose values a bound site re-reads on every call (with the
/// handle's address and the collection epoch) to know it is still the handle
/// it bound: `MH_CLASS`, `MH_NAME`, `MH_DESC`, `MH_KIND`, `MH_BOUND`,
/// `MH_VARARGS` -- every slot [`mh_compiled_bind_target`] decides on that some
/// path rewrites in place. Read only on a handle the admission measured wide
/// enough (an equal address in an equal epoch is that same object).
pub const MH_COMPILED_BIND_GUARD_SLOTS: [usize; 6] =
    [MH_CLASS, MH_NAME, MH_DESC, MH_KIND, MH_BOUND, MH_VARARGS];

/// The handle slots a SHAPE-keyed bound site (JIT round 16 wave 11, lane
/// mhbind; MHF-1, `CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE`) compares besides
/// [`MH_COMPILED_BIND_GUARD_SLOTS`], the handle's class, its slot count and its
/// `type`: the rest of the synthetic slots a `bindTo` mint writes
/// ([`alloc_bound_method_handle`]). An address-keyed site re-reads only the
/// slots some path rewrites in place; a site that takes a handle at another
/// address must also see that every slot written once at construction is the
/// bound handle's, so the admission (which reads `MH_SPECIAL_CALLER`) and the
/// door it stands for would answer that handle as they answered the bound one.
pub const MH_COMPILED_BIND_SHAPE_SLOTS: [usize; 4] = [
    MH_STATIC_SETTLED,
    MH_SPECIAL_CALLER,
    MH_CAST_CHAIN,
    MH_STATIC_OWNER,
];

/// May a compiled `invokeExact` site of descriptor `site` call `handle`'s
/// target directly, and as what (MH15-5)? `Some` only where the door, called
/// with ANY arguments a verified caller can pass at `site`, does nothing but
/// that call -- the direct lane's own argument
/// ([`mh_direct_lane_shape`]: a `findStatic` / unbound `findVirtual` handle
/// with no marking bit, no `specialCaller`, at its own exact type, no array
/// parameter), narrowed to what a dispatch helper cannot re-check per call:
///
/// * the leaf's reference casts (`leaf_receiver_refusal`,
///   `leaf_reference_refusal`) can refuse only a value the verifier did not
///   type: so the receiver class and every reference parameter must name a
///   loaded non-interface class (the verifier does not check interface
///   assignability). A null receiver is the caller's to screen per call (the
///   door's message-less NPE);
/// * the target is declared by `class` itself (static for a static handle;
///   neither static nor private for a virtual one, so `invokevirtual`
///   selection is `invoke_virtual_declared`'s);
/// * not a serialization hook (the door's neutral answer) and not a
///   `java/` / `jdk/` / `sun/` member (caller-sensitive methods see their
///   caller; through the door that is not the compiled caller);
/// * a static handle with a recorded owner only when the owner is what the
///   class name resolves to (the helper's call resolves by name, as the door
///   does without an owner).
///
/// Round 16 wave 8 (lane mh): with [`mh_constant_bind_bound_enabled`], also a
/// `bindTo` of a virtual member (`MH_BOUND` holds its receiver;
/// `MhCompiledBind::bound`) at its declared type, which must be the member's
/// own descriptor (the receiver is no call-site argument). The bound receiver
/// must be an instance of the member's class: `bindTo` refused anything else,
/// and the door's `leaf_receiver_refusal` asks the same question of the same
/// value. It is a property of the handle, re-validated per call by the
/// `MH_BOUND` guard word.
///
/// Pure reads; no allocation, no GC point.
pub fn mh_compiled_bind_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    mh_compiled_bind_target_with(ctx, handle, site, mh_constant_bind_bound_enabled())
}

/// [`mh_compiled_bind_target`] with every `bindTo` (bound) arm off: the same
/// answer for every handle it admits unbound, `None` for a bound one (JIT round 21
/// wave 19, lane mhbind2). For a caller that discards a bound answer anyway (the
/// interpreter's constant-bind fill), so it does not pay the REAL bound admission
/// (`mh_real_bound_compiled_bind`: two handle records and the `BIND_ARG` form scan)
/// on every fresh `bindTo` result it meets. Pure reads; no allocation, no GC point.
pub fn mh_compiled_bind_target_unbound(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    mh_compiled_bind_target_with(ctx, handle, site, false)
}

/// [`mh_compiled_bind_target`] with the bound-handle switch as a parameter
/// (the unit tests ask both answers).
pub(crate) fn mh_compiled_bind_target_with(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
    admit_bound: bool,
) -> Option<MhCompiledBind> {
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // SBR3 wave 2 (lane mhperf): a REAL `DirectMethodHandle` (none of this model's
    // slots) binds as its member too.
    if mh_is_real_handle(&*ctx, handle) {
        if let Some(bind) = mh_real_compiled_bind(ctx, handle, site) {
            return Some(bind);
        }
        // JIT round 21 wave 17, lane mhbind (MH6-3): a REAL `bindTo` result
        // binds as its member with the bound receiver spliced in.
        if admit_bound && mh_constant_bind_real_bound_enabled() {
            return mh_real_bound_compiled_bind(ctx, handle, site);
        }
        return None;
    }
    if ctx.object_num_fields(handle) <= MH_STATIC_OWNER {
        return None;
    }
    if ctx
        .class_name_of_id(ctx.class_id_of_object(handle))
        .as_deref()
        != Some("java/lang/invoke/MethodHandle")
    {
        return None;
    }
    let kind = match ctx.get_field(handle, MH_KIND) {
        Value::Int(k) if k == MH_KIND_STATIC || k == MH_KIND_VIRTUAL => k,
        _ => return None,
    };
    if mh_marking_bits(&*ctx, handle) != 0 || crate::panama::is_downcall_handle(&*ctx, handle) {
        return None;
    }
    if !matches!(
        ctx.get_field(handle, MH_SPECIAL_CALLER),
        Value::Object(None)
    ) {
        return None;
    }
    let bound = ctx.get_field(handle, MH_BOUND);
    // A bound value is a VIRTUAL handle's receiver; a static handle's leading
    // argument is the door's to place (the direct lane's own rule).
    let bound_receiver = match bound {
        Value::Object(Some(r)) if admit_bound && kind == MH_KIND_VIRTUAL => Some(r),
        Value::Object(Some(_)) => return None,
        _ => None,
    };
    let fields = [
        ctx.get_field(handle, MH_CLASS),
        ctx.get_field(handle, MH_NAME),
        ctx.get_field(handle, MH_DESC),
        ctx.get_field_by_name(handle, "type"),
    ];
    let unbound_virtual = kind == MH_KIND_VIRTUAL && bound_receiver.is_none();
    let record = mh_direct_lane_decode(ctx, fields, unbound_virtual, Some(site))?;
    if !record.declared_is_own_type || !record.own_type_aligned || record.own_type != site {
        return None;
    }
    mh_compiled_bind_member(ctx, kind, bound, bound_receiver, &record, None)
}

/// Is `handle` none of this model's KINDED carriers (the carrier's `MH_KIND`
/// is an `Int`)? One width compare for every real handle. Pure reads. Note
/// that a `Linker.downcallHandle` carrier is kindless and so passes: ask
/// [`mh_is_real_jdk_handle`] for "a handle the JDK built".
#[inline]
pub fn mh_is_real_handle(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    !(ctx.object_num_fields(handle) > MH_BOUND
        && matches!(ctx.get_field(handle, MH_KIND), Value::Int(_)))
}

/// Is `handle` a REAL JDK method handle: neither a kinded carrier
/// ([`mh_is_real_handle`]) nor the VM-minted downcall carrier
/// (`panama::is_downcall_handle`, its tag)? Pure reads. JIT round j27 wave 4,
/// lane MHROWS2 (MHEDITOR17-1: the one predicate for the fact).
pub fn mh_is_real_jdk_handle(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    mh_is_real_handle(ctx, handle) && !crate::panama::is_downcall_handle(ctx, handle)
}

/// A REAL `DirectMethodHandle`'s method member, as its `MemberName` names it
/// (JIT r23 wave 4b, lane L2; L2W4B-1, `CRATONVM_JIT_MH_CONSTANT_CONDY`): the
/// compile-time bind of an `invokeExact` whose receiver is a resolved
/// `CONSTANT_Dynamic` handle reads it once, on the mutator that resolved the
/// constant, and calls the member by its class id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MhDirectMember {
    /// `REF_invokeVirtual` (5), `REF_invokeStatic` (6), `REF_invokeSpecial`
    /// (7, only a `$Special` handle) or `REF_invokeInterface` (9).
    pub ref_kind: i32,
    /// The declaring class, from `MemberName.clazz`'s mirror (never by name).
    pub class_id: u32,
    pub class: String,
    pub name: String,
    /// The member's own descriptor (no receiver).
    pub desc: String,
    /// The handle's `type()` as a descriptor: what `invokeExact` compares.
    pub declared: String,
}

/// [`MhDirectMember`] of `handle`, or `None` for a model handle, a
/// non-`DirectMethodHandle`, a field or constructor kind, or a member whose
/// class mirror names no class. Pure reads (the per-thread record memo).
pub fn mh_real_direct_member(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
) -> Option<MhDirectMember> {
    if !mh_is_real_handle(&*ctx, handle) {
        return None;
    }
    let record = real_handle_record(ctx, handle)?;
    let target = record.direct.as_ref()?;
    if target.name.starts_with('<') {
        return None;
    }
    Some(MhDirectMember {
        ref_kind: target.ref_kind,
        class_id: target.owner?.as_u32(),
        class: target.class.clone(),
        name: target.name.clone(),
        desc: target.desc.clone(),
        declared: record.declared.clone(),
    })
}

/// The compiled-bind admission of a REAL `DirectMethodHandle` (SBR3 wave 2, lane
/// mhperf): a `findStatic` / `findVirtual` handle called at its own exact type is
/// what the carrier's `MH_KIND_STATIC` / `MH_KIND_VIRTUAL` handle is, and the member
/// questions are [`mh_compiled_bind_member`]'s. Every field the record is read from
/// is `final`, so the bound site's guard is the handle's address and the epoch alone.
pub(crate) fn mh_real_compiled_bind(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    let record = real_handle_record(ctx, handle)?;
    let target = record.direct.as_ref()?;
    let (kind, own_type) = mh_real_direct_kind(target)?;
    if record.declared != own_type || own_type != site {
        return None;
    }
    let lane = mh_real_lane_record(target, own_type);
    mh_compiled_bind_member(ctx, kind, Value::Object(None), None, &lane, target.owner)
}

/// The carrier kind (`MH_KIND_STATIC` / `MH_KIND_VIRTUAL`) and the own type
/// (an unbound virtual member's receiver first) a REAL direct handle of a
/// `REF_invokeStatic` / `REF_invokeVirtual` member binds as; `None` for any
/// other kind. Pure. (j27 wave 5, lane MHASTYPE: moved out of
/// [`mh_real_compiled_bind`] unchanged, shared with
/// [`mh_real_converting_compiled_bind`].)
pub(crate) fn mh_real_direct_kind(target: &RealDirectTarget) -> Option<(i32, String)> {
    match target.ref_kind {
        6 => Some((MH_KIND_STATIC, target.desc.clone())),
        5 => Some((
            MH_KIND_VIRTUAL,
            format!("(L{};{}", target.class, target.desc.get(1..)?),
        )),
        _ => None,
    }
}

/// The [`DirectLaneRecord`] of a REAL direct handle's member called at
/// `own_type` (j27 wave 5, lane MHASTYPE: moved out of
/// [`mh_real_compiled_bind`] unchanged). Pure.
pub(crate) fn mh_real_lane_record(target: &RealDirectTarget, own_type: String) -> DirectLaneRecord {
    DirectLaneRecord {
        class: target.class.clone(),
        name: target.name.clone(),
        desc: target.desc.clone(),
        params: target.params.clone(),
        own_type: own_type.clone(),
        declared: Some(own_type),
        declared_is_own_type: true,
        own_type_aligned: true,
    }
}

// ---------------------------------------------------------------------------
// JIT round 21 wave 17, lane mhbind (MH6-3 / MHB-3): a REAL `bindTo` result
// ---------------------------------------------------------------------------
//
// Since the `JdkInvoke` row (both modes) `findVirtual` answers a real
// `DirectMethodHandle` and `bindTo` runs the JDK's own body: `rebind()` (a
// `BoundMethodHandle$Species_L` reinvoker, `makeReinvoker`) then
// `editor().bindArgumentL(this, 0, x)`, a `Species_LL` whose `argL0` is the
// direct handle, `argL1` the bound value and `form` the reinvoker form's cached
// `BIND_ARG` transform at position 1 (`LambdaFormEditor.bindArgumentForm(1)`).
// That form is `lambda (bmh, a*) { invokeBasic(bmh.argL0, bmh.argL1, a*) }`,
// and `invokeBasic` on a plain `REF_invokeVirtual` direct handle is
// `linkToVirtual(receiver, a*, member)`: the member called virtually on the
// bound value. The compiled-bind admission had no arm for it (a real handle
// binds only when it is a `DirectMethodHandle`), so every compiled
// `invokeExact` of a `static final` bound handle took the signature-polymorphic
// link chain (linker, the species' spun form, the direct handle's form,
// `linkToVirtual`: four helper hops). HotSpot inlines the chain to a direct
// call of the member with `argL1` as receiver.

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND` -- perf, default ON, `=0` off (JIT
/// round 21 wave 17, lane mhbind; MH6-3 of the r18 backlog, page
/// `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`):
/// [`mh_compiled_bind_target`] also admits a REAL `bindTo` result
/// ([`mh_real_bound_compiled_bind`]). Read on the dispatch helper's cold fill
/// only.
pub fn mh_constant_bind_real_bound_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND")
}

/// The species `bindTo` of a plain direct handle answers.

/// `MethodTypeForm.LF_REBIND`: the `lambdaForms` index of the
/// `BoundMethodHandle` reinvoker form (`BoundMethodHandle.makeReinvoker`).
const MTF_LF_REBIND: usize = 7;

/// `LambdaFormEditor.TransformKey.of(BIND_ARG, 1)`'s packed bytes: `BIND_ARG`
/// (`1`) in the low nibble, the position (`1`) in the next one.
const LFE_BIND_ARG_1_PACKED: i64 = 0x11;

/// The most `Transform[]` entries [`mh_real_is_bind_to_form`] scans (the JDK's
/// array cache holds at most 16 before it becomes a `ConcurrentHashMap`).
const LFE_TRANSFORM_SCAN_MAX: usize = 64;

/// Is `form` (uncustomized) the form `bindTo` gives a rebind of the direct
/// handle `target`: the `BIND_ARG` position-1 transform cached on the
/// reinvoker form of `target`'s basic type
/// (`target.type.form.basicType.form.lambdaForms[LF_REBIND]`)? Pure reads. Any
/// shape this does not recognise (a cleared soft reference, a
/// `ConcurrentHashMap` cache, a `fullBytes` key) answers `false`: the site keeps
/// its link chain.
fn mh_real_is_bind_to_form(ctx: &dyn NativeContext, target: ObjectRef, form: ObjectRef) -> bool {
    // `LambdaForm.uncustomize()`: a customized form links its uncustomized
    // version through `transformCache`.
    let form = match mh_real_obj(ctx.get_field_by_name(form, "customized")) {
        None => form,
        Some(_) => match mh_real_obj(ctx.get_field_by_name(form, "transformCache")) {
            Some(f) => f,
            None => return false,
        },
    };
    let Some(reinvoker) = mh_real_rebind_form(ctx, target) else {
        return false;
    };
    let is_bind_arg_1 = |t: ObjectRef| {
        matches!(
            ctx.get_field_by_name(t, "packedBytes"),
            Value::Long(LFE_BIND_ARG_1_PACKED)
        ) && matches!(ctx.get_field_by_name(t, "referent"), Value::Object(Some(f)) if f == form)
    };
    let Some(cache) = mh_real_obj(ctx.get_field_by_name(reinvoker, "transformCache")) else {
        return false;
    };
    // An array first: `class_id_of_object` of a reference array answers its
    // component class.
    if ctx.object_is_array(cache) {
        let n = ctx.array_length(cache).min(LFE_TRANSFORM_SCAN_MAX);
        for i in 0..n {
            match mh_real_obj(ctx.get_array_element(cache, i)) {
                Some(t) => {
                    if is_bind_arg_1(t) {
                        return true;
                    }
                }
                None => break,
            }
        }
        return false;
    }
    ctx.class_name_arc_of_id(ctx.class_id_of_object(cache))
        .is_some_and(|n| &*n == "java/lang/invoke/LambdaFormEditor$Transform")
        && is_bind_arg_1(cache)
}

/// A non-null reference value's object.
pub(crate) fn mh_real_obj(v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The `BoundMethodHandle` reinvoker form cached for `target`'s basic type
/// (`target.type.form.basicType.form.lambdaForms[LF_REBIND]`'s referent), or
/// `None`. Pure reads.
fn mh_real_rebind_form(ctx: &dyn NativeContext, target: ObjectRef) -> Option<ObjectRef> {
    let mt = mh_real_obj(ctx.get_field_by_name(target, "type"))?;
    let mtf = mh_real_obj(ctx.get_field_by_name(mt, "form"))?;
    let basic = mh_real_obj(ctx.get_field_by_name(mtf, "basicType"))?;
    let basic_form = mh_real_obj(ctx.get_field_by_name(basic, "form"))?;
    let forms = mh_real_obj(ctx.get_field_by_name(basic_form, "lambdaForms"))?;
    if !ctx.object_is_array(forms) || ctx.array_length(forms) <= MTF_LF_REBIND {
        return None;
    }
    let soft = mh_real_obj(ctx.get_array_element(forms, MTF_LF_REBIND))?;
    mh_real_obj(ctx.get_field_by_name(soft, "referent"))
}

/// The direct handle and the bound value of a REAL `bindTo` result of a plain
/// `DirectMethodHandle` (`BoundMethodHandle$Species_LL`, its `form` the
/// `bindTo` form, [`mh_real_is_bind_to_form`]), or `None`. The bound value is
/// non-null. Pure reads; every field read is `final`, so the answer is a
/// function of the handle's identity.
fn mh_real_bound_parts(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
) -> Option<(ObjectRef, ObjectRef)> {
    if !mh_is_real_handle(ctx, handle)
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .as_deref()
            != Some(BMH_SPECIES_LL)
    {
        return None;
    }
    let Value::Object(Some(target)) = ctx.get_field_by_name(handle, "argL0") else {
        return None;
    };
    let Value::Object(Some(receiver)) = ctx.get_field_by_name(handle, "argL1") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(target))
        .as_deref()
        != Some("java/lang/invoke/DirectMethodHandle")
    {
        return None;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(handle, "form") else {
        return None;
    };
    mh_real_is_bind_to_form(ctx, target, form).then_some((target, receiver))
}

/// The bound value of a REAL `bindTo` result [`mh_real_bound_compiled_bind`]
/// admitted (its `argL1`), for the dispatch helper's guard word. Pure reads.
pub fn mh_real_bound_receiver(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(handle, "argL1") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    }
}

/// The compiled-bind admission of a REAL `bindTo` result (JIT round 21 wave
/// 17, lane mhbind; MH6-3, [`mh_constant_bind_real_bound_enabled`]): a
/// [`mh_real_bound_parts`] handle whose direct handle is a `REF_invokeVirtual`
/// of its member at the member's own exact type, called at the bound handle's
/// own type, which must be the member's descriptor (the receiver is no
/// call-site argument), binds as the carrier's `bindTo` handle does
/// (`MhCompiledBind::bound`): the member called virtually on the bound value,
/// which [`mh_compiled_bind_member`] requires to be a non-array instance of the
/// member's class. Pure reads; no GC point.
///
/// j27 wave 4 (lane MHROWS; MH16-7,
/// [`mh_constant_bind_real_bound_static_enabled`]): also a `REF_invokeStatic`
/// direct handle bound on its leading parameter (`findStatic(..).bindTo(x)`,
/// `insertArguments(.., 0, x)`): the same species and form (`rebind()` then
/// `bindArgumentL(0, x)` do not depend on the member's kind), so `invokeBasic`
/// on the direct handle is `linkToStatic(argL1, a*, member)`. It binds as a
/// static member with the bound value spliced as its first argument
/// (`MhCompiledBind::bound` with `invoke_kind` `3`); the value must be a
/// non-array instance of the class the leading parameter names, which the
/// JDK's `bindTo` / `insertArguments` cast already proved.
fn mh_real_bound_compiled_bind(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    let (target, receiver) = mh_real_bound_parts(&*ctx, handle)?;
    let bound_record = real_handle_record(ctx, handle)?;
    if bound_record.declared != site {
        return None;
    }
    let record = real_handle_record(ctx, target)?;
    let direct = record.direct.as_ref()?;
    let member = mh_real_bound_member_kind(
        direct.ref_kind,
        &direct.class,
        &direct.desc,
        &record.declared,
        &bound_record.declared,
        site,
        mh_constant_bind_real_bound_static_enabled(),
    )?;
    let own_type = match member {
        MhRealBoundMember::Virtual => format!("(L{};{}", direct.class, direct.desc.get(1..)?),
        MhRealBoundMember::Static { .. } => direct.desc.clone(),
    };
    let lane = DirectLaneRecord {
        class: direct.class.clone(),
        name: direct.name.clone(),
        desc: direct.desc.clone(),
        params: direct.params.clone(),
        own_type: own_type.clone(),
        declared: Some(own_type),
        declared_is_own_type: true,
        own_type_aligned: true,
    };
    match member {
        MhRealBoundMember::Virtual => mh_compiled_bind_member(
            ctx,
            MH_KIND_VIRTUAL,
            Value::Object(None),
            Some(receiver),
            &lane,
            direct.owner,
        ),
        MhRealBoundMember::Static { leading } => {
            if !mh_real_bound_leading_value_ok(&*ctx, receiver, &leading) {
                return None;
            }
            let bind = mh_compiled_bind_member(
                ctx,
                MH_KIND_STATIC,
                Value::Object(None),
                None,
                &lane,
                direct.owner,
            )?;
            Some(MhCompiledBind { bound: true, ..bind })
        }
    }
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC` -- perf, default ON, `=0`
/// off (j27 wave 4, lane MHROWS; MH16-7 of the r18 backlog): the REAL `bindTo`
/// admission ([`mh_real_bound_compiled_bind`]) also binds a static member bound
/// on its leading parameter. Off: such a handle keeps the signature-polymorphic
/// link chain (four helper hops per call), as before. Read on the dispatch
/// helper's cold fill only.
pub fn mh_constant_bind_real_bound_static_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC",
    )
}

/// What a REAL `bindTo` result's direct handle binds as (j27 wave 4, lane
/// MHROWS; [`mh_real_bound_member_kind`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MhRealBoundMember {
    /// `REF_invokeVirtual`: the member called virtually on the bound value.
    Virtual,
    /// `REF_invokeStatic`: the member called with the bound value as its
    /// leading argument, a reference of the class `leading` names.
    Static { leading: String },
}

/// The type questions of [`mh_real_bound_compiled_bind`], pure (j27 wave 4,
/// lane MHROWS; MH16-7). `ref_kind`, `class` and `desc` are the direct
/// handle's member (`desc` its own descriptor, no receiver),
/// `target_declared` the direct handle's `type`, `bound_declared` the bound
/// handle's `type`, `site` the call site's descriptor. The bound handle must
/// be called at its own exact type (any other `invokeExact` type is the door's
/// `WrongMethodTypeException`). A `REF_invokeVirtual` handle at its own type
/// `(L<class>;..)` binds its receiver, so the site is `desc`. A
/// `REF_invokeStatic` handle (only with `admit_static`) at its own type `desc`
/// binds its leading parameter, which must name a class (`L..;`: never a
/// primitive, which `bindTo` refuses, nor an array, which the member rules
/// refuse), so the site is `desc` without it. Anything else: `None`.
pub(crate) fn mh_real_bound_member_kind(
    ref_kind: i32,
    class: &str,
    desc: &str,
    target_declared: &str,
    bound_declared: &str,
    site: &str,
    admit_static: bool,
) -> Option<MhRealBoundMember> {
    if bound_declared != site {
        return None;
    }
    match ref_kind {
        5 => {
            let own_type = format!("(L{class};{}", desc.get(1..)?);
            (desc == site && target_declared == own_type).then_some(MhRealBoundMember::Virtual)
        }
        6 if admit_static && target_declared == desc => {
            let rest = desc.strip_prefix("(L")?;
            let end = rest.find(';')?;
            let leading = &rest[..end];
            let unbound = format!("({}", &rest[end + 1..]);
            (!leading.is_empty() && unbound == site).then(|| MhRealBoundMember::Static {
                leading: leading.to_string(),
            })
        }
        _ => None,
    }
}

/// May a bound static member take `value` as its leading argument of class
/// `leading` without a cast (j27 wave 4, lane MHROWS; MH16-7)? A non-array
/// instance of the one loaded class of that name; an ambiguous or absent name
/// refuses (the door keeps the call). Pure reads.
fn mh_real_bound_leading_value_ok(ctx: &dyn NativeContext, value: ObjectRef, leading: &str) -> bool {
    match ctx.classify_class_name(leading) {
        cratonvm_native_api::NameLookup::Unique(cid) => {
            !ctx.object_is_array(value) && ctx.is_subclass(ctx.class_id_of_object(value), cid)
        }
        _ => false,
    }
}

#[cfg(test)]
mod j27_mh_real_bound_member_kind_tests {
    use super::{mh_real_bound_member_kind as kind, MhRealBoundMember};

    /// `Acc.plus(I)I` bound on its receiver: `(I)I` at the site.
    #[test]
    fn j27_virtual_bind_to_keeps_the_round_21_answer() {
        let got = kind(5, "p/Acc", "(I)I", "(Lp/Acc;I)I", "(I)I", "(I)I", false);
        assert_eq!(got, Some(MhRealBoundMember::Virtual));
        // Another `invokeExact` type is the door's WrongMethodTypeException.
        assert_eq!(kind(5, "p/Acc", "(I)I", "(Lp/Acc;I)I", "(I)I", "(I)J", true), None);
        // A stamped (asType) direct handle is not at its own type.
        assert_eq!(kind(5, "p/Acc", "(I)I", "(Ljava/lang/Object;I)I", "(I)I", "(I)I", true), None);
    }

    /// `static long add(Long, int)` bound on its `Long`: `(I)J` at the site.
    #[test]
    fn j27_static_bind_to_drops_the_leading_reference_parameter() {
        let desc = "(Ljava/lang/Long;I)J";
        let got = kind(6, "p/S", desc, desc, "(I)J", "(I)J", true);
        assert_eq!(
            got,
            Some(MhRealBoundMember::Static {
                leading: "java/lang/Long".to_string()
            })
        );
        let one = kind(6, "p/S", "(Lp/Box;)V", "(Lp/Box;)V", "()V", "()V", true);
        assert_eq!(
            one,
            Some(MhRealBoundMember::Static {
                leading: "p/Box".to_string()
            })
        );
    }

    #[test]
    fn j27_static_bind_to_refuses_what_it_cannot_prove() {
        let desc = "(Ljava/lang/Long;I)J";
        // The switch off: the round-21 answer (no static bind).
        assert_eq!(kind(6, "p/S", desc, desc, "(I)J", "(I)J", false), None);
        // Site is not the bound type (WrongMethodTypeException is the door's).
        assert_eq!(kind(6, "p/S", desc, desc, "(I)J", "(Ljava/lang/Long;I)J", true), None);
        // The bound type disagrees with the member minus its leading parameter.
        assert_eq!(kind(6, "p/S", desc, desc, "(J)J", "(J)J", true), None);
        // The direct handle was stamped by asType.
        assert_eq!(kind(6, "p/S", desc, "(Ljava/lang/Object;I)J", "(I)J", "(I)J", true), None);
        // A primitive or array leading parameter.
        assert_eq!(kind(6, "p/S", "(II)J", "(II)J", "(I)J", "(I)J", true), None);
        assert_eq!(kind(6, "p/S", "([II)J", "([II)J", "(I)J", "(I)J", true), None);
        // No parameter at all, and an interface / special kind.
        assert_eq!(kind(6, "p/S", "()J", "()J", "()J", "()J", true), None);
        assert_eq!(kind(7, "p/S", desc, desc, "(I)J", "(I)J", true), None);
        assert_eq!(kind(9, "p/S", "(I)I", "(Lp/S;I)I", "(I)I", "(I)I", true), None);
    }
}

// ---------------------------------------------------------------------------
// JIT round 21 wave 20, lane mhfilter (MHBIND2-1): a REAL filter species
// ---------------------------------------------------------------------------
//
// `MethodHandles.filterArgument(target, pos, filter)` (JDK 25) is
// `target.rebind()` (the `LF_REBIND` reinvoker, `Species_L(argL0 = target)`)
// then `copyWithExtendL(newType, editor().filterArgumentForm(1 + pos,
// basicType(filter's parameter)), filter)`: a `Species_LL` whose `argL0` is the
// target, `argL1` the filter, and whose form is the reinvoker form's cached
// `FILTER_ARG` transform (`TransformKey.of(FILTER_ARG, 1 + pos, type)`) --
// `invokeBasic(argL0, a0 .. invokeBasic(argL1, a_pos) .. ak)`, the filter
// FIRST. `filterReturnValue(target, filter)` is the same with
// `filterReturnForm(rtype, false)` (`TransformKey.of(FILTER_RETURN, 0, rtype)`):
// `invokeBasic(argL1, invokeBasic(argL0, a*))`. Every compiled-bind admission
// refuses such a species (a `bindTo` one is told apart by its `BIND_ARG`
// form), so every compiled call walked the link chain (about six helper hops).

/// `LambdaFormEditor.FILTER_ARG`, the transform kind in a key's low nibble.
const LFE_FILTER_ARG: i64 = 5;

/// `LambdaFormEditor.FILTER_RETURN`.
const LFE_FILTER_RETURN: i64 = 6;

/// The packed `TransformKey` bytes of the transform cached on `target`'s
/// reinvoker form ([`mh_real_rebind_form`]) whose result is `form`
/// (uncustomized), or `None`: [`mh_real_is_bind_to_form`]'s scan, answering
/// the key instead of testing one. A `fullBytes` key (`packedBytes == 0`), a
/// cleared soft reference or a `ConcurrentHashMap` cache answers `None`. Pure
/// reads.
fn mh_real_transform_key(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    form: ObjectRef,
) -> Option<i64> {
    // `LambdaForm.uncustomize()`.
    let form = match mh_real_obj(ctx.get_field_by_name(form, "customized")) {
        None => form,
        Some(_) => mh_real_obj(ctx.get_field_by_name(form, "transformCache"))?,
    };
    let reinvoker = mh_real_rebind_form(ctx, target)?;
    let key_of = |t: ObjectRef| -> Option<i64> {
        match ctx.get_field_by_name(t, "referent") {
            Value::Object(Some(f)) if f == form => {}
            _ => return None,
        }
        match ctx.get_field_by_name(t, "packedBytes") {
            Value::Long(k) if k != 0 => Some(k),
            _ => None,
        }
    };
    let cache = mh_real_obj(ctx.get_field_by_name(reinvoker, "transformCache"))?;
    // An array first: `class_id_of_object` of a reference array answers its
    // component class.
    if ctx.object_is_array(cache) {
        let n = ctx.array_length(cache).min(LFE_TRANSFORM_SCAN_MAX);
        for i in 0..n {
            let t = mh_real_obj(ctx.get_array_element(cache, i))?;
            if let Some(k) = key_of(t) {
                return Some(k);
            }
        }
        return None;
    }
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(cache))
        .is_some_and(|n| &*n == "java/lang/invoke/LambdaFormEditor$Transform")
    {
        key_of(cache)
    } else {
        None
    }
}

/// `Some(Some(p))` for a `FILTER_ARG` key at site argument `p`, `Some(None)`
/// for a `FILTER_RETURN` key of a non-constant filter, `None` for any other
/// key (JIT round 21 wave 20, lane mhfilter). `TransformKey.packedBytes`:
/// the kind in bits 0-3, then 4 bits per byte. Pure.
fn mh_filter_transform_position(key: i64) -> Option<Option<u16>> {
    // Three bytes at most: kind, position / constant-zero flag, basic type.
    if key >> 12 != 0 {
        return None;
    }
    let b1 = (key >> 4) & 0xF;
    match key & 0xF {
        // `filterArgumentForm(1 + pos, ..)`: the LambdaForm counts the
        // handle as parameter 0.
        LFE_FILTER_ARG if b1 >= 1 => u16::try_from(b1 - 1).ok().map(Some),
        // `filterReturnForm(.., false)`: `1` is `constantZero`, no filter call.
        LFE_FILTER_RETURN if b1 == 0 => Some(None),
        _ => None,
    }
}

/// What a compiled `invokeExact` site may call for a REAL filter species
/// ([`mh_real_filter_compiled_target`]; JIT round 21 wave 20, lane mhfilter).
pub struct MhFilterCompiled {
    /// The target's member (`invoke_kind` `3` static or `0` virtual, never
    /// bound), as [`mh_compiled_bind_member`] admits a direct handle's.
    pub target: MhCompiledBind,
    /// The filter's member: a static method (`invoke_kind` `3`).
    pub filter: MhCompiledBind,
    /// The target's direct handle (the species' `argL0`).
    pub target_handle: ObjectRef,
    /// `Some(p)`: `filterArguments` at site argument `p`; `None`:
    /// `filterReturnValue`.
    pub position: Option<u16>,
}

/// The compiled-call admission of a REAL filter species (JIT round 21 wave 20,
/// lane mhfilter; MHBIND2-1): a `BoundMethodHandle$Species_LL` called at its
/// own exact type (`site`), whose `argL0` and `argL1` are plain
/// `DirectMethodHandle`s, whose form is the `FILTER_ARG` / `FILTER_RETURN`
/// transform cached on the target's reinvoker form ([`mh_real_transform_key`]),
/// whose target is a `findStatic` / unbound `findVirtual` handle at its own
/// exact type, and whose filter is a `findStatic` handle at its own exact type.
/// Each member passes [`mh_compiled_bind_member`] (no `java/` / `jdk/` /
/// `sun/` member, no array parameter, every parameter class unique and no
/// interface, declared by its class, static-ness per kind). The text shape of
/// `site` against the two members is the compile door's question
/// (`mh_filter_row_fits` in the jit crate); the JDK builds the species' type
/// from them. Pure reads; no GC point; every field read is `final`, so the
/// answer is a function of the handle's identity.
pub fn mh_real_filter_compiled_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhFilterCompiled> {
    if crate::nbflags().dbg_mh_dispatch
        || !mh_is_real_handle(&*ctx, handle)
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .as_deref()
            != Some(BMH_SPECIES_LL)
    {
        return None;
    }
    let target = mh_real_obj(ctx.get_field_by_name(handle, "argL0"))?;
    let filter = mh_real_obj(ctx.get_field_by_name(handle, "argL1"))?;
    for h in [target, filter] {
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(h))
            .as_deref()
            != Some("java/lang/invoke/DirectMethodHandle")
        {
            return None;
        }
    }
    let form = mh_real_obj(ctx.get_field_by_name(handle, "form"))?;
    let position = mh_filter_transform_position(mh_real_transform_key(&*ctx, target, form)?)?;
    let species = real_handle_record(ctx, handle)?;
    if species.declared != site {
        return None;
    }
    let target_bind = mh_real_member_compiled_bind(ctx, target)?;
    let filter_bind = mh_real_member_compiled_bind(ctx, filter)?;
    if filter_bind.invoke_kind != 3 {
        return None;
    }
    Some(MhFilterCompiled {
        target: target_bind,
        filter: filter_bind,
        target_handle: target,
        position,
    })
}

/// [`mh_real_compiled_bind`] without the call site: the member of a REAL
/// `findStatic` / unbound `findVirtual` `DirectMethodHandle` at its own exact
/// type, by [`mh_compiled_bind_member`]'s rules (JIT round 21 wave 20, lane
/// mhfilter). Pure reads.
fn mh_real_member_compiled_bind(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
) -> Option<MhCompiledBind> {
    let record = real_handle_record(ctx, handle)?;
    let target = record.direct.as_ref()?;
    let (kind, own_type) = match target.ref_kind {
        6 => (MH_KIND_STATIC, target.desc.clone()),
        5 => (
            MH_KIND_VIRTUAL,
            format!("(L{};{}", target.class, target.desc.get(1..)?),
        ),
        _ => return None,
    };
    if record.declared != own_type {
        return None;
    }
    let lane = DirectLaneRecord {
        class: target.class.clone(),
        name: target.name.clone(),
        desc: target.desc.clone(),
        params: target.params.clone(),
        own_type: own_type.clone(),
        declared: Some(own_type),
        declared_is_own_type: true,
        own_type_aligned: true,
    };
    mh_compiled_bind_member(ctx, kind, Value::Object(None), None, &lane, target.owner)
}

/// The class a method descriptor's return type names (through any array
/// dimensions), or `None` for a primitive / `void` return or a malformed
/// descriptor. JIT round 20, lane dispatch: [`mh_compiled_bind_member`]'s
/// return-type screen.
pub(crate) fn mh_bind_return_class_name(desc: &str) -> Option<&str> {
    let ret = &desc[desc.rfind(')')? + 1..];
    ret.trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod jr20_mh_bind_return_class_tests {
    use super::mh_bind_return_class_name;

    #[test]
    fn jr20_mh_bind_return_class_name_reads_the_return_class_only() {
        assert_eq!(mh_bind_return_class_name("(I)Lp/X;"), Some("p/X"));
        assert_eq!(mh_bind_return_class_name("(Lp/A;)[[Lp/X;"), Some("p/X"));
        assert_eq!(mh_bind_return_class_name("(Lp/A;)V"), None);
        assert_eq!(mh_bind_return_class_name("(Lp/A;)[I"), None);
        assert_eq!(mh_bind_return_class_name("no-parens"), None);
    }
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_AMBIGUOUS_NAMES` -- performance, default ON,
/// `=0` restores the refusal (JIT round 20 wave 4, lane dispatch;
/// `r19-jdk-only-compiled-mh-link-rows-regressed-after-r18w12-CLOSED-20261012.md`).
/// The compiled `invokeExact` bind refused any REAL handle whose site named a
/// class two loaders define (`class_id_by_name` answers `None` for such a
/// name). So once a child-first loader had defined a same-named copy, every
/// call of a perfectly matching handle took the door: `R20DispMhLinkRows`
/// `same-loader-exact-top` measured 15 978 ns/call on w3f, against HotSpot's 71.
/// With the switch on, such a name is resolved as the member's loader sees it
/// (`mh_caller_sees` from the member's class, without loading). The
/// HotSpot-exact identity question stays where it already was: the fill's
/// `mh_constant_bind_loaders_agree` admits the bind only when the CALLER
/// resolves every class the site names (receiver, parameters, return) to the
/// same class as the member. That check is skipped under
/// `CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK=0`, so this switch is off there too.
/// Read once per admission of a real handle (cold fills only).
pub fn mh_constant_bind_ambiguous_names_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_AMBIGUOUS_NAMES")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK")
}

/// The member half of the compiled-bind admission, shared by
/// [`mh_compiled_bind_target_with`] and (JIT round 16 wave 15, lane mh6)
/// [`mh_compiled_bind_converting_target`]: the class and member questions
/// that decide whether a helper may call `record`'s member by name as the
/// door's leaf would (see [`mh_compiled_bind_target`]'s list). Moved out
/// unchanged. Pure reads.
///
/// `owner`: the member's declaring class BY IDENTITY (a REAL handle's
/// `MemberName.clazz`), or `None`. With it, and
/// [`mh_constant_bind_ambiguous_names_enabled`], a name two loaders define is
/// answered as the MEMBER's loader sees it instead of refusing the bind (JIT
/// round 20, lane dispatch; see that switch).
pub(crate) fn mh_compiled_bind_member(
    ctx: &mut dyn NativeContext,
    kind: i32,
    bound: Value,
    bound_receiver: Option<ObjectRef>,
    record: &DirectLaneRecord,
    owner: Option<ClassId>,
) -> Option<MhCompiledBind> {
    let class = record.class.as_str();
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| class.starts_with(p))
        || serialization_hook_neutral_result(&record.name, &record.desc, &[]).is_some()
    {
        return None;
    }
    // Read once, and only for a real handle (the only caller with an owner).
    let by_owner = owner.filter(|_| mh_constant_bind_ambiguous_names_enabled());
    let names_a_class = |ctx: &dyn NativeContext, name: &str| match ctx.classify_class_name(name) {
        cratonvm_native_api::NameLookup::Unique(cid) => !ctx.is_interface_class(cid),
        cratonvm_native_api::NameLookup::Ambiguous { .. } => by_owner
            .and_then(|o| mh_caller_sees(ctx, name, o))
            .is_some_and(|cid| !ctx.is_interface_class(cid)),
        cratonvm_native_api::NameLookup::Absent => false,
    };
    for p in &record.params {
        if p.starts_with('[') {
            return None;
        }
        if let Some(inner) = p.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            if !names_a_class(&*ctx, inner) {
                return None;
            }
        }
    }
    // JIT round 20, lane dispatch: the RETURN type is part of `invokeExact`'s
    // type identity too, and the admission compares the site's descriptor with
    // the handle's own type by NAME (`own_type != site`). A return class whose
    // name two loaders define can mean one class at the caller and another in
    // the handle's type: HotSpot throws `WrongMethodTypeException` there, a
    // bound site would call the target. The parameters are screened above
    // (`class_id_by_name` answers `None` for an ambiguous name); an ambiguous
    // return class (or array of one) is refused here, so the door keeps the
    // call and its loader-aware type check. Correctness, unconditional: it only
    // refuses binds.
    //
    // With `by_owner` (round 20 wave 4) an ambiguous return class is admitted
    // when the member's loader resolves it: the fill's loader check
    // (`mh_constant_bind_loaders_agree`) then requires the caller to resolve
    // it, and every other class the site names, to the SAME class.
    if let Some(ret) = mh_bind_return_class_name(&record.desc) {
        if ctx.classify_class_name(ret).is_ambiguous()
            && by_owner
                .and_then(|o| mh_caller_sees(&*ctx, ret, o))
                .is_none()
        {
            return None;
        }
    }
    let cid = match (ctx.classify_class_name(class), by_owner) {
        (cratonvm_native_api::NameLookup::Unique(cid), _) => cid,
        // The member's own class, by identity (its name is ambiguous).
        (cratonvm_native_api::NameLookup::Ambiguous { .. }, Some(o)) => o,
        _ => return None,
    };
    if ctx.is_interface_class(cid) {
        return None;
    }
    if let Some(receiver) = bound_receiver {
        if ctx.object_is_array(receiver) || !ctx.is_subclass(ctx.class_id_of_object(receiver), cid)
        {
            return None;
        }
    }
    if kind == MH_KIND_STATIC {
        if let Some(owner) = mh_static_owner(bound) {
            if owner != cid {
                return None;
            }
        }
    }
    const ACC_PRIVATE: u16 = 0x0002;
    const ACC_STATIC: u16 = 0x0008;
    let declared = ctx
        .declared_methods(cid)
        .into_iter()
        .find(|m| m.name == record.name && m.descriptor == record.desc)?;
    let is_static = declared.access_flags & ACC_STATIC != 0;
    let shape_ok = if kind == MH_KIND_STATIC {
        is_static
    } else {
        !is_static && declared.access_flags & ACC_PRIVATE == 0
    };
    if !shape_ok {
        return None;
    }
    Some(MhCompiledBind {
        invoke_kind: if kind == MH_KIND_STATIC { 3 } else { 0 },
        class_id: cid.as_u32(),
        class: record.class.clone(),
        name: record.name.clone(),
        desc: record.desc.clone(),
        bound: bound_receiver.is_some(),
    })
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE` -- perf, default ON since the JIT
/// round 18 close (`=0` off; measured 2551 -> 649 ns on `R17Mh4Virtual
/// exact-interface`); built JIT round 18 wave 11, lane mhresid; THROWS18-4 / the helper half of
/// MH417-1, `r18-mh-interface-handle-invokeexact-costs-microseconds-FIXED-20261007.md`):
/// a compiled `invokeExact` / `invoke` site whose constant handle is an
/// unbound `findVirtual` of an INTERFACE method binds to an `invokeinterface`
/// of that method ([`mh_compiled_bind_interface_target`]), guarded per call
/// on the receiver's class implementing the interface (`vm/src/jit/helpers.rs`,
/// `mh_constant_bind_redirect`). Read on the dispatch helper's cold fill only.
pub fn mh_constant_bind_interface_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE")
}

/// The interface half of the compiled-bind admission (JIT round 18 wave 11,
/// lane mhresid; [`mh_constant_bind_interface_enabled`]), asked only after
/// [`mh_compiled_bind_target`] refused. `Some` (an `invoke_kind` `2` call of
/// the member, `class_id` the interface's) for an UNBOUND virtual handle of an
/// interface method -- the carrier's `MH_KIND_VIRTUAL` or a real
/// `DirectMethodHandle$Interface` (ref kind 9) -- at its own exact type, with
/// the exact bind's handle and parameter rules (no marking bit, not a
/// downcall, no `specialCaller`, no array parameter, every reference
/// parameter a loaded non-interface class, not a `java/` / `jdk/` / `sun/`
/// member, not a serialization hook), declared by the interface itself,
/// neither static nor private.
///
/// The exact bind refuses an interface member because the verifier does not
/// check interface assignability: a receiver need not implement the interface,
/// and the door's answer for such a receiver is a refusal (HotSpot's
/// `DirectMethodHandle.Interface.checkReceiver`). The bind therefore takes only
/// a receiver whose class implements the interface, tested per call by the
/// helper; a null receiver or any other one takes the door, unchanged.
///
/// Pure reads; no allocation, no GC point.
pub fn mh_compiled_bind_interface_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let lane = if mh_is_real_handle(&*ctx, handle) {
        let record = real_handle_record(ctx, handle)?;
        let target = record.direct.as_ref()?;
        if target.ref_kind != 9 {
            return None;
        }
        let own_type = format!("(L{};{}", target.class, target.desc.get(1..)?);
        if record.declared != own_type || own_type != site {
            return None;
        }
        DirectLaneRecord {
            class: target.class.clone(),
            name: target.name.clone(),
            desc: target.desc.clone(),
            params: target.params.clone(),
            own_type: own_type.clone(),
            declared: Some(own_type),
            declared_is_own_type: true,
            own_type_aligned: true,
        }
    } else {
        if ctx.object_num_fields(handle) <= MH_STATIC_OWNER
            || ctx
                .class_name_of_id(ctx.class_id_of_object(handle))
                .as_deref()
                != Some("java/lang/invoke/MethodHandle")
        {
            return None;
        }
        if !matches!(ctx.get_field(handle, MH_KIND), Value::Int(k) if k == MH_KIND_VIRTUAL) {
            return None;
        }
        if mh_marking_bits(&*ctx, handle) != 0 || crate::panama::is_downcall_handle(&*ctx, handle) {
            return None;
        }
        if !matches!(
            ctx.get_field(handle, MH_SPECIAL_CALLER),
            Value::Object(None)
        ) || !matches!(ctx.get_field(handle, MH_BOUND), Value::Object(None))
        {
            return None;
        }
        let fields = [
            ctx.get_field(handle, MH_CLASS),
            ctx.get_field(handle, MH_NAME),
            ctx.get_field(handle, MH_DESC),
            ctx.get_field_by_name(handle, "type"),
        ];
        let record = mh_direct_lane_decode(ctx, fields, true, Some(site))?;
        if !record.declared_is_own_type || !record.own_type_aligned || record.own_type != site {
            return None;
        }
        record
    };
    mh_compiled_bind_interface_member(ctx, &lane)
}

/// [`mh_compiled_bind_interface_target`]'s member questions (the exact bind's
/// [`mh_compiled_bind_member`], for an interface class).
pub(crate) fn mh_compiled_bind_interface_member(
    ctx: &mut dyn NativeContext,
    record: &DirectLaneRecord,
) -> Option<MhCompiledBind> {
    let class = record.class.as_str();
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| class.starts_with(p))
        || serialization_hook_neutral_result(&record.name, &record.desc, &[]).is_some()
    {
        return None;
    }
    for p in &record.params {
        if p.starts_with('[') {
            return None;
        }
        if let Some(inner) = p.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            let names_a_class = ctx
                .class_id_by_name(inner)
                .is_some_and(|cid| !ctx.is_interface_class(cid));
            if !names_a_class {
                return None;
            }
        }
    }
    let cid = ctx.class_id_by_name(class)?;
    if !ctx.is_interface_class(cid) {
        return None;
    }
    const ACC_PRIVATE: u16 = 0x0002;
    const ACC_STATIC: u16 = 0x0008;
    let declared = ctx
        .declared_methods(cid)
        .into_iter()
        .find(|m| m.name == record.name && m.descriptor == record.desc)?;
    if declared.access_flags & (ACC_STATIC | ACC_PRIVATE) != 0 {
        return None;
    }
    Some(MhCompiledBind {
        invoke_kind: 2,
        class_id: cid.as_u32(),
        class: record.class.clone(),
        name: record.name.clone(),
        desc: record.desc.clone(),
        bound: false,
    })
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL` -- performance, default OFF
/// (interpreter round i5 wave 109, lane L2): statically bound handles on the
/// compiled paths. A compiled `MethodHandle.linkToSpecial` (what
/// `DirectMethodHandle$Holder.invokeSpecial` ends in once
/// `CRATONVM_MHN_RESOLVE_DIRECT_CALL` settles a final / private virtual as
/// `REF_invokeSpecial`, and every `findSpecial` / constructor form) calls its
/// `MemberName`'s method as a compiled `invokespecial`
/// (`vm/src/jit/helpers.rs` `mh_compiled_link_target`), and a constant
/// `invokeExact` site binds a real `$Special` handle, or a plain one over a
/// private method, to that `invokespecial`
/// ([`mh_compiled_bind_special_target`]). Read per call (a `NativeFlags`
/// field load).
pub fn jit_mh_compiled_link_special_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_special
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_BY_ID` (interpreter round i6 wave 116,
/// lane L2; default off, performance): a compiled `invokeBasic` whose
/// `form.vmentry` is a hidden or name-ambiguous class's method calls its
/// published compiled body by class id (`vm/src/jit/helpers.rs`
/// `mh_compiled_basic_by_id`; `MhCompiledMember::bind_by_id`). Read per
/// refused link (a `NativeFlags` field load).
pub fn jit_mh_compiled_link_by_id_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_by_id
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID` (interpreter round i6 wave
/// 122, lane L2; default off, performance; page
/// `i100-L4-proposal-method-handle-warm-call-directions`, item 2): the
/// by-id admission of [`jit_mh_compiled_link_by_id_enabled`] for a compiled
/// `invokeExact` site's linker (`vm/src/jit/helpers.rs`
/// `mh_compiled_linker_by_id`), whose `MemberName` is a hidden spun form the
/// by-name link refuses. Read per refused link (a `NativeFlags` field load).
pub fn jit_mh_compiled_link_linker_by_id_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_linker_by_id
}

/// The statically bound half of the compiled-bind admission (interpreter round
/// i5 wave 109, lane L2; [`jit_mh_compiled_link_special_enabled`]), asked by the
/// dispatch helper's cold fill only after the exact and interface admissions
/// refused. `Some` (an `invoke_kind` `1` call -- `invokespecial` -- of the
/// member, `class_id` its declaring class) for a REAL `DirectMethodHandle`
/// whose member is
///
/// * `REF_invokeSpecial` on a `$Special` handle (`findSpecial` /
///   `unreflectSpecial` with the declaring class as the special caller: the
///   handle's type must be the member's own with `L<class>;` in front, so a
///   special caller other than the declaring class is refused), or
/// * a private instance method on a plain handle (`unreflect`, `findVirtual`
///   from a nestmate: HotSpot binds it without a vtable either),
///
/// called at its own exact type, with the exact bind's member rules: not a
/// `java/` / `jdk/` / `sun/` class, not a serialization hook, no array
/// parameter, every reference parameter a uniquely named non-interface class,
/// a return class whose name one loader defines, a non-interface declaring
/// class the name resolves to uniquely (and to the handle's own `clazz`),
/// the member declared there and not static, no constructor. A null receiver
/// stays the door's (the helper's warm path screens it per call); the
/// receiver's type is the verified site's, as for the exact bind.
///
/// Pure reads; no allocation, no GC point.
pub fn mh_compiled_bind_special_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<MhCompiledBind> {
    const ACC_PRIVATE: u16 = 0x0002;
    const ACC_STATIC: u16 = 0x0008;
    if crate::nbflags().dbg_mh_dispatch || !mh_is_real_handle(&*ctx, handle) {
        return None;
    }
    let record = real_handle_record(ctx, handle)?;
    let target = record.direct.as_ref()?;
    if !matches!(target.ref_kind, 5 | 7) || target.name.starts_with('<') {
        return None;
    }
    let own_type = format!("(L{};{}", target.class, target.desc.get(1..)?);
    if record.declared != own_type || own_type != site {
        return None;
    }
    let class = target.class.as_str();
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| class.starts_with(p))
        || serialization_hook_neutral_result(&target.name, &target.desc, &[]).is_some()
    {
        return None;
    }
    for p in &target.params {
        if p.starts_with('[') {
            return None;
        }
        if let Some(inner) = p.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            let names_a_class = ctx
                .class_id_by_name(inner)
                .is_some_and(|cid| !ctx.is_interface_class(cid));
            if !names_a_class {
                return None;
            }
        }
    }
    if let Some(ret) = mh_bind_return_class_name(&target.desc) {
        if ctx.classify_class_name(ret).is_ambiguous() {
            return None;
        }
    }
    let cid = ctx.class_id_by_name(class)?;
    if target.owner.is_some_and(|o| o != cid) || ctx.is_interface_class(cid) {
        return None;
    }
    let declared = ctx
        .declared_methods(cid)
        .into_iter()
        .find(|m| m.name == target.name && m.descriptor == target.desc)?;
    if declared.access_flags & ACC_STATIC != 0
        || (target.ref_kind == 5 && declared.access_flags & ACC_PRIVATE == 0)
    {
        return None;
    }
    Some(MhCompiledBind {
        invoke_kind: 1,
        class_id: cid.as_u32(),
        class: target.class.clone(),
        name: target.name.clone(),
        desc: target.desc.clone(),
        bound: false,
    })
}

// ---------------------------------------------------------------------------
// The converting constant bind (JIT round 16 wave 15, lane mh6; proposal MHB-1
// of `jit-r16-mhbind-proposals-RETIRED-20261006.md`)
// ---------------------------------------------------------------------------
//
// `R16MhFloor` `exact-astyped` (`twice(int)int.asType((Object)Object)`, called
// through `invokeExact`) and `invoke-astype` (`twice.invoke((Object) Integer)`)
// cost 2748 / 3397 ns on Linux against HotSpot's 56 / 59: the constant bind
// refuses both (the declared or the call-site type is not the member's own),
// so every call pays the native door's converting lane. HotSpot's cost there
// is its `LambdaForm`: an inline unbox, the call, an inline box.
//
// This admission lets a compiled site bind such a handle too, when the only
// difference between the type the call's arguments arrive in and the member's
// own type is one of a closed set of `int` conversions, position by position:
//
// * the same token (a reference one obeys the exact bind's rules);
// * `Integer`, `Number` or `Object` into an `int` parameter: the helper
//   unboxes an argument whose class is EXACTLY `java/lang/Integer` and sends
//   every other value (a `null`, any other class) to the door, which raises
//   or converts as before;
// * an `int` return into an `Integer`, `Number` or `Object` return: the helper
//   boxes through the VM's `Integer.valueOf` (the box cache's identity, as
//   `asType`'s `ValueConversions.boxInteger` keeps it).
//
// The arguments arrive in that type straight from the member's own, in ONE
// `asType` hop, in exactly two shapes:
//
// * `invokeExact` (or `invoke`) at a stamped copy's declared type, whose cast
//   chain is exactly one non-explicit link naming the member's own type (a
//   copy stamped over a copy, or an `explicitCastArguments` one, is refused:
//   its middle link's casts are not the direct conversion's);
// * `invoke` at a call-site type that is not the declared one, on a handle
//   whose declared type IS the member's own: `invoke` is `asType(site)` then
//   `invokeExact`, so the conversion is the site's into the member's.
//
// For an exact `Integer` argument, each admitted conversion is what the door
// does: `adapt_leaf_args` reads the wrapper's `value`, and
// `box_return_against_target` boxes through `Integer.valueOf`. Everything the
// door could raise for such a call (a `WrongMethodTypeException` for another
// `invokeExact` type, a cast or unboxing refusal) needs a value the helper
// does not take, or a type the admission refuses.

/// What a converting bound site does around its member's call (JIT round 16
/// wave 15, lane mh6; MHB-1, [`mh_compiled_bind_converting_target`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhBindConversion {
    /// Bit `i`: the member's `i`-th argument word (an unbound virtual
    /// handle's receiver is word 0) is an `int` the caller passes as a
    /// reference, to be unboxed from an exact `java/lang/Integer`.
    pub unbox_int: u32,
    /// The member returns `int` and the caller expects a reference: box it
    /// through `Integer.valueOf`.
    pub box_int_return: bool,
    /// `java/lang/Integer`'s class id, which an unboxed argument's class must
    /// be exactly.
    pub integer_class_id: u32,
}

/// The widest member a converting bind takes (one bit per argument word).
pub(crate) const MH_BIND_CONVERSION_MAX_WORDS: usize = 16;

/// Is `token` a reference type an `Integer` value converts from into `int`,
/// and an `int` boxes into, as one plain unbox / `Integer.valueOf`?
pub(crate) fn mh_bind_int_wrapper_token(token: &str) -> bool {
    matches!(
        token,
        "Ljava/lang/Integer;" | "Ljava/lang/Number;" | "Ljava/lang/Object;"
    )
}

/// The conversions from the call's type `from` into the member's own type
/// `to` (the two [`MhBindConversion`] fields without the class id); `None`
/// when some position needs anything outside the closed set, or when nothing
/// converts (the exact bind's case).
pub(crate) fn mh_bind_conversions(from: &str, to: &str) -> Option<(u32, bool)> {
    let (from_params, from_ret) = split_descriptor_params(from)?;
    let (to_params, to_ret) = split_descriptor_params(to)?;
    if from_params.len() != to_params.len() || to_params.len() > MH_BIND_CONVERSION_MAX_WORDS {
        return None;
    }
    let mut unbox = 0u32;
    for (i, (f, t)) in from_params.iter().zip(&to_params).enumerate() {
        if f == t {
            continue;
        }
        if t == "I" && mh_bind_int_wrapper_token(f) {
            unbox |= 1 << i;
            continue;
        }
        return None;
    }
    let box_return = if from_ret == to_ret {
        false
    } else if to_ret == "I" && mh_bind_int_wrapper_token(&from_ret) {
        true
    } else {
        return None;
    };
    if unbox == 0 && !box_return {
        return None;
    }
    Some((unbox, box_return))
}

/// May a compiled `invokeExact` (`invoke_door == false`) or `invoke` site of
/// descriptor `site` call `handle`'s member directly, with the conversions of
/// [`MhBindConversion`] around the call (JIT round 16 wave 15, lane mh6;
/// MHB-1; the section comment above)? Asked only after
/// [`mh_compiled_bind_target`] refused, so a site at the member's own type
/// never needs it. The handle questions are the exact bind's (an UNBOUND
/// `findStatic` / `findVirtual` handle, no marking bit, not a downcall, no
/// `specialCaller`), and so are the member questions
/// ([`mh_compiled_bind_member`]).
///
/// Pure reads; no allocation, no GC point.
pub fn mh_compiled_bind_converting_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
    invoke_door: bool,
) -> Option<(MhCompiledBind, MhBindConversion)> {
    if crate::nbflags().dbg_mh_dispatch || ctx.object_num_fields(handle) <= MH_STATIC_OWNER {
        return None;
    }
    if ctx
        .class_name_of_id(ctx.class_id_of_object(handle))
        .as_deref()
        != Some("java/lang/invoke/MethodHandle")
    {
        return None;
    }
    let kind = match ctx.get_field(handle, MH_KIND) {
        Value::Int(k) if k == MH_KIND_STATIC || k == MH_KIND_VIRTUAL => k,
        _ => return None,
    };
    if mh_marking_bits(&*ctx, handle) != 0 || crate::panama::is_downcall_handle(&*ctx, handle) {
        return None;
    }
    if !matches!(
        ctx.get_field(handle, MH_SPECIAL_CALLER),
        Value::Object(None)
    ) {
        return None;
    }
    // Unbound only: `MH_BOUND` holds nothing, or a static handle's recorded
    // owner (an `Int`).
    let bound = ctx.get_field(handle, MH_BOUND);
    if matches!(bound, Value::Object(Some(_))) {
        return None;
    }
    let fields = [
        ctx.get_field(handle, MH_CLASS),
        ctx.get_field(handle, MH_NAME),
        ctx.get_field(handle, MH_DESC),
        ctx.get_field_by_name(handle, "type"),
    ];
    let record = mh_direct_lane_decode(ctx, fields, kind == MH_KIND_VIRTUAL, None)?;
    if !record.own_type_aligned {
        return None;
    }
    let declared = record.declared.as_deref()?;
    if site == declared {
        // A stamped copy, one `asType` hop off the member's own type.
        if record.declared_is_own_type {
            return None;
        }
        let chain = parse_cast_chain(&mh_cast_chain_text(&*ctx, handle)?)?;
        let one_hop = chain.bound_refusal.is_none()
            && chain.links.len() == 1
            && !chain.links[0].explicit
            && chain.links[0].desc == record.own_type;
        if !one_hop {
            return None;
        }
    } else if !(invoke_door && record.declared_is_own_type) {
        // `invokeExact` at another type is the door's
        // `WrongMethodTypeException`; `invoke` on a stamped copy at another
        // type is two hops.
        return None;
    }
    let (unbox_int, box_int_return) = mh_bind_conversions(site, &record.own_type)?;
    let integer = ctx.class_id_by_name("java/lang/Integer")?;
    let bind = mh_compiled_bind_member(ctx, kind, bound, None, &record, None)?;
    Some((
        bind,
        MhBindConversion {
            unbox_int,
            box_int_return,
            integer_class_id: integer.as_u32(),
        },
    ))
}

// ---------------------------------------------------------------------------
// The field-accessor link (SBR wave 5, lane reflacc)
// ---------------------------------------------------------------------------
//
// JDK 18+ `Field.get`/`set` and the sixteen typed accessors are
// `MethodHandle{Integer,Long,Object,...}FieldAccessorImpl` over a field
// getter/setter `MethodHandle` retyped to `(Object)int`, `(Object,int)void`,
// `()int`, ... (`MethodHandleAccessorFactory.newFieldAccessor`), and every
// access is one `invokeExact` on that handle. HotSpot links the handle once
// (a `DirectMethodHandle` whose `LambdaForm` is `checkBase` + `Unsafe.getInt`)
// and the call is a field load. Here every `invokeExact` went through the
// general door: render the handle's `MethodType` to a descriptor, read the
// leaf's class/name/descriptor strings, resolve the field slot by name, judge
// the receiver cast by class name, box the primitive result and unbox it again
// in the VM -- measured 5.0-5.5 us per `getInt` against 1.1-1.2 us for the
// `Field.getInt` native it had to replace (`tools/probes/SbrW5ReflAccBench.java`,
// rows `getIntAcc` / `mhExact`), which is why `Field`'s accessors were held
// in SBR wave 3.
//
// The link is that one-time resolution, remembered ON THE HANDLE: the field
// slot, the receiver class it was admitted for (or the initialized owner of a
// static), the admitted value class of a reference setter, and the declared
// descriptor the exact call-site check compares against. A linked call is
// then a handful of slot reads, the exact-descriptor compare, and the field
// load/store the door's own leaf arm would have made -- the same slot, the
// same `get_field`/`set_field`, on a receiver the door would have admitted.
//
// Everything the link does not cover falls through to the door, unchanged:
// a null receiver (the door's message-less NPE), a receiver or value of any
// other class (the door's cast, `ClassCastException`), a call site that is not
// the declared type (the door's `WrongMethodTypeException`), a bound, marked,
// cast-chained or retyped handle, and every kind but `GETTER`/`SETTER`.
// (Wave 66, `CRATONVM_MH_FIELD_LINK_BOUND`: a bound instance accessor links
// on [`mh_field_link_try_bound`]'s rules, under [`FL_BOUND`].) The
// link is written only for a call the door is about to serve normally and
// never changes what the door answers.
//
// `--jdk-only`, and `--compatible` under `CRATONVM_MH_FIELD_LINK_COMPATIBLE`
// (interpreter round i2 wave 61, lane L4; [`mh_field_link_mode_admits`]):
// `--compatible` answers `Field`'s accessors with natives, but every
// framework accessor over a field `MethodHandle` reaches this door there too.
// Kill switch `CRATONVM_MH_FIELD_LINK=0` (default on).

/// Packed link state (an `Int`): bits 0-15 the field slot, 16-21 the index
/// of `MethodHandle.type`, then the `FL_*` flags. `0` = not linked.
/// Width-guarded like [`MH_VARARGS`]: only [`alloc_method_handle`] mints the
/// link slots, and a narrower handle is never linked.
pub(crate) const MH_FIELD_LINK: usize = MH_BASE + 10;
/// The `MethodType` the link was made under. A linked call requires the
/// handle's `type` to still be this object, so an in-place restamp of `type`
/// (whatever writes it) unlinks the handle rather than inheriting a link made
/// for another signature.
pub(crate) const MH_FIELD_LINK_TYPE: usize = MH_BASE + 11;
/// Instance: the receiver class id the door admitted when the link was made
/// (the receiver must be exactly that class and not an array). Static: the
/// initialized class whose static slot is read or written.
pub(crate) const MH_FIELD_LINK_CLASS: usize = MH_BASE + 12;
/// A reference setter whose leaf value type is not `Object`: the value class
/// id the door admitted (a value must be null or exactly that non-array class).
pub(crate) const MH_FIELD_LINK_VALUE: usize = MH_BASE + 13;
/// The declared descriptor, as a [`field_link_site_id`] id.
pub(crate) const MH_FIELD_LINK_SITE: usize = MH_BASE + 14;
/// Slots [`alloc_method_handle`] allocates. Still below
/// `panama::DOWNCALL_BASE`, so [`crate::panama::is_downcall_handle`]'s width
/// screen keeps rejecting every handle minted here.
pub(crate) const MH_SLOT_COUNT: usize = MH_FIELD_LINK_SITE + 1;

pub(crate) const FL_LINKED: i32 = 1 << 24;
pub(crate) const FL_STATIC: i32 = 1 << 25;
pub(crate) const FL_SETTER: i32 = 1 << 26;
pub(crate) const FL_VALUE_CHECK: i32 = 1 << 27;
/// Bits 28-30: how many times this handle has been (re)linked. A handle whose
/// receiver class keeps changing stops relinking after [`FL_MAX_LINKS`] and is
/// served by the door, so a megamorphic accessor costs the door, not the door
/// plus a relink per call.
pub(crate) const FL_LINKS_SHIFT: u32 = 28;
pub(crate) const FL_MAX_LINKS: i32 = 7;
/// Bit 31: bits 0-15 are the instance slot the handle carries FROM MINT
/// (interpreter round i2 wave 64, lane L4; `CRATONVM_MH_FIELD_SLOT_FROM_MINT`,
/// [`mh_record_minted_field_slot`]), with or without `FL_LINKED`.
pub(crate) const FL_MINT_SLOT: i32 = i32::MIN;
/// Bit 23 (interpreter round i2 wave 65, lane L4;
/// `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`): an INSTANCE link whose
/// [`MH_FIELD_LINK_CLASS`] is the handle's recorded OWNER, admitting a
/// receiver of the owner or of any subclass of it ([`field_link_receiver_admitted`]).
/// Taken from the type index, which was therefore 7 bits (6 since wave 66, [`FL_BOUND`]) ([`FL_TYPE_INDEX_MASK`]):
/// `MethodHandle.type`'s index is a small constant (one of the few instance
/// fields `MethodHandle` itself declares, below the synthetic window at
/// [`MH_BASE`]), so no link ever had a larger one.
pub(crate) const FL_OWNER_ADMITS: i32 = 1 << 23;
/// Bit 22 (interpreter round i2 wave 66, lane L4; `CRATONVM_MH_FIELD_LINK_BOUND`):
/// a link of a BOUND instance getter / setter ([`mh_field_link_try_bound`]).
/// The receiver is `MH_BOUND`'s object, never an argument: a getter's call
/// passes nothing and a setter's the value alone. [`MH_FIELD_LINK_CLASS`] is
/// the bound receiver's exact class, re-checked per call. Taken from the type
/// index, as [`FL_OWNER_ADMITS`] was (now 6 bits; a larger index never links).
pub(crate) const FL_BOUND: i32 = 1 << 22;
/// Bit 21 (interpreter round i3 wave 79, lane L4; page
/// `i71-L4-field-setter-handle-on-a-volatile-field-stores-without-the-volatile-fence`):
/// the linked field is `volatile` ([`field_link_field_is_volatile`], judged once
/// by the derivation). The fast path then stores as `VarHandle.setVolatile`
/// does ([`field_handle_volatile_store`]: a SeqCst fence on each side) and
/// follows a load with an acquire fence, as HotSpot's accessor form for a
/// volatile field (`putXxxVolatile` / `getXxxVolatile`) does. Taken from the
/// type index, as [`FL_BOUND`] was (now 5 bits; [`MH_BASE`] is 16, and
/// `MethodHandle.type` is declared below it).
pub(crate) const FL_VOLATILE: i32 = 1 << 21;
/// Bits 16-20: the index of `MethodHandle.type` (after `>> 16`).
pub(crate) const FL_TYPE_INDEX_MASK: i32 = 0x1F;

/// `ACC_VOLATILE` (JVMS 4.5), as `MemberName.flags` and a field's access flags
/// carry it.
pub(crate) const FIELD_ACC_VOLATILE: i32 = 0x0040;

/// A field handle's store of a `volatile` field (interpreter round i3 wave 79,
/// lane L4): `store` between two SeqCst fences, [`varhandle_set_volatile`]'s
/// rule (the trailing fence is the StoreLoad barrier a Dekker handshake through
/// two setter handles relies on); a plain store otherwise. Every field handle
/// path that stores -- the door's `SETTER` arm, the field-accessor link
/// ([`mh_field_link_fast`], [`mh_field_link_serve_written`]) and the real
/// accessor ([`mh_real_field_access_resolved`]) -- stores through this.
#[inline]
pub(crate) fn field_handle_volatile_store<R>(volatile: bool, store: impl FnOnce() -> R) -> R {
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    let r = store();
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    r
}

/// After a field handle's load of a `volatile` field: an acquire fence (free
/// on x86-64, where a plain load is already an acquire load; a barrier on a
/// weakly ordered target). Wave 79, lane L4; see [`field_handle_volatile_store`].
#[inline]
pub(crate) fn field_handle_volatile_load_fence(volatile: bool) {
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    }
}

/// Is the field a link resolves `volatile` (wave 79, lane L4; [`FL_VOLATILE`])?
/// Searched from `start` (a static's holder, an instance link's owner or
/// receiver class) up the superclass chain for `name` of the given
/// static-ness; an instance field is matched by its `slot` first (a subclass
/// may shadow the name), then by name alone. `false` when no class of the
/// chain declares it (a lambda proxy's capture, which is `final`). Called once
/// per link derivation, never on a linked call. Reads only.
pub(crate) fn field_link_field_is_volatile(
    ctx: &dyn NativeContext,
    start: ClassId,
    name: &str,
    is_static: bool,
    slot: usize,
) -> bool {
    let mut by_name: Option<u16> = None;
    let mut cur = Some(start);
    for _ in 0..64 {
        let Some(cid) = cur else {
            break;
        };
        for f in ctx.declared_fields(cid) {
            if f.name != name || f.is_static != is_static {
                continue;
            }
            if is_static || f.slot_index == slot {
                return f.access_flags & (FIELD_ACC_VOLATILE as u16) != 0;
            }
            by_name.get_or_insert(f.access_flags);
        }
        cur = ctx.superclass_of(cid);
    }
    by_name.is_some_and(|flags| flags & (FIELD_ACC_VOLATILE as u16) != 0)
}

/// `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS` (default on, performance; wave 65, lane
/// L4; proposal `i64-L4-proposal-field-link-admits-the-owners-subclasses`).
pub(crate) fn mh_field_link_owner_admits_enabled() -> bool {
    crate::nbflags().mh_field_link_owner_admits
}

/// `CRATONVM_MH_FIELD_LINK_BOUND` (default on since the wave-66 triage, performance; wave 66, lane L4;
/// proposal `i65-L4-proposal-bound-field-handles-link-too`): link a bound
/// instance field getter / setter too ([`mh_field_link_try_bound`]), and mint
/// `bindTo`'s copy of an owned field handle wide enough to hold the link.
pub(crate) fn mh_field_link_bound_enabled() -> bool {
    crate::nbflags().mh_field_link_bound
}

/// Does `bindTo`'s copy carrying `carry` need [`alloc_method_handle`]'s width?
/// For a minted slot (wave 65), and under `CRATONVM_MH_FIELD_LINK_BOUND` for
/// any owned field copy, so the bound link has its slots (`bound_link`).
pub(crate) fn bound_field_copy_needs_link_slots(carry: Option<BoundFieldCarry>, bound_link: bool) -> bool {
    carry.is_some_and(|c| c.needs_link_slots() || bound_link)
}

/// The value type a BOUND instance field link checks per call, from the
/// member's descriptor split (`leaf_*`, receiver first) and the bound
/// handle's declared type (`decl_*`, receiver dropped): `Some(None)` nothing
/// beyond the receiver (a getter; a primitive or `Object` setter value),
/// `Some(Some(t))` a setter's reference value of leaf type `t`, and `None`
/// for a shape the door does not serve by a plain field access (a static or
/// misshapen descriptor, an arity or primitive mismatch, a converted return).
/// The unbound link's rules, one parameter shorter. Pure.
pub(crate) fn bound_field_link_value_type<'a>(
    setter: bool,
    leaf_params: &'a [String],
    leaf_ret: &str,
    decl_params: &[String],
    decl_ret: &str,
) -> Option<Option<&'a str>> {
    let is_ref = |t: &str| t.starts_with('L') || t.starts_with('[');
    let arity = if setter { 2 } else { 1 };
    if leaf_params.len() != arity || decl_params.len() + 1 != arity {
        return None;
    }
    if !leaf_params.first().is_some_and(|r| is_ref(r)) {
        return None;
    }
    if !setter {
        let same = leaf_ret == decl_ret;
        let object_return = is_ref(leaf_ret) && decl_ret == "Ljava/lang/Object;";
        return ((same || object_return) && leaf_ret != "V").then_some(None);
    }
    if leaf_ret != "V" || decl_ret != "V" {
        return None;
    }
    let (leaf_v, decl_v) = (leaf_params[1].as_str(), decl_params[0].as_str());
    if is_ref(leaf_v) != is_ref(decl_v) || (!is_ref(leaf_v) && leaf_v != decl_v) {
        return None;
    }
    Some((is_ref(leaf_v) && leaf_v != "Ljava/lang/Object;").then_some(leaf_v))
}

/// Does linked handle `this` (packed `state`, linked receiver class or owner
/// `class_raw`) admit non-null `receiver` on the fast path? An exact class
/// always; under [`FL_OWNER_ADMITS`] also a subclass of the owner by id: the
/// one remembered in [`MH_FIELD_LINK_VALUE`] (a getter's, or a setter's
/// without a value check, where that slot is otherwise unused), else asked of
/// the hierarchy and remembered. Never an array (its header carries its
/// component's class id), and never another loader's namesake (not a subclass
/// by id), which the door refuses. Writes only the memo; cannot throw.
pub(crate) fn field_link_receiver_admitted(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    state: i32,
    class_raw: i32,
    receiver: ObjectRef,
) -> bool {
    if ctx.object_is_array(receiver) {
        return false;
    }
    let rc = ctx.class_id_of_object(receiver).as_u32() as i32;
    if rc == class_raw {
        return true;
    }
    if state & FL_OWNER_ADMITS == 0 || rc == 0 {
        return false;
    }
    let memo = state & FL_VALUE_CHECK == 0;
    if memo && matches!(ctx.get_field(this, MH_FIELD_LINK_VALUE), Value::Int(raw) if raw == rc) {
        return true;
    }
    if !ctx.is_subclass(ClassId::new(rc as u32), ClassId::new(class_raw as u32)) {
        return false;
    }
    if memo {
        ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(rc));
    }
    true
}

/// `CRATONVM_MH_FIELD_LINK` -- default ON (SBR wave 5, lane reflacc);
/// `0`/`false`/`off`/`no` sends every field-handle `invokeExact` through the
/// door, as before.
pub(crate) fn mh_field_link_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_FIELD_LINK"))
}

/// Whether this VM's mode links field handles: `--jdk-only` always;
/// `--compatible` with `CRATONVM_MH_FIELD_LINK_COMPATIBLE` (interpreter round
/// i2 wave 61, lane L4; proposal
/// `interpreter-L4-proposal-link-field-handles-under-compatible-too-FIXED-20261025`). The link never
/// changes an answer (see the section comment), and since wave 60's
/// `CRATONVM_MH_COMPATIBLE_RECORD_OWNER` a `--compatible` static handle
/// records its owner, which the static arms require; a handle minted without
/// one is still left to the door.
pub(crate) fn mh_field_link_mode_admits(ctx: &dyn NativeContext) -> bool {
    // `JdkInvoke` (nio21 wave 12, L3): the row takes `--jdk-only`'s arm.
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
        || crate::nbflags().mh_field_link_compatible
}

/// Declared descriptors a link compares a call site against, interned so a
/// linked call compares one string without rendering the `MethodType`. A
/// content-addressed cache of descriptor TEXT, not compatibility state: the
/// same text has the same id in every VM of the process, and an id is only
/// ever compared with the text it was issued for.
#[derive(Default)]
pub(crate) struct FieldLinkSites {
    pub(crate) ids: rustc_hash::FxHashMap<Arc<str>, u32>,
    pub(crate) names: Vec<Arc<str>>,
}

/// Bound on [`FieldLinkSites`]: past it nothing new is linked (the door
/// serves those handles), so a program minting descriptors without end cannot
/// grow the table without end.
pub(crate) const FIELD_LINK_SITES_MAX: usize = 1 << 16;

pub(crate) fn field_link_sites() -> &'static parking_lot::RwLock<FieldLinkSites> {
    static SITES: std::sync::OnceLock<parking_lot::RwLock<FieldLinkSites>> =
        std::sync::OnceLock::new();
    SITES.get_or_init(Default::default)
}

/// The id of `desc` in [`FieldLinkSites`], interning it; `None` when full.
pub(crate) fn field_link_site_id(desc: &str) -> Option<u32> {
    if let Some(&id) = field_link_sites().read().ids.get(desc) {
        return Some(id);
    }
    let mut sites = field_link_sites().write();
    if let Some(&id) = sites.ids.get(desc) {
        return Some(id);
    }
    if sites.names.len() >= FIELD_LINK_SITES_MAX {
        return None;
    }
    let id = u32::try_from(sites.names.len()).ok()?;
    let text: Arc<str> = Arc::from(desc);
    sites.names.push(text.clone());
    sites.ids.insert(text, id);
    Some(id)
}

/// Lines of the per-thread [`FIELD_LINK_SITE_TEXTS`] (direct-mapped by id).
pub(crate) const FIELD_LINK_SITE_TEXT_LINES: usize = 64;

thread_local! {
    /// Interpreter round i4 wave 91, lane L4
    /// (`CRATONVM_MH_FIELD_LINK_SITE_BY_ARC`, proposal
    /// `i71-L4-proposal-linked-calls-judge-the-site-by-its-shared-text`): this
    /// thread's copies of [`FieldLinkSites`] entries, `(id, the table's own
    /// Arc)`, so a linked call compares its site with the interned text
    /// without the table's `RwLock`. Exact by construction: the table is
    /// append-only and never rewrites an id's text, so a copied `(id, text)`
    /// pair stays the table's answer for the life of the process. Holding the
    /// `Arc` also keeps the text's address from being reused, so the pointer
    /// test in [`field_link_site_matches`] cannot name other text. Not
    /// compatibility state: a content-addressed cache, like the table itself.
    pub(crate) static FIELD_LINK_SITE_TEXTS: std::cell::RefCell<Vec<Option<(u32, Arc<str>)>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`field_link_site_matches`] under `CRATONVM_MH_FIELD_LINK_SITE_BY_ARC`:
/// answered from this thread's copy of the interned text when it holds `id`,
/// else from the table (under its read lock, once) and copied.
pub(crate) fn field_link_site_matches_local(id: usize, site: &str) -> bool {
    let Ok(id32) = u32::try_from(id) else {
        return false;
    };
    let line = id & (FIELD_LINK_SITE_TEXT_LINES - 1);
    let cached = FIELD_LINK_SITE_TEXTS.with(|lines| {
        let lines = lines.borrow();
        match lines.get(line) {
            Some(Some((cached_id, text))) if *cached_id == id32 => {
                // Same allocation, same length: the very text. Otherwise the
                // bytes decide.
                Some(
                    (std::ptr::eq(text.as_ptr(), site.as_ptr()) && text.len() == site.len())
                        || &**text == site,
                )
            }
            _ => None,
        }
    });
    if let Some(answer) = cached {
        return answer;
    }
    let Some(text) = field_link_sites().read().names.get(id).cloned() else {
        return false;
    };
    let answer = &*text == site;
    FIELD_LINK_SITE_TEXTS.with(|lines| {
        let mut lines = lines.borrow_mut();
        if lines.is_empty() {
            lines.resize(FIELD_LINK_SITE_TEXT_LINES, None);
        }
        lines[line] = Some((id32, text));
    });
    answer
}

#[cfg(test)]
mod i91_l4_field_link_site_local_tests {
    use super::{field_link_site_id, field_link_site_matches_local};

    /// The per-thread copy answers exactly what the table answers: on the
    /// filling call, on a cached repeat, for another text of the same id, for
    /// an id the table never issued, and for two ids sharing one line.
    #[test]
    fn the_local_copy_answers_as_the_table() {
        let a = field_link_site_id("(Lp/I91Cell;)I").expect("interned") as usize;
        let b = field_link_site_id("(Lp/I91Cell;)J").expect("interned") as usize;
        for _ in 0..2 {
            assert!(field_link_site_matches_local(a, "(Lp/I91Cell;)I"));
            assert!(!field_link_site_matches_local(a, "(Lp/I91Cell;)J"));
            assert!(field_link_site_matches_local(b, "(Lp/I91Cell;)J"));
            assert!(!field_link_site_matches_local(b, "(Lp/I91Cell;)I"));
        }
        // A site text from another allocation with the same bytes.
        let owned = String::from("(Lp/I91Cell;)I");
        assert!(field_link_site_matches_local(a, &owned));
        assert!(!field_link_site_matches_local(
            usize::MAX >> 1,
            "(Lp/I91Cell;)I"
        ));
        // Another id on `a`'s line (issued or not) evicts or misses; the text
        // is interned once, so no other id names it, and `a` still answers.
        let c = a + super::FIELD_LINK_SITE_TEXT_LINES;
        assert!(!field_link_site_matches_local(c, "(Lp/I91Cell;)I"));
        assert!(field_link_site_matches_local(a, "(Lp/I91Cell;)I"));
    }
}

/// Is `site` exactly the descriptor interned as `id`?
pub(crate) fn field_link_site_matches(id: i32, site: &str) -> bool {
    let Ok(id) = usize::try_from(id) else {
        return false;
    };
    if crate::nbflags().mh_field_link_site_by_arc {
        return field_link_site_matches_local(id, site);
    }
    field_link_sites()
        .read()
        .names
        .get(id)
        .is_some_and(|text| &**text == site)
}

/// Serve an `invokeExact` on a LINKED field getter/setter, or `None` to send
/// the call to the door. Reads only (but for an owner link's subclass memo,
/// [`field_link_receiver_admitted`]); allocates nothing; cannot throw. See the
/// section comment for what a link is and what it leaves to the door.
pub(crate) fn mh_field_link_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    if ctx.object_num_fields(this) < MH_SLOT_COUNT {
        return None;
    }
    let state = match ctx.get_field(this, MH_FIELD_LINK) {
        Value::Int(s) if s & FL_LINKED != 0 => s,
        _ => return None,
    };
    // Still the handle it was linked as: kind, no marking, no bound receiver,
    // the same `type` object.
    let setter = state & FL_SETTER != 0;
    let want_kind = if setter {
        MH_KIND_SETTER
    } else {
        MH_KIND_GETTER
    };
    if !matches!(ctx.get_field(this, MH_KIND), Value::Int(k) if k == want_kind) {
        return None;
    }
    if !matches!(ctx.get_field(this, MH_VARARGS), Value::Int(0)) {
        return None;
    }
    // A bound handle only under a bound link (wave 66, `FL_BOUND`), whose
    // receiver is the bound object; an unbound link never serves one.
    let bound_receiver = match ctx.get_field(this, MH_BOUND) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    if (state & FL_BOUND != 0) != bound_receiver.is_some() {
        return None;
    }
    let type_idx = ((state >> 16) & FL_TYPE_INDEX_MASK) as usize;
    match (
        ctx.get_field(this, type_idx),
        ctx.get_field(this, MH_FIELD_LINK_TYPE),
    ) {
        (Value::Object(Some(now)), Value::Object(Some(linked))) if now == linked => {}
        _ => return None,
    }
    // `exact_call_site_refusal`'s check, against the interned declared type.
    // Whatever `CRATONVM_MH_STRICT_INVOKEEXACT` says (interpreter round i3
    // wave 79, lane L4; page
    // `i71-L4-non-strict-invokeexact-serves-a-field-link-at-any-site`): with
    // the switch off the door ADAPTS a call at another type (`adapt_leaf_arg`,
    // the return conversion), which a raw load / store must not skip -- a
    // `(Box,long)void` site stored its `Long` into the `int` slot unconverted.
    // The link serves exactly the declared type, as the linking-call and
    // direct serves already did.
    let Value::Int(site_id) = ctx.get_field(this, MH_FIELD_LINK_SITE) else {
        return None;
    };
    if !field_link_site_matches(site_id, call_site?) {
        return None;
    }
    let slot = (state & 0xFFFF) as usize;
    let volatile = state & FL_VOLATILE != 0;
    let Value::Int(class_raw) = ctx.get_field(this, MH_FIELD_LINK_CLASS) else {
        return None;
    };
    let value_ok = |ctx: &mut dyn NativeContext, v: Value| -> bool {
        if state & FL_VALUE_CHECK == 0 {
            return true;
        }
        match v {
            Value::Object(None) => true,
            Value::Object(Some(o)) => {
                !ctx.object_is_array(o)
                    && matches!(
                        ctx.get_field(this, MH_FIELD_LINK_VALUE),
                        Value::Int(raw) if raw == ctx.class_id_of_object(o).as_u32() as i32
                    )
            }
            _ => false,
        }
    };
    if state & FL_STATIC != 0 {
        if bound_receiver.is_some() {
            return None;
        }
        let owner = ClassId::new(class_raw as u32);
        if !setter {
            if !extra.is_empty() {
                return None;
            }
            let v = ctx.get_static_field(owner, slot);
            field_handle_volatile_load_fence(volatile);
            return Some(Ok(Some(v)));
        }
        let [v] = extra else {
            return None;
        };
        if !value_ok(ctx, *v) {
            return None;
        }
        field_handle_volatile_store(volatile, || ctx.set_static_field(owner, slot, *v));
        return Some(Ok(Some(Value::Object(None))));
    }
    // A null receiver is the door's (its message-less NPE). A bound link's
    // receiver is `MH_BOUND`'s (wave 66), and every argument is after it.
    let (receiver, rest) = match bound_receiver {
        Some(r) => (r, extra),
        None => match extra.split_first() {
            Some((Value::Object(Some(r)), rest)) => (*r, rest),
            _ => return None,
        },
    };
    // The linked receiver class, or (wave 65, `FL_OWNER_ADMITS`) the owner
    // and its subclasses. A bound link names the bound receiver's exact
    // class, so a receiver of any other class (a `String`) is the door's.
    if !field_link_receiver_admitted(ctx, this, state, class_raw, receiver) {
        return None;
    }
    if !setter {
        if !rest.is_empty() {
            return None;
        }
        let v = ctx.get_field(receiver, slot);
        field_handle_volatile_load_fence(volatile);
        return Some(Ok(Some(v)));
    }
    let [v] = rest else {
        return None;
    };
    if !value_ok(ctx, *v) {
        return None;
    }
    field_handle_volatile_store(volatile, || ctx.set_field(receiver, slot, *v));
    Some(Ok(Some(Value::Object(None))))
}

/// What [`field_link_candidate`] found on a handle the link may consider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldLinkCandidate {
    /// A `SETTER` (else a `GETTER`).
    pub(crate) setter: bool,
    /// The packed [`MH_FIELD_LINK`] state as it stands (`0` when none).
    pub(crate) old: i32,
    /// `old`'s relink count, below [`FL_MAX_LINKS`].
    pub(crate) links: i32,
}

/// The checks every field-accessor link runs before it looks at a call (the
/// switches and this VM's mode, the width, the kind, the relink bound, no
/// marking): shared by [`mh_field_link_try`] and `bindTo`'s pre-link
/// ([`mh_bindto_prelink_bound_field`], wave 67), so the two cannot drift.
/// Reads only.
pub(crate) fn field_link_candidate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<FieldLinkCandidate> {
    if !mh_field_link_enabled()
        || !mh_field_link_mode_admits(&*ctx)
        || crate::nbflags().dbg_mh_dispatch
    {
        return None;
    }
    if ctx.object_num_fields(this) < MH_SLOT_COUNT {
        return None;
    }
    let setter = match ctx.get_field(this, MH_KIND) {
        Value::Int(MH_KIND_GETTER) => false,
        Value::Int(MH_KIND_SETTER) => true,
        _ => return None,
    };
    let old = match ctx.get_field(this, MH_FIELD_LINK) {
        Value::Int(s) => s,
        _ => 0,
    };
    let links = (old >> FL_LINKS_SHIFT) & 0x7;
    if links >= FL_MAX_LINKS {
        return None;
    }
    if !matches!(ctx.get_field(this, MH_VARARGS), Value::Int(0)) {
        return None;
    }
    Some(FieldLinkCandidate { setter, old, links })
}

/// What a bound field link is made for ([`mh_field_link_try_bound`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum BoundLinkCall<'a> {
    /// An `invokeExact` the door is about to serve: its arguments after the
    /// handle and its call-site descriptor.
    Invoke {
        extra: &'a [Value],
        call_site: Option<&'a str>,
    },
    /// `bindTo`'s pre-link of the copy it just minted (wave 67,
    /// `CRATONVM_MH_BINDTO_PRELINK_FIELD`): no call yet, so no call site to
    /// judge (the fast path judges every later one against the interned
    /// declared type) and no value to judge (a reference setter whose value
    /// needs a check is left to its first call).
    BindTo,
}

/// `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE` (default on since the wave-67 triage, performance;
/// interpreter round i2 wave 67, lane L4; proposal
/// `i66-L4-proposal-field-link-serves-invoke-at-the-declared-type-20261030`):
/// the `invoke` door consults the field-accessor link too, for a call site
/// that is exactly the link's declared type ([`mh_field_link_fast_for_invoke`]).
pub(crate) fn mh_field_link_serves_invoke_enabled() -> bool {
    crate::nbflags().mh_field_link_serves_invoke
}

/// Is `site` exactly the declared type linked handle `this` interned
/// ([`MH_FIELD_LINK_SITE`])? `false` for an unlinked or narrow handle, or no
/// site. Reads only.
pub(crate) fn field_link_site_is(ctx: &dyn NativeContext, this: ObjectRef, site: Option<&str>) -> bool {
    let Some(site) = site else {
        return false;
    };
    if ctx.object_num_fields(this) < MH_SLOT_COUNT {
        return false;
    }
    if !matches!(ctx.get_field(this, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED != 0) {
        return false;
    }
    match ctx.get_field(this, MH_FIELD_LINK_SITE) {
        Value::Int(id) => field_link_site_matches(id, site),
        _ => false,
    }
}

/// `invoke` on a LINKED field getter / setter (wave 67,
/// `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`), or `None` for the door. Only when
/// the call site is EXACTLY the declared type the link interned -- checked
/// here whether or not `CRATONVM_MH_STRICT_INVOKEEXACT` is on, since any other
/// `invoke` site is an `asType` the link must not skip (an `Object`-typed
/// receiver of `(Base)int` stays on the door) -- and then by
/// [`mh_field_link_fast`]'s every check. `invoke` at the declared type IS
/// `invokeExact` (`asType` to the same type is the identity). Links nothing:
/// the link is still made only by `invokeExact` ([`mh_field_link_try`]) or
/// `bindTo`'s pre-link. The caller has run `invoke`'s pre-door refusals.
pub(crate) fn mh_field_link_fast_for_invoke(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) -> Option<MethodCallResult> {
    if !mh_field_link_serves_invoke_enabled() || !field_link_site_is(&*ctx, this, call_site) {
        return None;
    }
    mh_field_link_fast(ctx, this, extra, call_site)
}

thread_local! {
    /// `CRATONVM_MH_FIELD_LINK_FIRST`: `(vm, class, Some(()))` for the
    /// synthetic `java/lang/invoke/MethodHandle` carrier class, `None` for any
    /// other handle class this thread asked about (a few real species), so
    /// neither answer takes the class-manager lock twice. The class is the
    /// VM's own `MethodHandle` (never unloaded while the VM lives); the key
    /// carries the VM.
    // addr-keyed: NOT an object address -- the `usize` is the VM identity, beside a class id; bounded to 8 ways.
    pub(crate) static FIELD_LINK_FIRST_CARRIER_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<()>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Is `this` an instance of the synthetic carrier class itself (the VM's
/// `java/lang/invoke/MethodHandle`, which is abstract in the JDK, so no real
/// handle is a direct instance of it)? Memoised per thread by class id
/// ([`FIELD_LINK_FIRST_CARRIER_MEMO`]). Reads only.
pub(crate) fn mh_is_synthetic_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return false;
    }
    let cid = ctx.class_id_of_object(this);
    seg_lean_memo(&FIELD_LINK_FIRST_CARRIER_MEMO, vm, cid, || {
        (ctx.class_name_arc_of_id(cid).as_deref() == Some("java/lang/invoke/MethodHandle"))
            .then_some(())
    })
    .is_some()
}

/// `CRATONVM_MH_FIELD_LINK_FIRST` (interpreter round i4 wave 94, lane L4;
/// default off, performance; proposal
/// `i71-L4-proposal-a-linked-field-accessor-is-served-before-the-real-handle-lanes`):
/// serve a LINKED synthetic field getter / setter called at exactly its
/// declared site, right after the native took the call site, or `None` (the
/// unchanged sequence runs).
///
/// The screens this skips all decline for such a handle (wave 94 reading):
/// * `mh_real_constructor_fast`, `mh_real_ctor_widening_serve`: a
///   `DirectMethodHandle$Constructor` class only;
/// * `mh_linked_invoker`, `mh_real_direct_fast`, `mh_real_field_record_fast`,
///   `mh_real_exact_refusal`, `mh_real_direct_dispatch`,
///   `mh_real_lambda_form_dispatch`, `mh_real_field_converting_serve`,
///   `mh_door_handle`: each returns at its carrier-width test
///   (`object_num_fields > MH_BOUND` with an `Int` `MH_KIND`);
/// * `panama::interp_downcall_lane`: a downcall carrier's width only;
/// * `mh_interface_null_receiver_npe`: a `VIRTUAL` kind only;
/// * `mh_direct_lane`, `mh_converting_lane`: `STATIC` / `VIRTUAL` kinds only.
///
/// The refusals that CAN fire for it keep their order: the site's access
/// refusal ([`mh_site_access_refusal`]) for both doors, then for `invoke` the
/// loader cast ([`mh_invoke_loader_cast_refusal`]); an `invoke` whose return
/// is cast across loaders ([`mh_invoke_return_cast_target`]) declines, as the
/// ordinary sequence skips the link for it. A site other than the declared
/// one declines before any refusal is asked, so the ordinary sequence asks
/// them once.
pub(crate) fn mh_field_link_first(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    door: LinkingDoor,
) -> Option<MethodCallResult> {
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // Width, link word and declared site first: pure field reads, which a
    // real handle (too narrow) or an unlinked carrier fails at once.
    if !field_link_site_is(&*ctx, this, call_site) {
        return None;
    }
    if !mh_is_synthetic_carrier(&*ctx, this) {
        return None;
    }
    if let Some(refusal) = mh_site_access_refusal(&*ctx, call_site) {
        return Some(Err(refusal));
    }
    match door {
        LinkingDoor::InvokeExact => mh_field_link_fast(ctx, this, extra, call_site),
        LinkingDoor::Invoke => {
            if !mh_field_link_serves_invoke_enabled() {
                return None;
            }
            if let Some(refusal) = mh_invoke_loader_cast_refusal(ctx, this, extra, call_site) {
                return Some(Err(refusal));
            }
            if mh_invoke_return_cast_target(ctx, this, call_site).is_some() {
                return None;
            }
            mh_field_link_fast(ctx, this, extra, call_site)
        }
    }
}

/// `CRATONVM_MH_INVOKE_LINKS_FIELD` (default ON since the wave-68 triage, `=0` off; performance;
/// interpreter round i2 wave 68, lane L4; proposal
/// `i67-L4-proposal-invoke-links-a-field-accessor-at-the-declared-type-20261031`):
/// an `invoke` at exactly a field accessor's declared type LINKS it
/// ([`mh_field_link_try_for_invoke`]), so a handle only ever called through
/// `invoke` is served by [`mh_field_link_fast_for_invoke`] too. Needs
/// `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE` (without it no `invoke` reads the
/// link this writes).
pub(crate) fn mh_invoke_links_field_enabled() -> bool {
    crate::nbflags().mh_invoke_links_field
}

/// Is the `invoke` call site `site` one at which `invoke` IS `invokeExact`
/// for a handle declared `declared`: a non-empty descriptor equal to the
/// declared type? An unknown declared type, or no site, is not. Pure.
pub(crate) fn invoke_site_is_declared(site: Option<&str>, declared: Option<&str>) -> bool {
    match (site, declared) {
        (Some(site), Some(declared)) => !site.is_empty() && site == declared,
        _ => false,
    }
}

/// Is `site` `this`'s declared type? Through the return-plan memo
/// ([`mh_return_facts_declared`], no `MethodType` render on a repeat call of
/// the same handle) when `CRATONVM_MH_RETURN_PLAN` is on, else rendered --
/// the answer [`mh_declared_descriptor`] gives either way, compared without
/// a `String` copy on the memo path.
pub(crate) fn mh_declared_type_is_site(ctx: &mut dyn NativeContext, this: ObjectRef, site: &str) -> bool {
    if crate::nbflags().mh_return_plan {
        let declared = mh_return_facts_declared(ctx, this);
        return invoke_site_is_declared(Some(site), declared.as_deref());
    }
    let declared = mh_declared_descriptor_rendered(ctx, this);
    invoke_site_is_declared(Some(site), declared.as_deref())
}

/// `invoke`'s link of a field getter / setter (wave 68,
/// `CRATONVM_MH_INVOKE_LINKS_FIELD`): after [`mh_field_link_fast_for_invoke`]
/// declined, and only when the call site is a non-empty descriptor EQUAL to
/// the handle's declared type, run [`mh_field_link_try`] exactly as
/// `invokeExact` runs it. At that site `invoke` is `invokeExact` (`asType`
/// to the same type is the identity), so what the link admits -- and the
/// receiver class it names and every later call re-checks -- is what an
/// `invokeExact` link admits. Any other site (an `Object`-typed receiver, a
/// widened return; the `--compatible` `String`-receiver row of
/// `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver`) is
/// never linked from here. The caller has run `invoke`'s pre-door refusals
/// and found no cross-loader return cast due. Links only; the door serves
/// this call. Performs no allocation and no Java call (the render of an
/// uncached declared type is the one `invokeExact`'s link makes), so `this`
/// and `extra` stay valid for the door.
pub(crate) fn mh_field_link_try_for_invoke(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) {
    mh_field_link_try_for_invoke_noting(ctx, this, extra, call_site, &mut None);
}

/// [`mh_field_link_try_for_invoke`], noting a written link in `note` (wave 71,
/// [`FieldLinkWritten`]).
pub(crate) fn mh_field_link_try_for_invoke_noting(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    note: &mut Option<FieldLinkWritten>,
) {
    let once = mh_invoke_link_declared_once_enabled();
    mh_field_link_try_for_invoke_with(ctx, this, extra, call_site, once, note);
}

/// `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE` (default OFF, opt-in, performance;
/// a pure refactor -- every answer is the same; interpreter round i2 wave 70,
/// lane L4; proposal
/// `i69-L4-proposal-invoke-link-renders-the-declared-type-once-20261102`):
/// [`mh_field_link_try_for_invoke`] hands the candidate screen it ran and the
/// declared type it compared (the call site itself, which it just found EQUAL
/// to the declared type) to [`mh_field_link_derive`], so the link neither
/// screens again nor looks the declared type up again (a return-plan memo
/// probe and a `String` copy, or a second `MethodType` render with
/// `CRATONVM_MH_RETURN_PLAN` off). Builds on the default-on
/// `CRATONVM_MH_INVOKE_LINKS_FIELD`.
pub(crate) fn mh_invoke_link_declared_once_enabled() -> bool {
    crate::nbflags().mh_invoke_link_declared_once
}

/// [`mh_field_link_try_for_invoke`] with `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`
/// as `once` (the unit tests pass it). Off, the link after the declared-type
/// comparison is wave 68's: [`field_link_candidate`] and the declared type
/// looked up again inside the attempt.
pub(crate) fn mh_field_link_try_for_invoke_with(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    once: bool,
    note: &mut Option<FieldLinkWritten>,
) {
    if !mh_invoke_links_field_enabled() || !mh_field_link_serves_invoke_enabled() {
        return;
    }
    let Some(site) = call_site.filter(|s| !s.is_empty()) else {
        return;
    };
    // The cheap screens first (switches, mode, width, kind, relink bound,
    // marking), so a handle that can never link pays no declared-type lookup.
    let Some(candidate) = field_link_candidate(ctx, this) else {
        return;
    };
    // Wave 70 (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`): a remembered
    // derivation of this shape whose interned declared type IS this site
    // writes the link before any declared-type lookup.
    let memo =
        match field_link_memo_replay_noting(ctx, this, candidate, extra, Some(site), true, note) {
            FieldLinkMemoReplay::Written => return,
            FieldLinkMemoReplay::Miss(memo) => memo,
        };
    if !mh_declared_type_is_site(ctx, this, site) {
        return;
    }
    if once {
        // `site` IS the declared type (just compared equal).
        mh_field_link_derive(ctx, this, candidate, Some(site), extra, Some(site), note);
    } else if let Some(candidate) = field_link_candidate(ctx, this) {
        mh_field_link_derive(ctx, this, candidate, None, extra, Some(site), note);
    }
    field_link_memo_fill_after(ctx, this, candidate, memo, extra);
}

/// `CRATONVM_MH_LINKING_CALL_SERVED` (default OFF, opt-in, performance;
/// interpreter round i2 wave 69, lane L4; proposal
/// `i68-L4-proposal-a-linking-call-is-served-by-its-link-20261101`): the call
/// that WRITES a field-accessor link (`invokeExact`'s [`mh_field_link_try`],
/// and -- building on the default-on `CRATONVM_MH_INVOKE_LINKS_FIELD` --
/// `invoke`'s [`mh_field_link_try_for_invoke`]) is served by that link
/// ([`mh_field_link_serve_linking_call_with`]) instead of the door, so a
/// handle called once or twice no longer pays the door AND the link.
pub(crate) fn mh_linking_call_served_enabled() -> bool {
    crate::nbflags().mh_linking_call_served
}

/// The packed [`MH_FIELD_LINK`] word of `this` (`0` for a narrow handle or
/// none). Reads only.
pub(crate) fn field_link_state_word(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if ctx.object_num_fields(this) < MH_SLOT_COUNT {
        return 0;
    }
    match ctx.get_field(this, MH_FIELD_LINK) {
        Value::Int(s) => s,
        _ => 0,
    }
}

/// Did a link attempt that found the word `before` write a link, leaving
/// `after`? Every write raises the relink count ([`FL_LINKS_SHIFT`]) and
/// sets [`FL_LINKED`], so a written link never equals the word it replaced;
/// an attempt that declined leaves the word as it was. Pure.
pub(crate) fn field_link_was_written(before: i32, after: i32) -> bool {
    after & FL_LINKED != 0 && after != before
}

/// Which door a linking call came through (wave 69,
/// [`mh_field_link_try_and_serve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkingDoor {
    /// `invokeExact`: linked by [`mh_field_link_try`], served by
    /// [`mh_field_link_fast`].
    InvokeExact,
    /// `invoke` with no return cast due: linked by
    /// [`mh_field_link_try_for_invoke`], served by
    /// [`mh_field_link_fast_for_invoke`].
    Invoke,
}

/// Wave 69 (`CRATONVM_MH_LINKING_CALL_SERVED`, passed as `on`): serve the call
/// that just linked `this` through the link, or `None` for the door. Only
/// when `on`, the attempt wrote a link ([`field_link_was_written`] from
/// `before`, the word the attempt found), and the call site is EXACTLY the
/// declared type the link interned ([`field_link_site_is`] -- the only site at
/// which the door's answer and the link's are the same answer whatever
/// `CRATONVM_MH_STRICT_INVOKEEXACT` says). The serve then re-checks every word
/// ([`mh_field_link_fast`]), so a decline is the door's, as on any later
/// call. A static link is written only for an initialized class, so the
/// door's `ensure_class_initialized` it skips is a no-op. Reads and the one
/// field load / store; no allocation, no Java call, cannot throw.
pub(crate) fn mh_field_link_serve_linking_call_with(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    before: i32,
    door: LinkingDoor,
    on: bool,
) -> Option<MethodCallResult> {
    if !on {
        return None;
    }
    if !field_link_was_written(before, field_link_state_word(&*ctx, this)) {
        return None;
    }
    if !field_link_site_is(&*ctx, this, call_site) {
        return None;
    }
    match door {
        LinkingDoor::InvokeExact => mh_field_link_fast(ctx, this, extra, call_site),
        LinkingDoor::Invoke => mh_field_link_fast_for_invoke(ctx, this, extra, call_site),
    }
}

/// The link attempt of `door` ([`mh_field_link_try`] or
/// [`mh_field_link_try_for_invoke`], unchanged), then (wave 69,
/// `CRATONVM_MH_LINKING_CALL_SERVED`) the serve of this very call through the
/// link it wrote ([`mh_field_link_serve_linking_call_with`]). `None`: the
/// door serves this call, exactly as before. With the switch off this is the
/// bare attempt (the link word is not even read).
pub(crate) fn mh_field_link_try_and_serve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    door: LinkingDoor,
) -> Option<MethodCallResult> {
    let on = mh_linking_call_served_enabled();
    if on && mh_linking_call_direct_serve_enabled() {
        // Wave 71 (`CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`): served from what
        // the attempt admitted, without re-reading what it wrote.
        let mut note = None;
        match door {
            LinkingDoor::InvokeExact => mh_field_link_try(ctx, this, extra, call_site, &mut note),
            LinkingDoor::Invoke => {
                mh_field_link_try_for_invoke_noting(ctx, this, extra, call_site, &mut note)
            }
        }
        return mh_field_link_serve_written(ctx, this, extra, call_site, note);
    }
    let before = if on {
        field_link_state_word(&*ctx, this)
    } else {
        0
    };
    match door {
        LinkingDoor::InvokeExact => mh_field_link_try(ctx, this, extra, call_site, &mut None),
        LinkingDoor::Invoke => mh_field_link_try_for_invoke(ctx, this, extra, call_site),
    }
    mh_field_link_serve_linking_call_with(ctx, this, extra, call_site, before, door, on)
}

/// `CRATONVM_MH_LINKING_CALL_DIRECT_SERVE` (default OFF, opt-in, performance;
/// interpreter round i3 wave 71, lane L4; proposals
/// `i70-L4-proposal-a-memo-hit-serves-the-linking-call-20261103` and
/// `i68-L4-proposal-a-linking-call-is-served-by-its-link-20261101`). Read only
/// under `CRATONVM_MH_LINKING_CALL_SERVED`, which it builds on: the linking
/// call is served by [`mh_field_link_serve_written`] from the attempt's
/// [`FieldLinkWritten`] note instead of by [`mh_field_link_serve_linking_call_with`],
/// which re-reads the word it wrote, asks the interned site under its lock
/// (twice through `invoke`) and then has [`mh_field_link_fast`] re-read every
/// link word, the site once more under the strict check, and the receiver's
/// class.
pub(crate) fn mh_linking_call_direct_serve_enabled() -> bool {
    crate::nbflags().mh_linking_call_direct_serve
}

/// What a link attempt that WROTE a field-accessor link admitted about the
/// call it was made for (wave 71, `CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`).
/// Written next to the link's last store by every writer that judges a call:
/// [`mh_field_link_derive`] (unbound), [`mh_field_link_try_bound`] (an
/// `invokeExact`'s bound link) and [`field_link_memo_write`] (a memo hit).
/// The receiver is the call's (`extra[0]`) or `MH_BOUND`'s, current: nothing
/// between the write and the serve allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldLinkWritten {
    /// The packed [`MH_FIELD_LINK`] word written (slot, kind bits).
    pub(crate) state: i32,
    /// [`MH_FIELD_LINK_CLASS`]: the static owner, the receiver class, or
    /// (with [`FL_OWNER_ADMITS`]) the field's owner.
    pub(crate) class_raw: i32,
    /// The receiver the link admitted; `None` for a static link.
    pub(crate) receiver: Option<ObjectRef>,
    /// The receiver's exact class (`0` for a static link).
    pub(crate) recv_raw: i32,
    /// The call site was compared EQUAL to the declared type the link interned.
    pub(crate) site_is_declared: bool,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type, asked when the
    /// site was not judged by the writer.
    pub(crate) site_id: i32,
}

/// Wave 71: serve the linking call from `note` (the attempt's
/// [`FieldLinkWritten`]), or `None` for the door. Exactly the answer
/// [`mh_field_link_fast`] gives on the words just written -- each of its
/// checks holds by construction of the write (kind, marking, bound shape and
/// `type` object from the candidate screen; receiver, class, value, arity and
/// a static owner's initialization from the attempt) -- except the call site,
/// which must be the declared type ([`mh_field_link_serve_linking_call_with`]'s
/// rule, whatever `CRATONVM_MH_STRICT_INVOKEEXACT` says), and the arity, which
/// is checked again. An owner link's subclass receiver is remembered in
/// [`MH_FIELD_LINK_VALUE`] as [`field_link_receiver_admitted`] remembers it.
/// Reads and the one field load / store; no allocation, no Java call.
pub(crate) fn mh_field_link_serve_written(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    note: Option<FieldLinkWritten>,
) -> Option<MethodCallResult> {
    let w = note?;
    if !w.site_is_declared && !call_site.is_some_and(|s| field_link_site_matches(w.site_id, s)) {
        return None;
    }
    let state = w.state;
    if state & FL_LINKED == 0 {
        return None;
    }
    let slot = (state & 0xFFFF) as usize;
    let setter = state & FL_SETTER != 0;
    let is_static = state & FL_STATIC != 0;
    let volatile = state & FL_VOLATILE != 0;
    // An unbound instance link's receiver is the first argument.
    let rest: &[Value] = if is_static || state & FL_BOUND != 0 {
        extra
    } else {
        extra.get(1..)?
    };
    let value = match (setter, rest) {
        (false, []) => None,
        (true, [v]) => Some(*v),
        _ => return None,
    };
    if is_static {
        let owner = ClassId::new(w.class_raw as u32);
        return Some(Ok(Some(match value {
            None => {
                let v = ctx.get_static_field(owner, slot);
                field_handle_volatile_load_fence(volatile);
                v
            }
            Some(v) => {
                field_handle_volatile_store(volatile, || ctx.set_static_field(owner, slot, v));
                Value::Object(None)
            }
        })));
    }
    let receiver = w.receiver?;
    if state & FL_OWNER_ADMITS != 0 && state & FL_VALUE_CHECK == 0 && w.recv_raw != w.class_raw {
        ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(w.recv_raw));
    }
    Some(Ok(Some(match value {
        None => {
            let v = ctx.get_field(receiver, slot);
            field_handle_volatile_load_fence(volatile);
            v
        }
        Some(v) => {
            field_handle_volatile_store(volatile, || ctx.set_field(receiver, slot, v));
            Value::Object(None)
        }
    })))
}

// ---------------------------------------------------------------------------
// A field-accessor link's derivation, memoised per source shape (interpreter
// round i2 wave 70, lane L4; `CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`, default
// OFF; proposal
// `i69-L4-proposal-field-link-derivation-is-memoised-per-source-shape-20261102`)
// ---------------------------------------------------------------------------
//
// Every fresh handle paid the whole link derivation on its linking call: the
// declared descriptor (a `String` copy, or a `MethodType` render), two
// `split_descriptor_params`, three slot-string decodes, the `type` field's
// index by name, the slot lookup and the interned-site `RwLock`. For the
// bound copies `bindTo` mints from one source (they share `MH_CLASS` /
// `MH_NAME` / `MH_DESC`, the bound-type table's `type` object, the carried
// owner and minted slot) every one of those inputs is the same words; only
// the receiver's class (and a checked setter value's class) varies.
//
// A per-thread, direct-mapped memo keyed -- within one collection epoch, where
// an equal address is the same object (`BINDTO_MINT_MEMO`'s rule) -- by the
// VM, the words of the member slots, `MH_KIND`, `MH_VARARGS`,
// `MH_CAST_CHAIN`, `MH_STATIC_OWNER`, whether `MH_BOUND` holds a receiver,
// and the link word the candidate screen found (its minted slot and relink
// count), remembers the words a derivation WROTE plus the receiver class it
// admitted, the arity and the `type` object it linked. A linking call whose
// key matches, whose `type` is that object, whose receiver (and checked
// value) is of exactly the remembered class, and -- where the derivation
// judges it -- whose site is the interned declared type, writes the same
// words. Anything else derives as before (and refills the line).
//
// Only answers that stay true are remembered: an instance link (never a
// static one, whose owner may be named by a loader-dependent name) whose slot
// is the recorded owner's (minted, or `FL_OWNER_ADMITS`), or a bound link
// (always by the owner's id). The receiver and value admissions are positive
// readings for a class, which a later class load does not undo. Under
// `CRATONVM_DBG_MH_FIELD_LINK` the memo is off, so the positive control still
// prints its line.

/// `CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO` (default OFF, opt-in, performance;
/// see the section comment).
pub(crate) fn mh_field_link_derivation_memo_enabled() -> bool {
    crate::nbflags().mh_field_link_derivation_memo
}

/// Lines in [`FIELD_LINK_MEMO`]; a power of two (the index is a mask).
pub(crate) const FIELD_LINK_MEMO_SLOTS: usize = 16;

/// The handle slots a memo key reads as words (then `MH_STATIC_OWNER` and
/// `MH_BOUND`'s shape).
pub(crate) const FIELD_LINK_MEMO_GUARD_SLOTS: [usize; 6] = [
    MH_CLASS,
    MH_NAME,
    MH_DESC,
    MH_KIND,
    MH_VARARGS,
    MH_CAST_CHAIN,
];

/// What a [`FIELD_LINK_MEMO`] line is keyed on, read off the handle after
/// [`field_link_candidate`] admitted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldLinkMemoKey {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    /// [`FIELD_LINK_MEMO_GUARD_SLOTS`], `MH_STATIC_OWNER`, then
    /// [`field_link_memo_bound_word`] of `MH_BOUND`.
    pub(crate) guard: [u64; 8],
    /// The link word the candidate screen found (`FieldLinkCandidate::old`).
    pub(crate) old: i32,
}

/// The words a derivation wrote for a [`FieldLinkMemoKey`], and what of the
/// call it depended on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldLinkMemoAnswer {
    /// The packed [`MH_FIELD_LINK`] word written.
    pub(crate) state: i32,
    /// The `type` object linked ([`MH_FIELD_LINK_TYPE`]), as a word.
    pub(crate) type_word: u64,
    /// [`MH_FIELD_LINK_CLASS`].
    pub(crate) class_raw: i32,
    /// [`MH_FIELD_LINK_VALUE`]: the checked value's class, else `0`.
    pub(crate) value_raw: i32,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type.
    pub(crate) site_id: i32,
    /// The call's argument count after the handle.
    pub(crate) arity: usize,
    /// The exact class of the receiver the derivation admitted.
    pub(crate) recv_class: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FieldLinkMemoLine {
    pub(crate) key: FieldLinkMemoKey,
    pub(crate) answer: FieldLinkMemoAnswer,
}

thread_local! {
    /// Per-thread link-derivation lines, direct-mapped by `MH_DESC`'s word. A
    /// context reporting VM identity `0` (the mocks) is never memoised outside
    /// the unit tests' own override.
    pub(crate) static FIELD_LINK_MEMO: std::cell::RefCell<[Option<FieldLinkMemoLine>; FIELD_LINK_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([None; FIELD_LINK_MEMO_SLOTS]) };
}

#[cfg(test)]
thread_local! {
    /// Unit tests only: the VM identity the memo uses with the switch taken as
    /// on, so the mock context (identity `0`) can exercise it.
    pub(crate) static FIELD_LINK_MEMO_TEST_VM: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The VM identity the memo keys on, or `None` when it is off (the switch,
/// `CRATONVM_DBG_MH_FIELD_LINK`, or a context with no identity).
pub(crate) fn field_link_memo_vm(ctx: &dyn NativeContext) -> Option<usize> {
    #[cfg(test)]
    if let Some(vm) = FIELD_LINK_MEMO_TEST_VM.with(|v| v.get()) {
        return Some(vm);
    }
    if !mh_field_link_derivation_memo_enabled() || crate::nbflags().dbg_mh_field_link {
        return None;
    }
    match ctx.vm_identity() {
        0 => None,
        vm => Some(vm),
    }
}

/// `MH_BOUND` as a key word: a bound receiver is ONE word whatever the
/// receiver (its class is judged per call); a recorded static owner or none
/// as [`bindto_mint_word`]. Pure.
pub(crate) fn field_link_memo_bound_word(v: Value) -> u64 {
    match v {
        Value::Object(Some(_)) => u64::MAX - 1,
        other => bindto_mint_word(other),
    }
}

/// The memo key of `this` (width-checked by [`field_link_candidate`], which
/// found `candidate`) under `vm`. Reads only.
pub(crate) fn field_link_memo_key(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    vm: usize,
) -> FieldLinkMemoKey {
    let mut guard = [0u64; 8];
    for (i, slot) in FIELD_LINK_MEMO_GUARD_SLOTS.iter().enumerate() {
        guard[i] = bindto_mint_word(ctx.get_field(this, *slot));
    }
    guard[6] = bindto_mint_word(ctx.get_field(this, MH_STATIC_OWNER));
    guard[7] = field_link_memo_bound_word(ctx.get_field(this, MH_BOUND));
    FieldLinkMemoKey {
        vm,
        epoch: ctx.gc_address_epoch(),
        guard,
        old: candidate.old,
    }
}

pub(crate) fn field_link_memo_line_index(key: &FieldLinkMemoKey) -> usize {
    ((key.guard[2] >> 3) as usize) & (FIELD_LINK_MEMO_SLOTS - 1)
}

/// Does a derivation that left the link word `state` leave words the memo
/// may remember? A written INSTANCE link whose slot is the recorded owner's
/// (bound, minted, or owner-admitting). Pure.
pub(crate) fn field_link_memo_keeps(state: i32) -> bool {
    state & FL_LINKED != 0
        && state & FL_STATIC == 0
        && state & (FL_BOUND | FL_OWNER_ADMITS | FL_MINT_SLOT) != 0
}

/// The receiver a link of `state` judges: the bound one, else the call's
/// first argument; `None` for a null or missing one. Reads only.
pub(crate) fn field_link_memo_receiver(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    state: i32,
    extra: &[Value],
) -> Option<ObjectRef> {
    let v = if state & FL_BOUND != 0 {
        ctx.get_field(this, MH_BOUND)
    } else {
        *extra.first()?
    };
    match v {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    }
}

/// What [`field_link_memo_replay`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldLinkMemoReplay {
    /// The remembered link is written; the derivation is skipped.
    Written,
    /// Derive as before; then [`field_link_memo_fill_after`] with this key
    /// (`None`: the memo is off).
    Miss(Option<FieldLinkMemoKey>),
}

/// Wave 70 (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`): write the remembered
/// link on `this` (which passed [`field_link_candidate`] as `candidate`) when
/// its key's line holds one and [`field_link_memo_write`] admits this call.
/// `judge_site`: the call site must be the interned declared type (`invoke`'s
/// link always, `invokeExact`'s under `CRATONVM_MH_STRICT_INVOKEEXACT`, as
/// the derivation judges it). Performs no allocation and no Java call.
/// The production callers take [`field_link_memo_replay_noting`] (wave 71).
#[cfg(test)]
pub(crate) fn field_link_memo_replay(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    extra: &[Value],
    call_site: Option<&str>,
    judge_site: bool,
) -> FieldLinkMemoReplay {
    field_link_memo_replay_noting(
        ctx, this, candidate, extra, call_site, judge_site, &mut None,
    )
}

/// [`field_link_memo_replay`], noting in `note` (wave 71,
/// `CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`) what a written link admitted.
pub(crate) fn field_link_memo_replay_noting(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    extra: &[Value],
    call_site: Option<&str>,
    judge_site: bool,
    note: &mut Option<FieldLinkWritten>,
) -> FieldLinkMemoReplay {
    let Some(vm) = field_link_memo_vm(&*ctx) else {
        return FieldLinkMemoReplay::Miss(None);
    };
    let key = field_link_memo_key(&*ctx, this, candidate, vm);
    let answer =
        FIELD_LINK_MEMO.with(
            |memo| match &memo.borrow()[field_link_memo_line_index(&key)] {
                Some(line) if line.key == key => Some(line.answer),
                _ => None,
            },
        );
    match answer {
        Some(answer)
            if field_link_memo_write(ctx, this, &answer, extra, call_site, judge_site, note) =>
        {
            FieldLinkMemoReplay::Written
        }
        _ => FieldLinkMemoReplay::Miss(Some(key)),
    }
}

/// Write `answer`'s link on `this` when this call is one its derivation
/// admitted: the same arity, the same `type` object, a non-array receiver of
/// exactly the remembered class, a checked value of exactly the remembered
/// class, and (when `judge_site`) the call site the interned declared type.
/// `true` when written. Reads, and the five link stores (the state last).
/// A written link is noted in `note` (wave 71) with the receiver it admitted.
pub(crate) fn field_link_memo_write(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    answer: &FieldLinkMemoAnswer,
    extra: &[Value],
    call_site: Option<&str>,
    judge_site: bool,
    note: &mut Option<FieldLinkWritten>,
) -> bool {
    if extra.len() != answer.arity {
        return false;
    }
    let type_idx = ((answer.state >> 16) & FL_TYPE_INDEX_MASK) as usize;
    let type_obj = match ctx.get_field(this, type_idx) {
        Value::Object(Some(t)) if bindto_mint_word(Value::Object(Some(t))) == answer.type_word => t,
        _ => return false,
    };
    let Some(receiver) = field_link_memo_receiver(&*ctx, this, answer.state, extra) else {
        return false;
    };
    if ctx.object_is_array(receiver)
        || ctx.class_id_of_object(receiver).as_u32() != answer.recv_class
    {
        return false;
    }
    if answer.state & FL_VALUE_CHECK != 0 {
        match extra.last() {
            Some(Value::Object(Some(o)))
                if !ctx.object_is_array(*o)
                    && ctx.class_id_of_object(*o).as_u32() as i32 == answer.value_raw => {}
            _ => return false,
        }
    }
    if judge_site && !call_site.is_some_and(|s| field_link_site_matches(answer.site_id, s)) {
        return false;
    }
    ctx.set_field(this, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(this, MH_FIELD_LINK_CLASS, Value::Int(answer.class_raw));
    ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(answer.value_raw));
    ctx.set_field(this, MH_FIELD_LINK_SITE, Value::Int(answer.site_id));
    // Last: a reader never sees the state before the slots it describes.
    ctx.set_field(this, MH_FIELD_LINK, Value::Int(answer.state));
    // The site is the declared type when it was judged here; else the serve
    // asks the interned id (the declared type of this `type` object).
    *note = Some(FieldLinkWritten {
        state: answer.state,
        class_raw: answer.class_raw,
        receiver: Some(receiver),
        recv_raw: answer.recv_class as i32,
        site_is_declared: judge_site,
        site_id: answer.site_id,
    });
    true
}

/// After a derivation of `this` (found as `candidate`) that missed `key`'s
/// line: remember the link it wrote, when it wrote one the memo keeps
/// ([`field_link_memo_keeps`]) and no collection ran since the key was read.
/// Reads only, and the memo line.
pub(crate) fn field_link_memo_fill_after(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    key: Option<FieldLinkMemoKey>,
    extra: &[Value],
) {
    let Some(key) = key else {
        return;
    };
    if ctx.gc_address_epoch() != key.epoch {
        return;
    }
    let state = field_link_state_word(&*ctx, this);
    if !field_link_was_written(candidate.old, state) || !field_link_memo_keeps(state) {
        return;
    }
    let Some(answer) = field_link_memo_answer_of(&*ctx, this, state, extra) else {
        return;
    };
    FIELD_LINK_MEMO.with(|memo| {
        memo.borrow_mut()[field_link_memo_line_index(&key)] =
            Some(FieldLinkMemoLine { key, answer });
    });
}

/// The [`FieldLinkMemoAnswer`] of the link `state` just written on `this` for
/// a call with arguments `extra`, or `None`. Reads only.
pub(crate) fn field_link_memo_answer_of(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    state: i32,
    extra: &[Value],
) -> Option<FieldLinkMemoAnswer> {
    let type_idx = ((state >> 16) & FL_TYPE_INDEX_MASK) as usize;
    let Value::Object(Some(type_obj)) = ctx.get_field(this, type_idx) else {
        return None;
    };
    let (Value::Int(class_raw), Value::Int(value_raw), Value::Int(site_id)) = (
        ctx.get_field(this, MH_FIELD_LINK_CLASS),
        ctx.get_field(this, MH_FIELD_LINK_VALUE),
        ctx.get_field(this, MH_FIELD_LINK_SITE),
    ) else {
        return None;
    };
    let receiver = field_link_memo_receiver(ctx, this, state, extra)?;
    if ctx.object_is_array(receiver) {
        return None;
    }
    Some(FieldLinkMemoAnswer {
        state,
        type_word: bindto_mint_word(Value::Object(Some(type_obj))),
        class_raw,
        value_raw,
        site_id,
        arity: extra.len(),
        recv_class: ctx.class_id_of_object(receiver).as_u32(),
    })
}

/// The arguments a bound link judges for `call`, or `None` (not linked): an
/// `invokeExact`'s own when it passes the declared arity (`decl_arity`, the
/// bound type's parameter count); for `bindTo`'s pre-link none, and only when
/// the shape needs no value judged (`value_type`, from
/// [`bound_field_link_value_type`], is `None`). Pure.
pub(crate) fn bound_link_call_extra<'a>(
    call: BoundLinkCall<'a>,
    value_type: Option<&str>,
    decl_arity: usize,
) -> Option<&'a [Value]> {
    match call {
        BoundLinkCall::Invoke { extra, .. } => (extra.len() == decl_arity).then_some(extra),
        BoundLinkCall::BindTo => {
            let none: &'a [Value] = &[];
            value_type.is_none().then_some(none)
        }
    }
}

/// Link `this` for [`mh_field_link_fast`] if it is a plain field getter or
/// setter and this call is one the door serves by a direct field load or
/// store; otherwise leave it as it is. Called before the door, which then
/// serves this call itself. Performs no allocation and no Java call, so
/// `this` and `extra` stay valid for the door. A written link is noted in
/// `note` (wave 71, [`FieldLinkWritten`]).
pub(crate) fn mh_field_link_try(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    note: &mut Option<FieldLinkWritten>,
) {
    let Some(candidate) = field_link_candidate(ctx, this) else {
        return;
    };
    // Wave 70 (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`): a remembered
    // derivation of this shape and receiver class writes the link; the site
    // is judged only where the derivation judges it (the strict check).
    let strict = mh_strict_exact_signature();
    let memo =
        match field_link_memo_replay_noting(ctx, this, candidate, extra, call_site, strict, note) {
            FieldLinkMemoReplay::Written => return,
            FieldLinkMemoReplay::Miss(memo) => memo,
        };
    mh_field_link_derive(ctx, this, candidate, None, extra, call_site, note);
    field_link_memo_fill_after(ctx, this, candidate, memo, extra);
}

/// [`mh_field_link_try`]'s derivation for a handle that passed
/// [`field_link_candidate`] (`candidate`, as it found it). `declared` is the
/// handle's declared descriptor when the caller already has it (wave 70,
/// `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`: `invoke`'s link, whose call site
/// it just found equal to it), else `None` and it is looked up here
/// ([`mh_declared_descriptor`]), as before. Performs no allocation and no Java
/// call, so `this` and `extra` stay valid for the door.
pub(crate) fn mh_field_link_derive(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    declared: Option<&str>,
    extra: &[Value],
    call_site: Option<&str>,
    note: &mut Option<FieldLinkWritten>,
) {
    let FieldLinkCandidate { setter, old, links } = candidate;
    let bound = ctx.get_field(this, MH_BOUND);
    if let Value::Object(Some(receiver)) = bound {
        // Wave 66 (`CRATONVM_MH_FIELD_LINK_BOUND`): a bound instance accessor
        // links on its own rules; otherwise a bound handle is the door's.
        if mh_field_link_bound_enabled() {
            let call = BoundLinkCall::Invoke { extra, call_site };
            mh_field_link_try_bound(
                ctx, this, receiver, setter, old, links, call, declared, note,
            );
        }
        return;
    }
    let looked_up: String;
    let declared: &str = match declared {
        Some(d) => d,
        None => {
            let Some(d) = mh_declared_descriptor(ctx, this) else {
                return;
            };
            looked_up = d;
            &looked_up
        }
    };
    // A call site the door refuses (`WrongMethodTypeException`) is not linked.
    if mh_strict_exact_signature() && call_site != Some(declared) {
        return;
    }
    let (Some(class), Some(name), Some(desc)) = (
        mh_read_class(&*ctx, this),
        mh_read_name(&*ctx, this),
        mh_read_desc(&*ctx, this),
    ) else {
        return;
    };
    let Some(type_idx) = ctx.resolve_field_index("java/lang/invoke/MethodHandle", "type") else {
        return;
    };
    // 6 bits since wave 66 (bit 23 is `FL_OWNER_ADMITS`, bit 22 `FL_BOUND`).
    if type_idx > FL_TYPE_INDEX_MASK as usize {
        return;
    }
    let type_obj = match ctx.get_field(this, type_idx) {
        Value::Object(Some(t)) => t,
        _ => return,
    };
    let (Some((leaf_params, leaf_ret)), Some((decl_params, decl_ret))) = (
        split_descriptor_params(&desc),
        split_descriptor_params(&declared),
    ) else {
        return;
    };
    if leaf_params.len() != decl_params.len() || leaf_params.len() != extra.len() {
        return;
    }
    let is_ref = |t: &str| t.starts_with('L') || t.starts_with('[');
    // The door's leaf arms decide static by the leaf descriptor's arity.
    let is_static = if setter {
        leaf_params.len() == 1
    } else {
        leaf_params.is_empty()
    };
    // The value parameter of a setter, and the getter's return: the door
    // converts nothing on these for a link to skip. A primitive must be the
    // same primitive on both types; a reference return may be declared
    // `Object` (no return cast); a reference value is admitted below.
    let mut value_check: Option<i32> = None;
    if setter {
        if leaf_ret != "V" || decl_ret != "V" {
            return;
        }
        let (Some(leaf_v), Some(decl_v), Some(v)) =
            (leaf_params.last(), decl_params.last(), extra.last())
        else {
            return;
        };
        if is_ref(leaf_v) != is_ref(decl_v) || (!is_ref(leaf_v) && leaf_v != decl_v) {
            return;
        }
        if is_ref(leaf_v) && leaf_v != "Ljava/lang/Object;" {
            // Admitted exactly as the door admits it (`reference_arg_admitted`
            // is the predicate its leaf and inner casts refuse on), and only
            // for a plain class instance: an array's header carries its
            // component's class id.
            let Value::Object(Some(o)) = *v else {
                return;
            };
            if ctx.object_is_array(o) || reference_arg_admitted(ctx, *v, leaf_v) != Some(true) {
                return;
            }
            value_check = Some(ctx.class_id_of_object(o).as_u32() as i32);
        }
    } else {
        let same = leaf_ret == decl_ret;
        let object_return = is_ref(&leaf_ret) && decl_ret == "Ljava/lang/Object;";
        if !(same || object_return) || leaf_ret == "V" {
            return;
        }
    }
    // An instance link naming the owner (wave 65; set below).
    let mut owner_admits = false;
    // The receiver this call passes and its exact class (wave 71's note).
    let mut admitted: Option<(ObjectRef, i32)> = None;
    // The class the field's `volatile` bit is read from (wave 79, `FL_VOLATILE`).
    let volatile_start: ClassId;
    let (slot, class_raw) = if is_static {
        // The door's static arms: the declaring class of an inherited static,
        // else the recorded owner. A by-name owner (no recorded one) is left to
        // the door, which resolves and initializes it per call.
        let holder = mh_inherited_static_field_holder(&*ctx, bound, &class, &name);
        let Some(owner) = holder.or_else(|| mh_static_owner(bound)) else {
            return;
        };
        // Only a class whose initialization is complete: from then on the
        // door's `ensure_class_initialized` is a no-op for it.
        if !ctx.is_class_initialized(owner) {
            return;
        }
        let Some(slot) = ctx.static_field_index_by_name(owner, &name) else {
            return;
        };
        volatile_start = owner;
        (slot, owner.as_u32() as i32)
    } else {
        let receiver = match extra.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return,
        };
        if ctx.object_is_array(receiver) {
            return;
        }
        // The receiver cast the door makes (`leaf_receiver_refusal`, and the
        // inner cast from the declared receiver type): admitted for sure.
        let leaf_recv = match leaf_params.first() {
            Some(p) => p.as_str(),
            None => return,
        };
        let receiver_cid = ctx.class_id_of_object(receiver);
        admitted = Some((receiver, receiver_cid.as_u32() as i32));
        // Stage 2 (wave 63, `CRATONVM_MH_OWNER_ID_AUTHORITY`): a recorded
        // owner admits by id alone -- the door refuses another loader's
        // namesake, which the by-name admission below would link -- and names
        // the slot's class.
        let link_owner = mh_field_slot_owner(&*ctx, this);
        if let Some(owner) = link_owner {
            if receiver_cid != owner && !ctx.is_subclass(receiver_cid, owner) {
                return;
            }
        } else {
            let exact = ctx.class_name_arc_of_id(receiver_cid).as_deref() == Some(class.as_str());
            if !exact
                && reference_arg_admitted(ctx, Value::Object(Some(receiver)), leaf_recv)
                    != Some(true)
            {
                return;
            }
        }
        // The slot carried from mint (wave 64, `CRATONVM_MH_FIELD_SLOT_FROM_MINT`)
        // is the one the by-id lookup would answer for `link_owner`.
        let minted = mh_field_arm_minted_slot(&*ctx, this, link_owner);
        let Some(slot) =
            minted.or_else(|| mh_instance_field_slot(&*ctx, link_owner, &class, &name))
        else {
            return;
        };
        volatile_start = link_owner.unwrap_or(receiver_cid);
        // Wave 65 (`CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`): a slot that is the
        // OWNER's (minted, or found in the owner by id) is the slot of every
        // subclass receiver too, so the link names the owner and admits its
        // subclasses ([`field_link_receiver_admitted`]). A by-name slot, or a
        // lambda proxy's capture, keeps the exact receiver class.
        let owner_link = match link_owner {
            Some(owner) if mh_field_link_owner_admits_enabled() => {
                let owners_slot = minted.is_some()
                    || ctx.resolve_field_index_by_class_id(owner, &name) == Some(slot);
                owners_slot.then_some(owner)
            }
            _ => None,
        };
        match owner_link {
            Some(owner) => {
                owner_admits = true;
                (slot, owner.as_u32() as i32)
            }
            None => (slot, receiver_cid.as_u32() as i32),
        }
    };
    if slot > 0xFFFF || class_raw == 0 {
        return;
    }
    let Some(site_id) = field_link_site_id(&declared) else {
        return;
    };
    let Ok(site_id) = i32::try_from(site_id) else {
        return;
    };
    let mut state = FL_LINKED | ((type_idx as i32) << 16) | slot as i32;
    state |= (links + 1) << FL_LINKS_SHIFT;
    if !is_static {
        // A minted slot survives the link (wave 64): its bits are this slot.
        state = field_link_state_keeping_mint(state, old, slot);
    }
    if is_static {
        state |= FL_STATIC;
    }
    if setter {
        state |= FL_SETTER;
    }
    if value_check.is_some() {
        state |= FL_VALUE_CHECK;
    }
    if owner_admits {
        state |= FL_OWNER_ADMITS;
    }
    if field_link_field_is_volatile(&*ctx, volatile_start, &name, is_static, slot) {
        state |= FL_VOLATILE;
    }
    ctx.set_field(this, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(this, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
    ctx.set_field(
        this,
        MH_FIELD_LINK_VALUE,
        Value::Int(value_check.unwrap_or(0)),
    );
    ctx.set_field(this, MH_FIELD_LINK_SITE, Value::Int(site_id));
    // Last: a reader never sees the state before the slots it describes.
    ctx.set_field(this, MH_FIELD_LINK, Value::Int(state));
    *note = Some(FieldLinkWritten {
        state,
        class_raw,
        receiver: admitted.map(|(r, _)| r),
        recv_raw: admitted.map_or(0, |(_, raw)| raw),
        site_is_declared: call_site == Some(declared),
        site_id,
    });
    if crate::nbflags().dbg_mh_field_link && !ctx.is_jdk_only() {
        note_compatible_field_link(&class, &name, &declared, is_static);
    }
}

/// [`mh_field_link_try`] for a BOUND instance getter / setter (interpreter
/// round i2 wave 66, lane L4; `CRATONVM_MH_FIELD_LINK_BOUND`; proposal
/// `i65-L4-proposal-bound-field-handles-link-too`). `receiver` is `this`'s
/// `MH_BOUND` object, `old` / `links` its packed state and relink count, and
/// the kind, marking and relink bound are already checked.
///
/// Linked only when the door's bound arm answers this call with a plain field
/// access of the same slot: no cast chain, the call site at the declared type,
/// the shape [`bound_field_link_value_type`] admits, a recorded slot owner
/// ([`mh_field_slot_owner`], which the bound copy carries since wave 65) that
/// admits the bound receiver BY ID (exact or a subclass; a receiver the door's
/// by-name judgement let through is not linked), and the owner's slot (minted,
/// or found in the owner by id -- the door's own first answer). The link names
/// the receiver's exact class, which [`mh_field_link_fast`] re-checks per call.
/// A reference setter's value is judged as the unbound link judges it.
/// Performs no allocation and no Java call.
///
/// `call` (wave 67): an `invokeExact`'s arguments and site, or
/// [`BoundLinkCall::BindTo`] -- `bindTo`'s pre-link of its fresh copy, which
/// skips the two call judgements (site and arity) and links no shape that
/// needs a value check; every other admission is this function's, shared.
///
/// `declared` (wave 70, `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`): the handle's
/// declared descriptor when the caller already has it, else `None` and it is
/// looked up here, as before.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_field_link_try_bound(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    receiver: ObjectRef,
    setter: bool,
    old: i32,
    links: i32,
    call: BoundLinkCall<'_>,
    declared: Option<&str>,
    note: &mut Option<FieldLinkWritten>,
) {
    if matches!(ctx.get_field(this, MH_CAST_CHAIN), Value::Object(Some(_))) {
        return;
    }
    if ctx.object_is_array(receiver) {
        return;
    }
    let Some(owner) = mh_field_slot_owner(&*ctx, this) else {
        return;
    };
    let receiver_cid = ctx.class_id_of_object(receiver);
    if receiver_cid != owner && !ctx.is_subclass(receiver_cid, owner) {
        return;
    }
    let looked_up: String;
    let declared: &str = match declared {
        Some(d) => d,
        None => {
            let Some(d) = mh_declared_descriptor(ctx, this) else {
                return;
            };
            looked_up = d;
            &looked_up
        }
    };
    // A call site the door refuses (`WrongMethodTypeException`) is not linked.
    if let BoundLinkCall::Invoke { call_site, .. } = call {
        if mh_strict_exact_signature() && call_site != Some(declared) {
            return;
        }
    }
    let (Some(name), Some(desc)) = (mh_read_name(&*ctx, this), mh_read_desc(&*ctx, this)) else {
        return;
    };
    let Some(type_idx) = ctx.resolve_field_index("java/lang/invoke/MethodHandle", "type") else {
        return;
    };
    if type_idx > FL_TYPE_INDEX_MASK as usize {
        return;
    }
    let type_obj = match ctx.get_field(this, type_idx) {
        Value::Object(Some(t)) => t,
        _ => return,
    };
    let (Some((leaf_params, leaf_ret)), Some((decl_params, decl_ret))) = (
        split_descriptor_params(&desc),
        split_descriptor_params(&declared),
    ) else {
        return;
    };
    let Some(value_type) =
        bound_field_link_value_type(setter, &leaf_params, &leaf_ret, &decl_params, &decl_ret)
    else {
        return;
    };
    let Some(extra) = bound_link_call_extra(call, value_type, decl_params.len()) else {
        return;
    };
    let mut value_check: Option<i32> = None;
    if let Some(leaf_v) = value_type {
        // Admitted exactly as the door admits it, and only for a plain class
        // instance (the unbound link's rule).
        let Some(Value::Object(Some(o))) = extra.last().copied() else {
            return;
        };
        if ctx.object_is_array(o)
            || reference_arg_admitted(ctx, Value::Object(Some(o)), leaf_v) != Some(true)
        {
            return;
        }
        value_check = Some(ctx.class_id_of_object(o).as_u32() as i32);
    }
    let minted = mh_field_arm_minted_slot(&*ctx, this, Some(owner));
    let Some(slot) = minted.or_else(|| ctx.resolve_field_index_by_class_id(owner, &name)) else {
        return;
    };
    let class_raw = receiver_cid.as_u32() as i32;
    if slot > 0xFFFF || class_raw == 0 {
        return;
    }
    let Some(site_id) = field_link_site_id(&declared) else {
        return;
    };
    let Ok(site_id) = i32::try_from(site_id) else {
        return;
    };
    let mut state = FL_LINKED | FL_BOUND | ((type_idx as i32) << 16) | slot as i32;
    state |= (links + 1) << FL_LINKS_SHIFT;
    state = field_link_state_keeping_mint(state, old, slot);
    if setter {
        state |= FL_SETTER;
    }
    if value_check.is_some() {
        state |= FL_VALUE_CHECK;
    }
    // Wave 79 (`FL_VOLATILE`): the owner's field, judged once here.
    if field_link_field_is_volatile(&*ctx, owner, &name, false, slot) {
        state |= FL_VOLATILE;
    }
    ctx.set_field(this, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(this, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
    ctx.set_field(
        this,
        MH_FIELD_LINK_VALUE,
        Value::Int(value_check.unwrap_or(0)),
    );
    ctx.set_field(this, MH_FIELD_LINK_SITE, Value::Int(site_id));
    // Last: a reader never sees the state before the slots it describes.
    ctx.set_field(this, MH_FIELD_LINK, Value::Int(state));
    // `bindTo`'s pre-link serves no call: only an `invokeExact` at the
    // declared type is noted as one the link answers.
    let site_is_declared = match call {
        BoundLinkCall::Invoke { call_site, .. } => call_site == Some(declared),
        BoundLinkCall::BindTo => false,
    };
    *note = Some(FieldLinkWritten {
        state,
        class_raw,
        receiver: Some(receiver),
        recv_raw: class_raw,
        site_is_declared,
        site_id,
    });
    if crate::nbflags().dbg_mh_field_link && !ctx.is_jdk_only() {
        let class = mh_read_class(&*ctx, this).unwrap_or_default();
        note_compatible_field_link(&class, &name, &declared, false);
    }
}

/// `CRATONVM_MH_BINDTO_PRELINK_FIELD` (default OFF, opt-in, performance;
/// interpreter round i2 wave 67, lane L4; proposal
/// `i66-L4-proposal-bindto-pre-links-a-bound-field-copy-20261030`): `bindTo`
/// links the bound copy of an instance field getter / setter as it mints it
/// ([`mh_bindto_prelink_bound_field`]), so a fresh copy's FIRST `invokeExact`
/// is already served by [`mh_field_link_fast`]. Needs
/// `CRATONVM_MH_FIELD_LINK_BOUND` (the link it writes is that switch's).
pub(crate) fn mh_bindto_prelink_field_enabled() -> bool {
    crate::nbflags().mh_bindto_prelink_field
}

/// `bindTo`'s pre-link (wave 67, `CRATONVM_MH_BINDTO_PRELINK_FIELD`): write
/// on the fresh bound copy `mh` (current, fully minted: `type`, `MH_BOUND`
/// and the field carry written) the bound link its first `invokeExact` would
/// write, through the same admission ([`field_link_candidate`] and
/// [`mh_field_link_try_bound`] with [`BoundLinkCall::BindTo`]). The link
/// names the bound receiver's exact class, which every linked call re-checks;
/// a reference setter that needs a value check is left to its first call.
/// Performs no allocation and no Java call; cannot throw.
pub(crate) fn mh_bindto_prelink_bound_field(ctx: &mut dyn NativeContext, mh: ObjectRef) {
    if !mh_bindto_prelink_field_enabled() || !mh_field_link_bound_enabled() {
        return;
    }
    let Some(FieldLinkCandidate { setter, old, links }) = field_link_candidate(ctx, mh) else {
        return;
    };
    let Value::Object(Some(receiver)) = ctx.get_field(mh, MH_BOUND) else {
        return;
    };
    mh_field_link_try_bound(
        ctx,
        mh,
        receiver,
        setter,
        old,
        links,
        BoundLinkCall::BindTo,
        None,
        &mut None,
    );
}

/// `CRATONVM_DBG_MH_FIELD_LINK`'s line: a field handle linked under
/// `--compatible` (`CRATONVM_MH_FIELD_LINK_COMPATIBLE`'s positive control).
#[cold]
#[inline(never)]
pub(crate) fn note_compatible_field_link(class: &str, name: &str, declared: &str, is_static: bool) {
    let kind = if is_static { "static" } else { "instance" };
    eprintln!("[mh-field-link] --compatible {kind} link {class}.{name} at {declared}");
}

/// Clear a copied handle's link: the copy is a different handle (its `type`
/// is about to be restamped, or its marking changed), and whatever the source
/// was linked for does not describe it. Width-guarded.
pub(crate) fn mh_clear_field_link(ctx: &dyn NativeContext, mh: ObjectRef) {
    if ctx.object_num_fields(mh) >= MH_SLOT_COUNT {
        // The slot carried from mint describes the member, which the copy
        // shares (wave 64, `CRATONVM_MH_FIELD_SLOT_FROM_MINT`); `0` otherwise.
        let kept = match ctx.get_field(mh, MH_FIELD_LINK) {
            Value::Int(state) => field_link_state_kept_by_copy(state),
            _ => 0,
        };
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(kept));
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(None));
    }
}

/// `CRATONVM_MH_BINDTO_FIXED_ARITY` -- default ON (round 15 wave 4, lane mh3;
/// `r15w4-mh2-bindto-keeps-varargs-collector-FIXED-20260930.md`, HotSpot's answer
/// per `R15Mh2BoundVarargs`); `0`/`false`/`off`/`no` restores the old
/// answers: `bindTo` of a variable-arity member minted another collector, and
/// the `invoke` / `invokeWithArguments` doors collected a bound handle's
/// surplus arguments. `MethodHandle.bindTo` is `bindArgumentL(0, x)` on
/// `rebind()`, and `AsVarargsCollector` rebinds its fixed-arity target, so the
/// result is a `BoundMethodHandle`, which "is never a variable-arity method
/// handle, even if the original target method handle was" (its javadoc):
/// `isVarargsCollector()` is `false`, and any other arity is `asType`'s
/// `WrongMethodTypeException` ([`bound_handle_arity_refusal`]). Both modes.
/// Read once.
pub(crate) fn mh_bindto_fixed_arity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_FIXED_ARITY")
    })
}

/// `CRATONVM_MH_BINDTO_SHARED_MINT` -- default ON (round 15 wave 4, lane mh3;
/// proposal MH2-3 of `jit-r15w4-mh2-proposals-RETIRED-20261003.md`); `0`/`false`/`off`/`no`
/// makes `bindTo`'s ordinary mint go through [`alloc_method_handle`] again:
/// three fresh `String`s for `MH_CLASS` / `MH_NAME` / `MH_DESC`, a `type` built
/// for the UNBOUND signature and then replaced, and a `declared_methods` walk
/// for the variable-arity bit. [`alloc_bound_method_handle`] shares the
/// source's three (immutable) strings and builds the bound `type` once.
/// Performance only, both modes. Read once.
pub(crate) fn mh_bindto_shared_mint_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_SHARED_MINT")
    })
}

/// The adapter kinds [`bound_handle_arity_refusal`] judges: the combinator
/// kinds whose `type` is measured authoritative
/// ([`kind_has_authoritative_adapted_type`]) that are not direct members
/// (round 15 wave 5, lane mh4).
pub(crate) fn mh_adapter_arity_kind(kind: i32) -> bool {
    kind_has_authoritative_adapted_type(kind) && !kind_has_authoritative_type(kind)
}

/// `CRATONVM_MH_INVOKE_ADAPTER_ARITY` -- default ON (round 15 wave 5, lane
/// mh4; `r15w4-mh3-adapter-invoke-has-no-arity-refusal-FIXED-20260930.md`);
/// `0`/`false`/`off`/`no` stops [`bound_handle_arity_refusal`] judging the
/// adapter kinds and makes `insertArguments(t, pos)` with no values mint an
/// INSERT adapter again instead of answering `t`. Both modes. Read only on the
/// cold paths (about to refuse; an empty `insertArguments`).
pub(crate) fn mh_invoke_adapter_arity_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_ADAPTER_ARITY")
}

/// The `WrongMethodTypeException` `asType(site)` raises for a BOUND direct
/// handle (what `bindTo` mints: a `STATIC` handle with its leading argument, a
/// `VIRTUAL` / `SPECIAL` one with its receiver, in `MH_BOUND`) that is not a
/// variable-arity collector, when `site` names another number of arguments
/// than its `type` has; `None` otherwise (round 15 wave 4, lane mh3). `site` is
/// the `invoke` call site, or `invokeWithArguments`' `genericMethodType(n)`.
///
/// The permissive path below the doors never refused this: a bound handle
/// skips [`mh_entry_adapt`] (its arity is the unbound member's), and
/// [`collect_trailing_varargs`] gathers any surplus by the MEMBER's
/// `ACC_VARARGS`, so `findStatic(C, "cat", (String,String[])String)
/// .bindTo("-").invoke("a", "b")` answered `"a-b"` where HotSpot throws.
///
/// The same refusal for an UNBOUND virtual / special handle (the caller passes
/// its receiver), under its own switch `CRATONVM_MH_INVOKE_VIRTUAL_ARITY`
/// (default ON, read only when about to refuse): nothing refused
/// `findVirtual(String, "concat", (String)String).invoke("a", "b", "c")`, and
/// the dispatch handed `concat` two arguments. An unbound `STATIC` handle is
/// [`mh_entry_adapt`]'s, at the dispatch.
///
/// Refuses on a positive reading only: the `type` must be the leaf's own
/// parameter list minus what the bind captured, or plus the receiver of an
/// unbound virtual one (`MH_DESC` names a `STATIC` handle's bound parameter
/// and never a receiver), so a `type` this model failed to narrow is never
/// judged. The switches ([`mh_bindto_fixed_arity_enabled`]) and
/// `CRATONVM_MH_STRICT_INVOKEEXACT` are read only after the cheap screens.
///
/// Round 15 wave 5 (lane mh4): the ADAPTER kinds too ([`mh_adapter_arity_kind`];
/// switch `CRATONVM_MH_INVOKE_ADAPTER_ARITY`, default ON, read only when about
/// to refuse). HotSpot's adapters are `BoundMethodHandle`s, fixed-arity unless
/// `asVarargsCollector` was applied (the marking, screened below), so
/// `insertArguments(cat, 0, "-").invoke("a", "b")`, a second `bindTo`,
/// `filterArguments(..)` / `dropArguments(..)` at another arity are
/// `WrongMethodTypeException` (ref15 `R15Mh3AdapterArity`); the dispatch below
/// collected the surplus by the LEAF's `ACC_VARARGS` or passed it on. An
/// adapter's `type` is the adapted one (the combinator sweeps measured it), and
/// its `MH_DESC` is its leaf's, so no leaf cross-check applies to it; a `()V`
/// `type` is `alloc_method_handle`'s unknown-descriptor stand-in (C21) and is
/// not judged, as `invoker_call_target` does not judge one.
pub(crate) fn bound_handle_arity_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    site: &str,
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return None;
    }
    // Interpreter round i3 wave 79, lane L4 (page
    // `i70-orch-compatible-bound-getter-invoke-ignores-an-extra-argument`): the
    // field accessors too. `findGetter(Box, "v", int).bindTo(box).invoke(5)`
    // (`()int` at `(int)int`) answered the field, its door arm reading the bound
    // receiver and dropping the surplus; HotSpot's `asType` refuses the arity
    // (`cannot convert MethodHandle()int to (int)int`). Bound or not, static
    // or instance: a field accessor is fixed-arity, and its `type` is its
    // `MH_DESC` (unbound) or that minus the receiver (bound), checked below.
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k @ (MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_SPECIAL)) => k,
        Value::Int(k @ (MH_KIND_GETTER | MH_KIND_SETTER)) => k,
        Value::Int(k) if mh_adapter_arity_kind(k) => k,
        _ => return None,
    };
    let adapter = mh_adapter_arity_kind(kind);
    let field = matches!(kind, MH_KIND_GETTER | MH_KIND_SETTER);
    let bound = matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
    if (kind == MH_KIND_STATIC && !bound) || mh_is_varargs_collector(&*ctx, mh) {
        return None;
    }
    let declared = declared?;
    if declared == site || !mh_strict_exact_signature() || (adapter && declared == "()V") {
        return None;
    }
    let (declared_params, _) = split_descriptor_params(declared)?;
    let (site_params, _) = split_descriptor_params(site)?;
    if declared_params.len() == site_params.len() {
        return None;
    }
    if !adapter {
        let leaf = mh_read_desc(&*ctx, mh)?;
        let (leaf_params, _) = split_descriptor_params(&leaf)?;
        let consistent = match (kind, bound) {
            (MH_KIND_STATIC, _) => leaf_params.len() == declared_params.len() + 1,
            // A field accessor's `MH_DESC` names its receiver (wave 79).
            (_, true) if field => leaf_params.len() == declared_params.len() + 1,
            (_, false) if field => leaf_params.len() == declared_params.len(),
            (_, true) => leaf_params.len() == declared_params.len(),
            (_, false) => leaf_params.len() + 1 == declared_params.len(),
        };
        if !consistent {
            return None;
        }
    }
    let switched_on = if adapter {
        mh_invoke_adapter_arity_enabled()
    } else if field {
        // Wave 79: no switch of its own (a refusal path, HotSpot's answer);
        // `CRATONVM_MH_STRICT_INVOKEEXACT=0` above still turns it off.
        true
    } else if bound {
        mh_bindto_fixed_arity_enabled()
    } else {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_VIRTUAL_ARITY")
    };
    if !switched_on {
        return None;
    }
    let shown_declared = method_type_display_in(&*ctx, declared)?;
    let shown_site = method_type_display_in(&*ctx, site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

/// Does `bindTo` on a handle of `kind` mint the `__adapter__` INSERT handle
/// (`insertArguments(this, 0, x)`, which dispatches `this` itself with `x`
/// prepended) rather than a copy of `this` with `x` in `MH_BOUND`?
///
/// The copy is right only where the dispatch arm reads an object in `MH_BOUND`
/// as the captured leading argument and nothing else lives there: an unbound
/// `STATIC` / `VIRTUAL` / `SPECIAL` / `GETTER` / array-accessor / `IDENTITY`
/// handle and an instance `SETTER`; a `CONSTANT` (its value stays) and a
/// lambda factory (its own capture array) are the copy's too. Everything else
/// was wrong on the copy (round 15 wave 4, lane mh3; switch
/// `CRATONVM_MH_BINDTO_INSERT_ADAPTER`, default ON, read only for those
/// kinds): an adapter keeps its wrapper in `MH_BOUND` and the copy overwrote
/// it with the bound value (`insertArguments(h, 1, v).bindTo(x)` then read the
/// value's fields as the wrapper's), a constructor keeps its class id there
/// and never prepends a bound argument, and an invoker and a static setter
/// ignore `MH_BOUND`. A bound handle of the four direct kinds binds its NEXT
/// parameter, which the copy's single slot cannot hold either (the `STATIC` /
/// `VIRTUAL` / `SPECIAL` arm predates this function).
///
/// Round 15 wave 5 (lane mh4; MH3-3): answered from [`mh_bound_role`]. The
/// copy is right for a [`BoundRole::Capture`] handle not yet bound, a
/// [`BoundRole::Value`] and a [`BoundRole::Captures`]; every other role (and a
/// kind this model does not know) takes the adapter. The one answer that moved:
/// a STATIC getter (`()T`, an `Owner`) now takes the adapter, where it copied;
/// the leading-parameter guard in `bindTo` refuses it first whenever its `type`
/// reads (HotSpot: "no leading reference parameter").
pub(crate) fn bindto_takes_insert_adapter(kind: i32, already_bound: bool, desc: &str) -> bool {
    let wrong_on_the_copy = match mh_bound_role(kind, desc) {
        Some(BoundRole::Capture) => already_bound,
        Some(BoundRole::Value | BoundRole::Captures) => false,
        Some(
            BoundRole::Owner
            | BoundRole::Wrapper
            | BoundRole::CallSite
            | BoundRole::Data
            | BoundRole::Unused,
        )
        | None => true,
    };
    wrong_on_the_copy
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_INSERT_ADAPTER")
}

/// Does `bindTo(null)` take the INSERT adapter (JIT round 16, lane mh)? For a
/// [`BoundRole::Capture`] kind the copy's `MH_BOUND` IS the captured argument,
/// and a null there reads as "not bound": `findStatic(str, (String)String)
/// .bindTo(null).invokeExact()` was refused as a call with 0 arguments where
/// HotSpot answers `null`, and `findVirtual(String, length).bindTo(null)
/// .invokeExact()` answered `0` where HotSpot throws
/// `NullPointerException` (`R16MhErrors` `bind-null` / `bind-virtual-null`).
/// The adapter keeps the null in its values array and splices it in at
/// dispatch, so the target sees it exactly as an argument. Switch
/// `CRATONVM_MH_BINDTO_NULL_INSERT` (default ON, a correctness fix; both
/// modes), read only for a null capture.
pub(crate) fn bindto_null_takes_insert_adapter(kind: i32, recv: Value, desc: &str) -> bool {
    matches!(recv, Value::Object(None))
        && mh_bound_role(kind, desc) == Some(BoundRole::Capture)
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_NULL_INSERT")
}

/// What a handle's `MH_BOUND` slot holds, by kind (round 15 wave 5, lane mh4;
/// proposal MH3-3 of `jit-r15w4-mh3-proposals-RETIRED-20261003.md`). One place a new kind states
/// it, instead of each reader re-deriving it from the kinds it happens to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundRole {
    /// The captured leading argument once `bindTo` stored an object there
    /// (direct members, instance field accessors, array accessors, identity);
    /// before that nothing, or for a `STATIC` member the recorded owner's
    /// `Int` ([`mh_record_static_owner`]; a bound one keeps it in
    /// [`MH_STATIC_OWNER`]).
    Capture,
    /// A recorded owner `Int`, never a captured argument (a constructor, a
    /// static field accessor).
    Owner,
    /// A combinator carrier (target, operands, position; every adapter).
    Wrapper,
    /// The answer itself (`constant` / `empty`).
    Value,
    /// A lambda factory's `Object[]` of captures, grown by each `bindTo`.
    Captures,
    /// The `CallSite` a dynamic invoker reads its target from.
    CallSite,
    /// A kind-private carrier that is neither an argument nor a combinator
    /// (a string concatenation's constants, a record deserializer's
    /// `ObjectStreamClass`).
    Data,
    /// Not read (an invoker takes its target as the first argument).
    Unused,
}

/// [`BoundRole`] of a handle of `kind` whose `MH_DESC` is `desc` (the field
/// accessors differ between their static and instance forms: an instance
/// getter / setter's descriptor names the receiver), or `None` for a kind this
/// model does not mint. Pure.
pub(crate) fn mh_bound_role(kind: i32, desc: &str) -> Option<BoundRole> {
    Some(match kind {
        MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_SPECIAL | MH_KIND_ARRAY_GET
        | MH_KIND_ARRAY_SET | MH_KIND_ARRAY_LENGTH | MH_KIND_IDENTITY => BoundRole::Capture,
        // An unreadable descriptor keeps the instance answer (a capture), which
        // is what every reader assumed before this function.
        MH_KIND_GETTER => match split_descriptor_params(desc) {
            Some((params, _)) if params.is_empty() => BoundRole::Owner,
            _ => BoundRole::Capture,
        },
        MH_KIND_SETTER if desc_has_two_params(desc) => BoundRole::Capture,
        MH_KIND_SETTER | MH_KIND_CONSTRUCTOR => BoundRole::Owner,
        MH_KIND_PERMUTE
        | MH_KIND_GUARD
        | MH_KIND_DROP
        | MH_KIND_INSERT
        | MH_KIND_COLLECT
        | MH_KIND_SPREAD
        | MH_KIND_FILTER
        | MH_KIND_FOLD
        | MH_KIND_CATCH
        | MH_KIND_RETURN_FILTER
        | MH_KIND_COLLECT_ARGS
        | MH_KIND_LOOP_COUNTED
        | MH_KIND_LOOP_WHILE
        | MH_KIND_LOOP_DOWHILE
        | MH_KIND_LOOP_ITERATE
        | MH_KIND_TRY_FINALLY
        | MH_KIND_TABLE_SWITCH => BoundRole::Wrapper,
        MH_KIND_CONSTANT => BoundRole::Value,
        MH_KIND_LAMBDA_FACTORY => BoundRole::Captures,
        MH_KIND_DYNAMIC_INVOKER => BoundRole::CallSite,
        MH_KIND_STRING_CONCAT | MH_KIND_RECORD_DESER => BoundRole::Data,
        MH_KIND_INVOKER => BoundRole::Unused,
        _ => return None,
    })
}

/// Does `bindTo`'s ordinary mint of `this` take [`alloc_bound_method_handle`]
/// (MH2-3)? Only for the kinds whose bound `type` [`bindto_bound_type_descriptor`]
/// knows, and only for a handle of this model's layout (a readable `MH_KIND`
/// and a string object in each of `MH_CLASS` / `MH_NAME` / `MH_DESC`, which
/// the copy shares; anything else keeps [`alloc_method_handle`], which writes
/// `""` for a missing one). Reads only. (`bindTo` itself asks
/// [`bindto_shared_mint_applies_read`] with the strings it already decoded.)
#[cfg(test)]
pub(crate) fn bindto_shared_mint_applies(ctx: &dyn NativeContext, this: ObjectRef, kind: i32) -> bool {
    let strings_read =
        [MH_CLASS, MH_NAME, MH_DESC]
            .into_iter()
            .all(|slot| match ctx.get_field(this, slot) {
                Value::Object(Some(s)) => ctx.read_string(s).is_some(),
                _ => false,
            });
    bindto_shared_mint_applies_read(ctx, this, kind, strings_read)
}

/// `bindto_shared_mint_applies` for a caller that has ALREADY decoded the
/// three strings (JIT round 16, lane mh): `bindTo` reads `MH_CLASS` /
/// `MH_NAME` / `MH_DESC` through `mh_read_class` / `_name` / `_desc` (the same
/// `read_string` of the same slot) a few lines earlier, with no GC point
/// between, so `strings_read` is that answer and the three decodes (a Rust
/// `String` each, every bind) are not paid twice.
pub(crate) fn bindto_shared_mint_applies_read(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    kind: i32,
    strings_read: bool,
) -> bool {
    matches!(
        kind,
        MH_KIND_STATIC
            | MH_KIND_VIRTUAL
            | MH_KIND_SPECIAL
            | MH_KIND_GETTER
            | MH_KIND_SETTER
            | MH_KIND_ARRAY_GET
            | MH_KIND_ARRAY_SET
            | MH_KIND_ARRAY_LENGTH
            | MH_KIND_IDENTITY
            | MH_KIND_CONSTANT
    ) && ctx.object_num_fields(this) > MH_CAST_CHAIN
        && matches!(ctx.get_field(this, MH_KIND), Value::Int(_))
        && strings_read
        && mh_bindto_shared_mint_enabled()
}

/// The `type` descriptor `bindTo`'s ordinary mint gives the bound handle: for a
/// virtual / special handle its stamped type minus the receiver when it had one
/// (`bound_virtual_type`), else `MH_DESC` (receiver-less); for the other kinds
/// the handle's `type` minus its leading parameter, or `MH_DESC` when `type`
/// does not read. The same answers the two post-mint rebuilds in `bindTo` give
/// (which the shared mint skips). `this` must be current; reads only.
#[cfg(test)]
pub(crate) fn bindto_bound_type_descriptor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    kind: i32,
    desc: &str,
    bound_virtual_type: Option<&str>,
) -> String {
    bindto_bound_type_descriptor_with(ctx, this, kind, desc, bound_virtual_type, None)
}

/// `bindto_bound_type_descriptor` with the `type` render `bindTo` already made
/// for its leading-parameter guard (`rendered_type`, round 16 lane mh): the
/// same `methodtype_to_descriptor` of the same `type` object, so the bound
/// type of a non-virtual handle no longer renders it (a mirror-to-name
/// resolution per parameter) a second time per bind. `None` renders it here.
pub(crate) fn bindto_bound_type_descriptor_with(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    kind: i32,
    desc: &str,
    bound_virtual_type: Option<&str>,
    rendered_type: Option<&str>,
) -> String {
    if kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL {
        return bound_virtual_type.unwrap_or(desc).to_string();
    }
    let type_desc = match rendered_type {
        Some(t) => Some(t.to_string()),
        None => mh_type_descriptor(ctx, this),
    };
    match type_desc.and_then(|t| split_descriptor_params(&t)) {
        Some((mut params, ret)) => {
            if !params.is_empty() {
                params.remove(0);
            }
            format!("({}){}", params.concat(), ret)
        }
        None => desc.to_string(),
    }
}

/// `CRATONVM_MH_BOUND_TYPE_TABLE` -- default ON since round 16 (JIT round 16, lane mh;
/// proposal MH3-2 of `jit-r15w4-mh3-proposals-RETIRED-20261003.md`): `bindTo` takes the bound
/// handle's `type` from the VM's descriptor-keyed `MethodType` table
/// ([`bound_method_type_from_descriptor`]) instead of building a fresh
/// `MethodType` (a `Class[]`, the type, its form and the form's two cache
/// arrays) per bind. Every bind of one target then presents the SAME four
/// field objects, so the direct lane's record memo hits for bound handles, and
/// `mh.bindTo(a).type() == mh.bindTo(b).type()` as on HotSpot (which interns).
/// Measured: `R16MhFloor` bind-each 10182→5905 ns. `0`/`false`/`off`/`no`
/// turns it off (a fresh `MethodType` per bind, as before). Read once.
pub(crate) fn mh_bound_type_table_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BOUND_TYPE_TABLE")
    })
}

/// `CRATONVM_MH_ARRAY_SETTER_COMPONENT_CAST` -- default ON (JIT round 16,
/// lane mh; a correctness fix, both modes): an `arrayElementSetter` handle's
/// reference value is cast to the handle's own component before the access
/// (`ClassCastException`, as HotSpot's `asType`), not refused by the store
/// (`ArrayStoreException`). `0`/`false`/`off`/`no` restores the store-only
/// refusal. Read once.
pub(crate) fn mh_array_setter_component_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_ARRAY_SETTER_COMPONENT_CAST")
    })
}

/// May the bound type `desc` live in the VM's shared `MethodType` table
/// (MH3-2)? Only when every component -- each parameter and the return -- is a
/// primitive (or `void`), or a `java/...` class (or an array of one, or a
/// primitive array) that is ALREADY loaded. Why each conjunct:
///
/// * `java/...`: only the bootstrap / platform loaders define `java.*`
///   classes, and they never unload, so a row (rooted outright by the VM's
///   `bound-method-types` native-roots row) retains no user loader and never
///   names an unloaded class. Names that look like a hidden or spun class
///   (`$$`, `/0x`, `.`) are refused anyway.
/// * already loaded: [`mirror_for_descriptor_name`] answers a loaded class's
///   own mirror, and a stand-in for one it cannot find; a stand-in frozen into
///   the table would outlive the day the class loads. A class found now is
///   found by every later build, so the row is what every build would answer.
///
/// Reads only (`class_id_by_name`); no allocation.
pub(crate) fn bound_method_type_shareable(ctx: &dyn NativeContext, desc: &str) -> bool {
    if !desc.starts_with('(') {
        return false;
    }
    let Some(close) = desc_close_paren(desc) else {
        return false;
    };
    let ret = descriptor_to_class_name(&desc[close + 1..]);
    parse_descriptor_types(&desc[1..close])
        .iter()
        .map(|n| n.as_ref())
        .chain(std::iter::once(ret.as_ref()))
        .all(|name| bound_type_component_shareable(ctx, name))
}

/// One component of [`bound_method_type_shareable`], as
/// [`parse_descriptor_types`] / [`descriptor_to_class_name`] spell it
/// (`int`, `java/lang/String`, `[Ljava/lang/String;`, `[I`).
pub(crate) fn bound_type_component_shareable(ctx: &dyn NativeContext, name: &str) -> bool {
    if is_primitive_descriptor_name(name) {
        return true;
    }
    // A fabricated stand-in is "loaded" too, but it is the very thing a row
    // must not freeze; asked only on a table miss. A class outside `java/`
    // is never a descriptor-keyed row: MH16-1 keys it by class
    // ([`bound_method_type_through_class_key`], round 16 wave 15).
    bound_type_component_java_named(name)
        && ctx.class_id_by_name(name).is_some()
        && !ctx.is_class_synthetic_stub(name)
}

/// `int` .. `void`: a component [`mirror_for_descriptor_name`] answers with
/// a primitive mirror, which no loader defines.
pub(crate) fn is_primitive_descriptor_name(name: &str) -> bool {
    matches!(
        name,
        "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short" | "void"
    )
}

/// A name only the hidden-class or spinning machinery produces (`$$`, `/0x`,
/// a `.`); never a row.
pub(crate) fn is_spun_looking_descriptor_name(name: &str) -> bool {
    name.contains("$$") || name.contains("/0x") || name.contains('.')
}

/// By its NAME alone, is the reference component `name` a `java/...` class,
/// an array of one, or a primitive array? Only built-in loaders may define
/// `java.*` (a user loader is refused the package), so the name is the class.
pub(crate) fn bound_type_component_java_named(name: &str) -> bool {
    if is_spun_looking_descriptor_name(name) {
        return false;
    }
    if let Some(elem) = name.strip_prefix('[') {
        let elem = elem.trim_start_matches('[');
        match elem.as_bytes() {
            [c] => b"IJFDZBCS".contains(c),
            _ => elem.starts_with("Ljava/") && elem.ends_with(';'),
        }
    } else {
        name.starts_with("java/")
    }
}

/// The components of the method descriptor `desc`, parameters then the
/// return, as [`parse_descriptor_types`] / [`descriptor_to_class_name`] spell
/// them; `None` when `desc` does not parse.
pub(crate) fn bound_type_components(desc: &str) -> Option<Vec<String>> {
    if !desc.starts_with('(') {
        return None;
    }
    let close = desc_close_paren(desc)?;
    let mut names: Vec<String> = parse_descriptor_types(&desc[1..close])
        .iter()
        .map(|n| n.to_string())
        .collect();
    names.push(descriptor_to_class_name(&desc[close + 1..]).to_string());
    Some(names)
}

/// `CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES` -- default OFF (JIT round 16
/// wave 13, lane wmte; proposal MH16-1 of `jit-r16-mh-proposals-RETIRED-20261006.md`, a
/// performance switch): the bound `MethodType` table
/// ([`bound_method_type_through_class_key`]) also shares a type naming a class that a
/// BUILT-IN loader (bootstrap, platform or application) is known to have
/// defined ([`bound_type_component_builtin_loaded`]), so `bindTo` on a target
/// whose type names an application class (`(LApp;)V`, the common user shape)
/// stops building a fresh type per bind. `1`/`true`/`on`/`yes` turns it on.
/// Read on a descriptor-row miss.
///
/// Round 16 wave 15 (lane mhapp): such a row is keyed by the descriptor AND
/// the `ClassId` of every reference component
/// ([`bound_method_type_through_class_key`]), so a same-named class of
/// another loader is another row, and the table answers exactly what a fresh
/// build answers. Read once (a test's thread override is honoured).
pub(crate) fn mh_bound_type_table_app_classes_enabled() -> bool {
    const KEY: &str = "CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES";
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if cratonvm_types::flags::overrides_active() {
        return cratonvm_types::flags::runtime_flag_default_on(KEY);
    }
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(KEY))
}

/// A reference component `name` (an object class `a/B`, or an array of one,
/// `[La/B;`) outside `java/` that MH16-1 may freeze into a class-keyed row
/// ([`bound_type_class_key_admissible`]): it is loaded, not a stand-in, and
/// its element class's defining loader is KNOWN to be a built-in one
/// (`NativeClassAccess::loader_id_of_class_definite`: 0, 1 or 2), which never
/// unloads. The row is rooted outright (the `bound-method-types` native-roots
/// row), so a row naming a USER loader's class would keep that loader alive
/// for the VM's life: that, not the key, is why user loaders stay out.
pub(crate) fn bound_type_component_builtin_loaded(ctx: &dyn NativeContext, name: &str) -> bool {
    let element = match name.strip_prefix('[') {
        Some(elem) => {
            let elem = elem.trim_start_matches('[');
            match elem.strip_prefix('L').and_then(|e| e.strip_suffix(';')) {
                Some(class) => class,
                None => return false,
            }
        }
        None => name,
    };
    if element.is_empty() || element.contains(';') || element.contains('[') {
        return false;
    }
    let (Some(_), Some(element_id)) = (ctx.class_id_by_name(name), ctx.class_id_by_name(element))
    else {
        return false;
    };
    !ctx.is_class_synthetic_stub(name)
        && !ctx.is_class_synthetic_stub(element)
        && matches!(ctx.loader_id_of_class_definite(element_id), Some(0..=2))
}

/// The bound handle's `MethodType` for `desc`: the VM's shared row when the
/// table is on ([`mh_bound_type_table_enabled`]) and holds one; else a fresh
/// [`build_method_type_from_descriptor`], offered to the table when
/// [`bound_method_type_shareable`] (the answer is the table's object, which is
/// the fresh one unless another thread stored first or the table is full).
/// Same `Ok(None)` / `Err` contract as the builder. A hit allocates nothing,
/// so it is not a GC point.
///
/// `src` is the target handle and its kind (current: read by the caller with
/// no GC point since). Its names are resolved through the target's own loader
/// ([`bound_type_resolution_near`]; round 16 wave 17, lane mhapp), so a user
/// loader's `Box` is never the application loader's.
pub(crate) fn bound_method_type_from_descriptor(
    ctx: &mut dyn NativeContext,
    desc: &str,
    src: Option<(ObjectRef, i32)>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if !mh_bound_type_table_enabled() {
        let near = src.and_then(|(h, kind)| bound_type_resolution_near(&*ctx, h, kind));
        return build_method_type_from_descriptor_near(ctx, desc, near);
    }
    bound_method_type_through_table(ctx, desc, src)
}

/// The table half of [`bound_method_type_from_descriptor`] (no switch read).
pub(crate) fn bound_method_type_through_table(
    ctx: &mut dyn NativeContext,
    desc: &str,
    src: Option<(ObjectRef, i32)>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // A descriptor-only row names only `java/...` classes and primitives,
    // which every loader resolves to the same classes.
    if let Some(mt) = ctx.bound_method_type_get(desc) {
        return Ok(Some(mt));
    }
    // Asked only past the descriptor-only hit (no allocation, no GC point).
    let near = src.and_then(|(h, kind)| bound_type_resolution_near(&*ctx, h, kind));
    bound_method_type_table_miss(ctx, desc, near)
}

/// [`bound_method_type_from_descriptor`]'s answer for a caller that knows the
/// resolution class `near` itself (wave 71, a finder's field accessor mint,
/// `CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS`): the table's row when the table
/// is on, else a fresh build. Same `Ok(None)` / `Err` contract as the builder.
pub(crate) fn method_type_through_table_near(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if !mh_bound_type_table_enabled() {
        return build_method_type_from_descriptor_near(ctx, desc, near);
    }
    if let Some(mt) = ctx.bound_method_type_get(desc) {
        return Ok(Some(mt));
    }
    bound_method_type_table_miss(ctx, desc, near)
}

/// The table half past a descriptor-only miss, resolving through `near`.
pub(crate) fn bound_method_type_table_miss(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Round 16 wave 15 (mhapp): a type naming a class outside `java/` is a
    // class-keyed row or none, never a descriptor-only one.
    if mh_bound_type_table_app_classes_enabled() && !bound_type_descriptor_java_named(desc) {
        return bound_method_type_through_class_key(ctx, desc, near);
    }
    // Decided BEFORE the build: a class this finds loaded is the class the
    // build resolves (see `bound_method_type_shareable`).
    let shareable = bound_method_type_shareable(&*ctx, desc);
    match build_method_type_from_descriptor_near(ctx, desc, near)? {
        // No GC point between the build's return and the store: `mt` is current.
        Some(mt) if shareable => Ok(Some(ctx.bound_method_type_put(desc, mt))),
        other => Ok(other),
    }
}

/// Does every component of `desc` pass [`bound_type_component_java_named`]
/// (or name a primitive)? By name only; `false` when `desc` does not parse.
pub(crate) fn bound_type_descriptor_java_named(desc: &str) -> bool {
    bound_type_components(desc).is_some_and(|names| {
        names
            .iter()
            .all(|n| is_primitive_descriptor_name(n) || bound_type_component_java_named(n))
    })
}

/// The `ClassId` a build would resolve for each reference component of
/// `desc`, parameters then the return, primitives skipped: the lookup
/// [`mirror_for_descriptor_name_resolving`] makes (through `near`'s loader
/// first, then `class_id_by_name`, then `class_id_by_name_delegated`),
/// without its on-demand array load. `None` when `desc` does not parse, names
/// a spun-looking class, or names one the lookup does not find (the build
/// would mint a stand-in, or load). Reads only; no heap allocation.
pub(crate) fn bound_type_class_key_ids(
    ctx: &dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Option<Vec<u32>> {
    let names = bound_type_components(desc)?;
    let mut ids = Vec::with_capacity(names.len());
    for name in &names {
        if is_primitive_descriptor_name(name) {
            continue;
        }
        if is_spun_looking_descriptor_name(name) {
            return None;
        }
        ids.push(descriptor_component_class_id(ctx, name, near)?.as_u32());
    }
    Some(ids)
}

/// The class a descriptor component `name` names, as the builder resolves it
/// ([`mirror_for_descriptor_name_resolving`] minus the array load): through
/// `near`'s defining loader first (`class_id_by_name_near`), then the
/// loader-blind `class_id_by_name` / `class_id_by_name_delegated`.
pub(crate) fn descriptor_component_class_id(
    ctx: &dyn NativeContext,
    name: &str,
    near: Option<ClassId>,
) -> Option<ClassId> {
    near.and_then(|n| mh_caller_sees(&*ctx, name, n))
        .or_else(|| ctx.class_id_by_name(name))
        .or_else(|| ctx.class_id_by_name_delegated(name))
}

/// May the type `desc`, whose reference components resolved to `ids`
/// ([`bound_type_class_key_ids`]), be a class-keyed row? Every component is
/// a real class (no stand-in) of a built-in loader: a `java/...` name, or a
/// class (an array's element class) whose defining loader, read off its OWN
/// `ClassId` and never re-resolved by name, is KNOWN to be built-in
/// (`loader_id_of_class_definite`: 0, 1 or 2). Asked on a miss only.
pub(crate) fn bound_type_class_key_admissible(
    ctx: &dyn NativeContext,
    desc: &str,
    ids: &[u32],
    near: Option<ClassId>,
) -> bool {
    let Some(names) = bound_type_components(desc) else {
        return false;
    };
    let mut ids = ids.iter();
    for name in names.iter().filter(|n| !is_primitive_descriptor_name(n)) {
        let Some(&id) = ids.next() else {
            return false;
        };
        if ctx.is_class_synthetic_stub(name) {
            return false;
        }
        if bound_type_component_java_named(name) {
            continue;
        }
        let element_id = match name.strip_prefix('[') {
            None => Some(ClassId::new(id)),
            Some(elem) => elem
                .trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|e| e.strip_suffix(';'))
                .and_then(|e| descriptor_component_class_id(ctx, e, near)),
        };
        let builtin =
            element_id.is_some_and(|e| matches!(ctx.loader_id_of_class_definite(e), Some(0..=2)));
        if !builtin {
            return false;
        }
    }
    ids.next().is_none()
}

/// `CRATONVM_MH_LOADER_SCOPED_TYPES` -- default ON (JIT round 16 wave 17,
/// lane mhapp; a correctness fix, both modes): a handle's `type` is resolved
/// through the loader of the class it was looked up on (`findVirtual` /
/// `findStatic` / `findSpecial` / `unreflect*`'s `refc`), and `bindTo`'s bound
/// type through the target's ([`bound_type_resolution_near`]), as HotSpot's
/// `MethodType`s hold the target's own mirrors. A same-named class of a user
/// loader was resolved by name to the application loader's, so with
/// descriptor-built types interned (`CRATONVM_MH_INTERN_DESCRIPTOR_TYPES`)
/// both loaders' bound types were ONE object (`R16MhappIntern`:
/// `user-ptype=false same-row=true`). `0`/`false`/`off`/`no` restores the
/// loader-blind lookup. Read once (a test's thread override is honoured).
pub(crate) fn mh_loader_scoped_types_enabled() -> bool {
    const KEY: &str = "CRATONVM_MH_LOADER_SCOPED_TYPES";
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if cratonvm_types::flags::overrides_active() {
        return cratonvm_types::flags::runtime_flag_default_on(KEY);
    }
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(KEY))
}

/// The class whose defining loader resolves the names of the target handle
/// `src`'s bound type (`kind` is its `MH_KIND`), or `None` (the loader-blind
/// lookup): a bound VIRTUAL / SPECIAL handle's bound receiver's class; an
/// unbound one's receiver parameter (`type.ptypes[0]`, the class it was looked
/// up on); a STATIC handle's recorded owner. Reads only; no allocation.
pub(crate) fn bound_type_resolution_near(
    ctx: &dyn NativeContext,
    src: ObjectRef,
    kind: i32,
) -> Option<ClassId> {
    if !mh_loader_scoped_types_enabled() {
        return None;
    }
    match kind {
        MH_KIND_STATIC => mh_static_owner(ctx.get_field(src, MH_BOUND)),
        MH_KIND_VIRTUAL | MH_KIND_SPECIAL => match ctx.get_field(src, MH_BOUND) {
            Value::Object(Some(recv)) => Some(ctx.class_id_of_object(recv)),
            _ => {
                let Value::Object(Some(mt)) = ctx.get_field_by_name(src, "type") else {
                    return None;
                };
                let Value::Object(Some(ptypes)) = ctx.get_field(mt, 1) else {
                    return None;
                };
                if ctx.array_length(ptypes) == 0 {
                    return None;
                }
                match ctx.get_array_element(ptypes, 0) {
                    Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
                    _ => None,
                }
            }
        },
        _ => None,
    }
}

/// MH16-1 made sound (JIT round 16 wave 15, lane mhapp): the bound type
/// `desc`, which names a class outside `java/`, from the row keyed by `desc`
/// plus the `ClassId` of every reference component
/// (`method_type_table::class_keyed`). Another loader's class of the same
/// name resolves to another `ClassId`, so it is another row; a name the
/// lookup cannot pin to one class is no row at all (a fresh build). The row
/// is stored only when the build resolved exactly the ids the key holds (a
/// class defined between the two gets a plain fresh answer), so a hit is
/// always what a fresh build would answer now. A row naming a class that
/// unloads is dropped by the VM's unload transaction
/// (`MethodTypeTable::forget_classes`).
pub(crate) fn bound_method_type_through_class_key(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(ids) = bound_type_class_key_ids(&*ctx, desc, near) else {
        return build_method_type_from_descriptor_near(ctx, desc, near);
    };
    let key = cratonvm_native_api::method_type_table::class_keyed(desc, &ids);
    if let Some(mt) = ctx.bound_method_type_get(&key) {
        return Ok(Some(mt));
    }
    let admissible = bound_type_class_key_admissible(&*ctx, desc, &ids, near);
    match build_method_type_from_descriptor_resolving(ctx, desc, near)? {
        // No GC point between the build's return and the store: `mt` is current.
        Some((mt, Some(built))) if admissible && built == ids => {
            Ok(Some(ctx.bound_method_type_put(&key, mt)))
        }
        Some((mt, _)) => Ok(Some(mt)),
        None => Ok(None),
    }
}

/// `bindTo`'s ordinary mint without [`alloc_method_handle`]'s per-call strings
/// (round 15 wave 4, lane mh3; MH2-3, [`mh_bindto_shared_mint_enabled`]): a
/// fresh carrier holding `src`'s own `MH_CLASS` / `MH_NAME` / `MH_DESC` string
/// objects (Java strings are immutable, and every writer of those slots stores
/// a new object rather than editing one), `kind`, nothing bound, settled or
/// chained, the variable-arity bit `collects`, and a `type` built once from
/// `type_desc` (`()V` when that does not parse, as the ordinary mint's C21 rule
/// has it). `src_pin` roots `src` (the caller's pin); the carrier is pinned
/// across the `MethodType` build and released before it returns.
///
/// `link_slots` (interpreter round i2 wave 65, lane L4;
/// `CRATONVM_MH_BOUND_FIELD_OWNER`): allocate [`alloc_method_handle`]'s width,
/// the field-accessor link slots unlinked, so a bound field handle can keep
/// its minted slot ([`BoundFieldCarry`]). `false` is the narrow carrier, as
/// before.
pub(crate) fn alloc_bound_method_handle(
    ctx: &mut dyn NativeContext,
    src_pin: usize,
    src: ObjectRef,
    kind: i32,
    type_desc: &str,
    collects: bool,
    link_slots: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let width = if link_slots {
        MH_SLOT_COUNT
    } else {
        MH_STATIC_OWNER + 1
    };
    let mh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", width)?;
    let src = ctx.read_native_pin(src_pin, src);
    for slot in [MH_CLASS, MH_NAME, MH_DESC] {
        let shared = ctx.get_field(src, slot);
        ctx.set_field(mh, slot, shared);
    }
    ctx.set_field(mh, MH_KIND, Value::Int(kind));
    ctx.set_field(mh, MH_BOUND, Value::Object(None));
    ctx.set_field(mh, MH_STATIC_SETTLED, Value::Int(0));
    ctx.set_field(mh, MH_SPECIAL_CALLER, Value::Object(None));
    ctx.set_field(mh, MH_CAST_CHAIN, Value::Object(None));
    ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(0));
    if link_slots {
        // Not linked: `alloc_method_handle`'s writes, for the same reason.
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(None));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(0));
    }
    // Through the one writer (`the_marking_bits_have_one_writer`; this store
    // arrived with the JIT round 15 merge): a fresh carrier's only bit.
    mh_write_marking_bits(&*ctx, mh, i32::from(collects));
    let mh_pin = ctx.pin_native_root(mh);
    // `src` is current: nothing has collected since its re-read above.
    let mt = match bound_method_type_from_descriptor(ctx, type_desc, Some((src, kind))) {
        Ok(None) => build_method_type_from_descriptor(ctx, "()V"),
        other => other,
    };
    let mh = ctx.read_native_pin(mh_pin, mh);
    if let Ok(Some(mt)) = mt {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    ctx.unpin_native_roots(mh_pin);
    Ok(mh)
}

// ---------------------------------------------------------------------------
// The `bindTo` fast mint (JIT round 16 wave 11, lane mhbind; proposal MHF-2 of
// `jit-r16-mhfloor-proposals-RETIRED-20261006.md`)
// ---------------------------------------------------------------------------
//
// `R16MhFloor` `bind-only` (`LEN.bindTo(box)`, Linux 3360 ns against HotSpot's
// 149) pays per bind, besides the allocation: the three string decodes, the
// leading-parameter guard's `type` render (a mirror-to-name per parameter), the
// cast judgement, the variable-arity question and the bound type's text. For an
// ordinary mint of an unbound direct handle (`STATIC` / `VIRTUAL` / `SPECIAL`,
// no cast chain, the shared mint) every one of those answers depends only on
// the source handle's slots and on the bound value's class. A per-thread memo
// keyed by the source's address within one collection epoch (the rule of
// `DIRECT_LANE_MEMO` and `TYPE_RENDER_MEMO`: within one epoch nothing moves or
// is freed, so an equal address is the same object) plus the words of every
// slot the slow path decided on hands those answers over, and the bind is the
// mint (`alloc_bound_method_handle`) and the `MH_BOUND` store. A bound value
// whose class the cast judged by a positive reading (`Some(true)`: the class
// itself, a loaded subclass) is remembered by its class id; a leading `Object`
// or array parameter, which the cast never judges, admits any non-null
// non-array value. Anything else (a null, an array, another class, a slot
// rewritten in place, a collection since) takes the slow path, which re-fills
// the line.

/// `CRATONVM_MH_BINDTO_FAST_MINT` -- default ON, kill switch (JIT round 16 wave
/// 11, lane mhbind; MHF-2; on by default since the w11d A/B, `R16MhFloor`
/// `bind-only` 2435 -> 1372 ns): `bindTo` of a source handle it has already
/// bound in this collection epoch takes [`bindto_fast_mint_hit`] (the memoised
/// answers) instead of re-deriving them. `0`/`false`/`off`/`no` turns it off
/// (`runtime_flag_default_on`; INVENTORY `mh-bindto-fast-mint`,
/// `off_word: Some("0")`), so an A/B of a `bindTo` row sets `=0` for the off
/// arm -- setting `=1` measures nothing. Both modes; performance only (every
/// answer, refusal and message is the slow path's). For a FIELD kind the fast
/// mint fills a line only under `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`.
///
/// Read once, into a process-wide `OnceLock` over the environment rather than
/// a per-VM `NativeFlags` field (two VMs in one process cannot disagree about
/// it). It gates a performance path, not compatibility state, so it is not one
/// of the AGENTS.md process-global violations; moving it to `crate::nbflags()`
/// is a refactor left for a wave that can re-freeze
/// `per_vm_state_statics_ratchet` (interpreter round i3 wave 79, lane L4: the
/// comment said "default OFF, opt-in", page
/// `i66-L4-bindto-fast-mint-doc-says-opt-in-but-it-is-default-on`).
pub(crate) fn mh_bindto_fast_mint_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_FAST_MINT")
    })
}

/// The source slots a fast-mint line is keyed on, after the source's address:
/// every slot `bindTo`'s ordinary mint reads (`MH_CLASS` / `MH_NAME` /
/// `MH_DESC` decoded and shared, `MH_KIND`, `MH_BOUND` for "already bound" and
/// the static owner, `MH_VARARGS` for the marking, `MH_SPECIAL_CALLER`,
/// `MH_CAST_CHAIN` for the chain); `type` (by name), which the guard renders,
/// follows them in the key. Any of them rewritten in place is a miss.
pub(crate) const BINDTO_MINT_GUARD_SLOTS: [usize; 8] = [
    MH_CLASS,
    MH_NAME,
    MH_DESC,
    MH_KIND,
    MH_BOUND,
    MH_VARARGS,
    MH_SPECIAL_CALLER,
    MH_CAST_CHAIN,
];

/// Lines in [`BINDTO_MINT_MEMO`]; a power of two (the index is a mask).
pub(crate) const BINDTO_MINT_MEMO_SLOTS: usize = 16;

/// The words a fast-mint line is keyed on, read off the source on entry to
/// `bindTo` ([`bindto_fast_mint_probe`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BindToMintKey {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) src: usize,
    /// [`BINDTO_MINT_GUARD_SLOTS`] then `type`, as [`bindto_mint_word`]s.
    pub(crate) guard: [u64; 9],
}

/// What the slow path answered for a [`BindToMintKey`].
#[derive(Clone, Debug)]
pub(crate) struct BindToMintAnswer {
    pub(crate) kind: i32,
    /// The bound handle's `type` descriptor (`bindto_bound_type_descriptor_with`).
    pub(crate) type_desc: std::rc::Rc<str>,
    /// The variable-arity bit `alloc_bound_method_handle` writes.
    pub(crate) collects: bool,
    /// The recorded owner a bound `STATIC` handle keeps (`MH_STATIC_OWNER`).
    pub(crate) static_owner: Option<u32>,
    /// The bound value's class the cast admitted on a positive reading, or
    /// `None`: the leading parameter is one the cast never judges.
    pub(crate) recv_class: Option<u32>,
    /// What the slow path carried to a bound instance field copy (wave 65's
    /// [`BoundFieldCarry`]; a line holds one under
    /// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`, wave 66). A hit re-reads it
    /// off the source ([`bindto_fast_mint_refresh_carry`]).
    pub(crate) field_carry: Option<BoundFieldCarry>,
    /// The bound field link the slow path's pre-link wrote on its copy
    /// (wave 68, `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`), replayed by a hit
    /// ([`bindto_fast_mint_replay_prelink`]); `None` when it wrote none.
    pub(crate) prelink: Option<BoundPrelinkMemo>,
    /// A second admitted class of the same line and its pre-link (wave 69,
    /// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`; [`bindto_mint_answer_merged`]),
    /// or `None`. Only ever set beside a `Some` `recv_class`.
    pub(crate) alt: Option<BindToMintAlt>,
}

/// The receiver-dependent words of a second class a fast-mint line admits
/// (interpreter round i2 wave 69, lane L4; proposal
/// `i68-L4-proposal-fast-mint-line-keeps-a-second-receiver-class-20261101`).
/// Every other word of a [`BindToMintAnswer`] depends on the line's key alone
/// (the source's slots in one collection epoch), so a line filled for class A
/// and then for class B of the same key differs only in these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BindToMintAlt {
    /// The class the slow path's cast admitted on a positive reading.
    pub(crate) recv_class: u32,
    /// The pre-link remembered for a receiver of that class.
    pub(crate) prelink: Option<BoundPrelinkMemo>,
}

/// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES` (default OFF, opt-in,
/// performance; interpreter round i2 wave 69, lane L4): a fast-mint line
/// refilled for the same key with a second admitted class keeps the first as
/// [`BindToMintAnswer::alt`] instead of losing it, so a source bound
/// alternately to receivers of two classes hits every time.
pub(crate) fn mh_bindto_fast_mint_two_classes_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_two_classes
}

/// Do two answers for the same key agree on every receiver-independent word
/// (kind, bound type, marking, static owner, the carry's owner)? They always
/// should; a disagreement keeps only the newer answer. Pure.
pub(crate) fn bindto_mint_answers_share_shape(a: &BindToMintAnswer, b: &BindToMintAnswer) -> bool {
    a.kind == b.kind
        && *a.type_desc == *b.type_desc
        && a.collects == b.collects
        && a.static_owner == b.static_owner
        && a.field_carry.map(|c| c.owner) == b.field_carry.map(|c| c.owner)
}

/// The answer a fill writes when the line already holds `old` for the same
/// key (wave 69, `on` = `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`): `new`,
/// keeping `old`'s class (and its pre-link) as the second class when both
/// name an admitted class, the classes differ, and the shapes agree. A third
/// class therefore replaces the older of the two. Pure.
pub(crate) fn bindto_mint_answer_merged(
    old: Option<&BindToMintAnswer>,
    new: BindToMintAnswer,
    on: bool,
) -> BindToMintAnswer {
    if !on {
        return new;
    }
    let (Some(old), Some(new_class)) = (old, new.recv_class) else {
        return new;
    };
    let Some(old_class) = old.recv_class else {
        return new;
    };
    if old_class == new_class || !bindto_mint_answers_share_shape(old, &new) {
        return new;
    }
    let alt = Some(BindToMintAlt {
        recv_class: old_class,
        prelink: old.prelink,
    });
    BindToMintAnswer { alt, ..new }
}

/// The answer a line's `answer` gives a bound value of exact class `cid`, or
/// `None` (the slow path): any class when the line judges none, the line's
/// class, or (wave 69) its second class -- handed out with that class's
/// pre-link in place of the first's. Pure.
pub(crate) fn bindto_mint_answer_for_class(answer: BindToMintAnswer, cid: u32) -> Option<BindToMintAnswer> {
    match answer.recv_class {
        None => Some(answer),
        Some(c) if c == cid => Some(answer),
        Some(_) => match answer.alt {
            Some(alt) if alt.recv_class == cid => Some(BindToMintAnswer {
                recv_class: Some(alt.recv_class),
                prelink: alt.prelink,
                alt: None,
                ..answer
            }),
            _ => None,
        },
    }
}

/// The receiver-independent words of a bound field link `bindTo`'s pre-link
/// wrote on a fresh copy, plus the one receiver-dependent word, its bound
/// receiver's exact class (interpreter round i2 wave 68, lane L4;
/// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`; proposal
/// `i67-L4-proposal-fast-mint-line-carries-the-pre-link-20261031`). Every
/// other input of that link -- the kind, the shared `MH_NAME` / `MH_DESC`,
/// the bound `type`'s descriptor, the recorded owner, the minted slot, no
/// chain, no marking -- is fixed by the fast-mint line's key and its
/// [`BoundFieldCarry`], so a copy minted from the same line for a receiver of
/// the same class gets the same link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundPrelinkMemo {
    /// The carry's `link_state` the link was derived from: a fresh copy's
    /// `old` state, hence its minted bit and relink count.
    pub(crate) carry_state: i32,
    /// The packed [`MH_FIELD_LINK`] state written (`FL_LINKED | FL_BOUND`,
    /// never `FL_VALUE_CHECK`, `FL_STATIC` or `FL_OWNER_ADMITS`).
    pub(crate) state: i32,
    /// [`MH_FIELD_LINK_CLASS`]: the bound receiver's exact class.
    pub(crate) class_raw: i32,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type.
    pub(crate) site_id: i32,
}

/// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK` (default OFF, opt-in, performance;
/// interpreter round i2 wave 68, lane L4): see [`BoundPrelinkMemo`]. Needs
/// `CRATONVM_MH_BINDTO_PRELINK_FIELD` (the link it replays is that switch's).
pub(crate) fn mh_bindto_fast_mint_prelink_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_prelink
}

/// What a fast-mint line may remember of the link the slow path's pre-link
/// just wrote on its fresh bound copy `mh` (current), derived from `carry`,
/// when `on`: a bound, unchecked instance link whose `type` is the copy's
/// own. `None` otherwise (nothing linked, or a shape a hit must re-derive).
/// Reads only.
pub(crate) fn bindto_prelink_memo_of_with(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    carry: BoundFieldCarry,
    on: bool,
) -> Option<BoundPrelinkMemo> {
    if !on || ctx.object_num_fields(mh) < MH_SLOT_COUNT {
        return None;
    }
    let Value::Int(state) = ctx.get_field(mh, MH_FIELD_LINK) else {
        return None;
    };
    if state & FL_LINKED == 0
        || state & FL_BOUND == 0
        || state & (FL_VALUE_CHECK | FL_STATIC | FL_OWNER_ADMITS) != 0
    {
        return None;
    }
    let type_idx = ((state >> 16) & FL_TYPE_INDEX_MASK) as usize;
    match (
        ctx.get_field(mh, type_idx),
        ctx.get_field(mh, MH_FIELD_LINK_TYPE),
    ) {
        (Value::Object(Some(now)), Value::Object(Some(linked))) if now == linked => {}
        _ => return None,
    }
    let (Value::Int(class_raw), Value::Int(site_id)) = (
        ctx.get_field(mh, MH_FIELD_LINK_CLASS),
        ctx.get_field(mh, MH_FIELD_LINK_SITE),
    ) else {
        return None;
    };
    if class_raw == 0 {
        return None;
    }
    Some(BoundPrelinkMemo {
        carry_state: carry.link_state,
        state,
        class_raw,
        site_id,
    })
}

/// [`bindto_prelink_memo_of_with`] under the switch.
pub(crate) fn bindto_prelink_memo_of(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    carry: BoundFieldCarry,
) -> Option<BoundPrelinkMemo> {
    bindto_prelink_memo_of_with(ctx, mh, carry, mh_bindto_fast_mint_prelink_enabled())
}

/// A fast-mint hit's pre-link (wave 68): write `memo`'s link on the fresh
/// bound copy `mh` (current; `type`, `MH_BOUND` = `recv` and the carry
/// already written) when `on` and every input the slow path derived it from
/// still holds -- the pre-link's own switches ([`field_link_candidate`]'s
/// checks on the copy), the copy's state exactly the carry the link was
/// derived from, no cast chain, and a bound receiver of EXACTLY
/// `memo.class_raw`. `true` when written; `false` sends the caller to
/// [`mh_bindto_prelink_bound_field`], which derives it (or declines) as the
/// slow path does. `on` is the three switches together (this one,
/// `CRATONVM_MH_BINDTO_PRELINK_FIELD`, `CRATONVM_MH_FIELD_LINK_BOUND`). Under `CRATONVM_DBG_MH_FIELD_LINK` it declines, so the
/// positive control still prints its line. Performs no allocation and no
/// Java call; cannot throw.
pub(crate) fn bindto_fast_mint_replay_prelink_with(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    recv: Value,
    carry: BoundFieldCarry,
    memo: Option<BoundPrelinkMemo>,
    on: bool,
) -> bool {
    let Some(memo) = memo else {
        return false;
    };
    if !on {
        return false;
    }
    if crate::nbflags().dbg_mh_field_link || carry.link_state != memo.carry_state {
        return false;
    }
    let Some(FieldLinkCandidate { old, .. }) = field_link_candidate(ctx, mh) else {
        return false;
    };
    if old != memo.carry_state {
        return false;
    }
    if matches!(ctx.get_field(mh, MH_CAST_CHAIN), Value::Object(Some(_))) {
        return false;
    }
    match (recv, ctx.get_field(mh, MH_BOUND)) {
        (Value::Object(Some(r)), Value::Object(Some(b))) if r == b => {
            if ctx.object_is_array(r) || ctx.class_id_of_object(r).as_u32() as i32 != memo.class_raw
            {
                return false;
            }
        }
        _ => return false,
    }
    let type_idx = ((memo.state >> 16) & FL_TYPE_INDEX_MASK) as usize;
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, type_idx) else {
        return false;
    };
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(memo.class_raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(memo.site_id));
    // Last: a reader never sees the state before the slots it describes.
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(memo.state));
    true
}

/// [`bindto_fast_mint_replay_prelink_with`] under the switch.
pub(crate) fn bindto_fast_mint_replay_prelink(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    recv: Value,
    carry: BoundFieldCarry,
    memo: Option<BoundPrelinkMemo>,
) -> bool {
    let on = mh_bindto_fast_mint_prelink_enabled()
        && mh_bindto_prelink_field_enabled()
        && mh_field_link_bound_enabled();
    bindto_fast_mint_replay_prelink_with(ctx, mh, recv, carry, memo, on)
}

/// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS` (default on since the wave-66 triage, performance; interpreter
/// round i2 wave 66, lane L4; proposal
/// `i65-L4-proposal-bindto-fast-mint-covers-field-handles`).
pub(crate) fn mh_bindto_fast_mint_fields_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_fields
}

/// Does `bindTo`'s ordinary mint of an unbound, chain-free source of `kind`
/// fill a fast-mint line? The direct kinds always; an instance field getter /
/// setter when `fields` (wave 66). A static field accessor never reaches the
/// fill (`bindTo` refuses a static getter and adapts a static setter). Pure.
pub(crate) fn bindto_fast_mint_fills_kind(kind: i32, fields: bool) -> bool {
    match kind {
        MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_SPECIAL => true,
        MH_KIND_GETTER | MH_KIND_SETTER => fields,
        _ => false,
    }
}

/// Bring a hit's field carry up to date with the source `src` (current): the
/// owner must still be the one the line remembers (else `false`, the slow
/// path), and the minted slot is re-read as the slow path reads it (a link
/// made since does not change it; a copy keeps the minted bits only). `true`
/// with no carry. Reads only.
pub(crate) fn bindto_fast_mint_refresh_carry(
    ctx: &dyn NativeContext,
    src: ObjectRef,
    answer: &mut BindToMintAnswer,
) -> bool {
    if answer.field_carry.is_none() {
        return true;
    }
    let owner = mh_instance_owner(ctx, src);
    bindto_fast_mint_refresh_carry_with(ctx, src, answer, owner)
}

/// [`bindto_fast_mint_refresh_carry`] with the source's recorded owner
/// (`owner`, its [`mh_instance_owner`]) as a parameter (the unit tests).
pub(crate) fn bindto_fast_mint_refresh_carry_with(
    ctx: &dyn NativeContext,
    src: ObjectRef,
    answer: &mut BindToMintAnswer,
    owner: Option<ClassId>,
) -> bool {
    let Some(memo) = answer.field_carry else {
        return true;
    };
    if owner != Some(memo.owner) {
        return false;
    }
    let link_state = if ctx.object_num_fields(src) >= MH_SLOT_COUNT {
        match ctx.get_field(src, MH_FIELD_LINK) {
            Value::Int(state) => field_link_state_kept_by_copy(state),
            _ => 0,
        }
    } else {
        0
    };
    answer.field_carry = Some(BoundFieldCarry {
        owner: memo.owner,
        link_state,
    });
    true
}

#[derive(Clone, Debug)]
pub(crate) struct BindToMintLine {
    pub(crate) key: BindToMintKey,
    pub(crate) answer: BindToMintAnswer,
}

thread_local! {
    /// Per-thread fast-mint lines (MHF-2), direct-mapped by the source's
    /// address. A context reporting VM identity `0` (the mocks) is never
    /// memoised outside the unit tests' own override.
    pub(crate) static BINDTO_MINT_MEMO: std::cell::RefCell<Vec<Option<BindToMintLine>>> =
        std::cell::RefCell::new(vec![None; BINDTO_MINT_MEMO_SLOTS]);
}

#[cfg(test)]
thread_local! {
    /// Unit tests only: the VM identity the fast mint uses with the switch
    /// taken as on, so the mock context (identity `0`) can exercise the memo.
    pub(crate) static BINDTO_MINT_TEST_VM: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Unit tests only: fast-mint hits on this thread.
    pub(crate) static BINDTO_MINT_TEST_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The VM identity the fast mint keys on, or `None` when it is off (the switch,
/// or a context with no identity).
pub(crate) fn bindto_fast_mint_vm(ctx: &dyn NativeContext) -> Option<usize> {
    #[cfg(test)]
    if let Some(vm) = BINDTO_MINT_TEST_VM.with(|v| v.get()) {
        return Some(vm);
    }
    if !mh_bindto_fast_mint_enabled() {
        return None;
    }
    match ctx.vm_identity() {
        0 => None,
        vm => Some(vm),
    }
}

/// A slot value as a key word (an object's address, an `Int` tagged so it can
/// never equal an address; the encoding of the JIT's constant-bind guard).
pub(crate) fn bindto_mint_word(v: Value) -> u64 {
    match v {
        Value::Object(Some(o)) => o.as_ptr() as u64,
        Value::Object(None) => 0,
        Value::Int(i) => (1u64 << 63) | u64::from(i as u32),
        _ => u64::MAX,
    }
}

/// The fast-mint key of `src` on entry to `bindTo`, or `None` when the fast
/// mint is off or `src` is narrower than the shared mint's layout. Reads only.
pub(crate) fn bindto_fast_mint_probe(ctx: &dyn NativeContext, src: ObjectRef) -> Option<BindToMintKey> {
    let vm = bindto_fast_mint_vm(ctx)?;
    if ctx.object_num_fields(src) <= MH_CAST_CHAIN {
        return None;
    }
    let mut guard = [0u64; 9];
    for (i, slot) in BINDTO_MINT_GUARD_SLOTS.iter().enumerate() {
        guard[i] = bindto_mint_word(ctx.get_field(src, *slot));
    }
    guard[8] = bindto_mint_word(ctx.get_field_by_name(src, "type"));
    Some(BindToMintKey {
        vm,
        epoch: ctx.gc_address_epoch(),
        src: src.as_ptr() as usize,
        guard,
    })
}

pub(crate) fn bindto_mint_line_index(src: usize) -> usize {
    (src >> 3) & (BINDTO_MINT_MEMO_SLOTS - 1)
}

/// The memoised answer for `key` and the bound value `recv`, or `None` (the
/// slow path). A hit needs the same key and a non-null, non-array `recv` the
/// line admits (any, or exactly the admitted class). Reads only.
pub(crate) fn bindto_fast_mint_hit(
    ctx: &dyn NativeContext,
    key: &BindToMintKey,
    recv: Value,
) -> Option<BindToMintAnswer> {
    let Value::Object(Some(r)) = recv else {
        return None;
    };
    let answer = BINDTO_MINT_MEMO.with(|memo| {
        let memo = memo.borrow();
        match &memo[bindto_mint_line_index(key.src)] {
            Some(line) if line.key == *key => Some(line.answer.clone()),
            _ => None,
        }
    })?;
    if ctx.object_is_array(r) {
        return None;
    }
    // The line's class, or (wave 69) its second class.
    let answer = if answer.recv_class.is_some() {
        bindto_mint_answer_for_class(answer, ctx.class_id_of_object(r).as_u32())?
    } else {
        answer
    };
    #[cfg(test)]
    BINDTO_MINT_TEST_HITS.with(|h| h.set(h.get() + 1));
    Some(answer)
}

/// What the bound value's cast verdict lets a line remember (`None`: nothing,
/// the line is not filled). `pdesc` is the leading parameter the guard judged
/// `recv` against and `verdict` [`reference_arg_admitted`]'s answer. Only
/// answers that stay true for every later value of the remembered kind: a
/// leading `Object` parameter (never judged, any value passes), and a
/// positive `Some(true)` for a non-null non-array value, remembered by its
/// exact class. An "I cannot tell" (`None`) about a class the hierarchy may yet
/// learn about is not remembered.
pub(crate) fn bindto_fast_mint_admits(
    ctx: &dyn NativeContext,
    recv: Value,
    pdesc: &str,
    verdict: Option<bool>,
) -> Option<Option<u32>> {
    let Value::Object(Some(r)) = recv else {
        return None;
    };
    if ctx.object_is_array(r) {
        return None;
    }
    // An array parameter is judged since wave 60 (lane L4): a non-array value
    // there is refused before a line is filled, so it remembers nothing.
    if pdesc == "Ljava/lang/Object;" {
        return Some(None);
    }
    if verdict == Some(true) {
        return Some(Some(ctx.class_id_of_object(r).as_u32()));
    }
    None
}

/// Remember `answer` for `key`, unless a collection ran since the key was read
/// (its words would name moved objects). Wave 69
/// (`CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`): a line already holding
/// another admitted class for the same key keeps it as the second class
/// ([`bindto_mint_answer_merged`]).
pub(crate) fn bindto_fast_mint_fill(ctx: &dyn NativeContext, key: BindToMintKey, answer: BindToMintAnswer) {
    if ctx.gc_address_epoch() != key.epoch {
        return;
    }
    let two = mh_bindto_fast_mint_two_classes_enabled();
    BINDTO_MINT_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let slot = &mut memo[bindto_mint_line_index(key.src)];
        let old = slot
            .as_ref()
            .filter(|line| line.key == key)
            .map(|line| &line.answer);
        let answer = bindto_mint_answer_merged(old, answer, two);
        *slot = Some(BindToMintLine { key, answer });
    });
}

/// The fast mint's bind: the ordinary mint's tail for the memoised answers,
/// step for step (`alloc_bound_method_handle`, the bound value re-read through
/// its pin, the pins released, `MH_BOUND`, the static owner). `this_pin` roots
/// `this` (and, pinned after it, `recv_pin` the bound value).
pub(crate) fn bindto_fast_mint_finish(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    recv_pin: usize,
    recv: Value,
    answer: &BindToMintAnswer,
) -> MethodCallResult {
    let new_mh = alloc_bound_method_handle(
        ctx,
        this_pin,
        this,
        answer.kind,
        &answer.type_desc,
        answer.collects,
        // Wave 66 (`CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`): a field line's
        // carry asks for the slow path's width (`None` for the direct kinds).
        bound_field_copy_needs_link_slots(answer.field_carry, mh_field_link_bound_enabled()),
    )?;
    let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(new_mh, MH_BOUND, recv);
    if let Some(owner) = answer.static_owner {
        if ctx.object_num_fields(new_mh) > MH_STATIC_OWNER && mh_bindto_static_owner_enabled() {
            ctx.set_field(new_mh, MH_STATIC_OWNER, Value::Int(owner as i32));
        }
    }
    // The slow path's carry write, after `MH_BOUND` (wave 66).
    if let Some(carry) = answer.field_carry {
        mh_write_bound_field_carry(&*ctx, new_mh, carry);
        // Wave 67 (`CRATONVM_MH_BINDTO_PRELINK_FIELD`): the slow path's
        // pre-link too. No GC point since `new_mh` was minted. Wave 68
        // (`CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`): the line's remembered
        // link, for a receiver of the remembered class; else derived.
        if !bindto_fast_mint_replay_prelink(ctx, new_mh, recv, carry, answer.prelink) {
            mh_bindto_prelink_bound_field(ctx, new_mh);
        }
    }
    Ok(Some(Value::Object(Some(new_mh))))
}

/// `raw` (the handle's `MH_DESC`) without the parameter `bindTo` captured,
/// for a BOUND `STATIC` handle (whose `MH_DESC` still names it; a virtual
/// one's never named its receiver), or `None` (round 15 wave 4, lane mh3;
/// under [`mh_bindto_fixed_arity_enabled`]). Reads only.
pub(crate) fn bound_static_raw_desc(ctx: &dyn NativeContext, mh: ObjectRef, raw: &str) -> Option<String> {
    if ctx.object_num_fields(mh) <= MH_BOUND
        || !matches!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_STATIC))
        || !matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
        || !mh_bindto_fixed_arity_enabled()
    {
        return None;
    }
    descriptor_without_first_param(raw)
}

/// [`bound_static_raw_desc`] for EVERY kind whose `MH_DESC` names the value
/// `bindTo` captured (round 15 wave 6, lane mh5; proposal MH3-6, through
/// MH3-3's [`mh_bound_role`]): a bound instance getter / setter, array
/// accessor, `arrayLength` or `identity` keeps its member's `MH_DESC`
/// (`([II)I` for `arrayElementGetter(int[].class)`) while its `type` drops the
/// capture (`(I)I`), so the raw-descriptor escapes of [`mh_astype_refusal_desc`]
/// and [`mh_explicit_cast_refusal`] judged a type ONE PARAMETER LONGER than
/// the handle's against the leaf and let it through:
/// `arrayElementGetter(int[].class).bindTo(a).asType((Object,int)int)`
/// "converted" where HotSpot's `(int)int` handle refuses the arity. `VIRTUAL` /
/// `SPECIAL` capture a receiver their `MH_DESC` never named, so they keep the
/// raw descriptor; `STATIC` is [`bound_static_raw_desc`]'s, under its own
/// switch. Reads only.
pub(crate) fn bound_capture_raw_desc(ctx: &dyn NativeContext, mh: ObjectRef, raw: &str) -> Option<String> {
    if let Some(desc) = bound_static_raw_desc(ctx, mh, raw) {
        return Some(desc);
    }
    if ctx.object_num_fields(mh) <= MH_BOUND
        || !matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
    {
        return None;
    }
    let Value::Int(kind) = ctx.get_field(mh, MH_KIND) else {
        return None;
    };
    if matches!(kind, MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_SPECIAL)
        || mh_bound_role(kind, raw) != Some(BoundRole::Capture)
        || !mh_astype_capture_raw_desc_enabled()
    {
        return None;
    }
    descriptor_without_first_param(raw)
}

/// `CRATONVM_MH_ASTYPE_CAPTURE_RAW_DESC` -- default ON (round 15 wave 6, lane
/// mh5; MH3-6). `0`/`false`/`off`/`no` makes [`bound_capture_raw_desc`] answer
/// only for a bound `STATIC` handle, as before. Both modes. Read only on a
/// refusal path, for a bound handle of a capturing non-direct kind.
pub(crate) fn mh_astype_capture_raw_desc_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_ASTYPE_CAPTURE_RAW_DESC")
}

/// The registered `MethodHandle.invoke` native past its refusals: the
/// `asType` adaptation and the dispatch lanes, in order (moved out unchanged
/// in JIT round 17 wave 3, lane mh4, so the caller can cast the result).
/// `args` are the call's arguments after the handle.
pub(crate) fn mh_invoke_registered_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
    call_site: Option<&str>,
) -> MethodCallResult {
    let this = match mh_real_invoke_adapt(ctx, this, call_site) {
        Ok(Some(adapted)) => adapted,
        Ok(None) => this,
        Err(e) => return Err(e),
    };
    if let Some(result) = mh_real_direct_dispatch(ctx, this, args, false) {
        return result;
    }
    if let Some(result) = mh_real_lambda_form_dispatch(ctx, this, args, call_site, false) {
        return result;
    }
    // Round 13 wave 10 (lane callcost4): a direct handle at its own
    // exact type takes the direct lane (`mh_direct_lane`).
    if mh_direct_lane_enabled() {
        if let Some(result) = mh_direct_lane(ctx, this, args, call_site, DirectLaneDoor::Invoke) {
            return result;
        }
    }
    // Round 16 (lane mh, MH16-2): a static handle at a converting
    // call site takes the converting lane (`mh_converting_lane`).
    if mh_converting_lane_enabled() {
        if let Some(result) = mh_converting_lane(ctx, this, args, call_site, DirectLaneDoor::Invoke)
        {
            return result;
        }
    }
    // The channel's text is shared (round 15 wave 4, MH15-2; interpreter
    // wave 49, lane L7): the door borrows it, no copy.
    mh_invoke_door(ctx, this, args, call_site, true)
}

/// `CRATONVM_MH_INTERFACE_NULL_RECEIVER_MESSAGE` -- correctness, default ON,
/// `=0` restores the message-less NPE (JIT round 18 wave 11, lane mhresid;
/// `r18-mh-interface-handle-null-receiver-npe-has-no-message-FIXED-20261006.md`):
/// see [`mh_interface_null_receiver_npe`]. Read only for a null first argument.
pub(crate) fn mh_interface_null_receiver_message_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INTERFACE_NULL_RECEIVER_MESSAGE")
}

/// The `NullPointerException` HotSpot raises for an `invokeExact` / `invoke`
/// of an unbound INTERFACE method handle (`findVirtual` of an interface
/// method: ref kind `REF_invokeInterface`) with a null receiver:
/// `Cannot invoke <MemberName> with null receiver`, where the member name is
/// `MemberName.toString()` -- the declaring class's binary name, `.`, the
/// method name, its `MethodType.toString()` (simple names) and
/// `/invokeInterface` (`DirectMethodHandle.Interface.checkReceiver`; measured
/// on HotSpot 25: `Cannot invoke Npe$Fn.apply(int)int/invokeInterface with
/// null receiver`). A VIRTUAL handle's null receiver has no message on
/// HotSpot (the door's message-less NPE stays).
///
/// Asked first by both registered natives, only for a null first argument
/// and a call site whose descriptor IS the handle's own type (so no
/// `WrongMethodTypeException` or `asType` conversion could answer first);
/// the site's access refusal (`mh_site_access_refusal`, the link's) keeps its
/// place ahead of it. `None` for anything else. Pure reads.
pub(crate) fn mh_interface_null_receiver_npe(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    args: &[Value],
    site: Option<&str>,
) -> Option<MethodCallFailed> {
    if !matches!(args.first(), Some(Value::Object(None))) {
        return None;
    }
    let site = site?;
    if !mh_interface_null_receiver_message_enabled() {
        return None;
    }
    let handle = mh_door_handle(ctx, handle);
    let (class, name, desc) = if mh_is_real_handle(&*ctx, handle) {
        let record = real_handle_record(ctx, handle)?;
        let target = record.direct.as_ref()?;
        if target.ref_kind != 9 {
            return None;
        }
        let own_type = format!("(L{};{}", target.class, target.desc.get(1..)?);
        if record.declared != own_type || own_type != site {
            return None;
        }
        (
            target.class.clone(),
            target.name.clone(),
            target.desc.clone(),
        )
    } else {
        if ctx.object_num_fields(handle) <= MH_STATIC_OWNER
            || !matches!(ctx.get_field(handle, MH_KIND), Value::Int(k) if k == MH_KIND_VIRTUAL)
            || !matches!(ctx.get_field(handle, MH_BOUND), Value::Object(None))
            || !matches!(
                ctx.get_field(handle, MH_SPECIAL_CALLER),
                Value::Object(None)
            )
        {
            return None;
        }
        let fields = [
            ctx.get_field(handle, MH_CLASS),
            ctx.get_field(handle, MH_NAME),
            ctx.get_field(handle, MH_DESC),
            ctx.get_field_by_name(handle, "type"),
        ];
        let record = mh_direct_lane_decode(ctx, fields, true, Some(site))?;
        if !record.declared_is_own_type {
            return None;
        }
        let cid = ctx.class_id_by_name(&record.class)?;
        if !ctx.is_interface_class(cid) {
            return None;
        }
        (record.class, record.name, record.desc)
    };
    if let Some(refusal) = mh_site_access_refusal(&*ctx, Some(site)) {
        return Some(refusal);
    }
    let shown = method_type_display_with(&desc, &|n| loaded_simple_name(&*ctx, n))?;
    Some(
        RuntimeError::NullPointerException {
            message: Some(format!(
                "Cannot invoke {}.{name}{shown}/invokeInterface with null receiver",
                class.class_text(&*ctx)
            )),
        }
        .into(),
    )
}

/// `MethodHandle.invokeWithArguments` on a REAL (non-carrier) handle `this`.
///
/// The JDK's body is `genericMethodType(n)`, `this.asType(generic)`, then the
/// spread invoker's spun `LambdaForm` entries (`spreadInvoker(0).invokeExact`)
/// -- seven door crossings and a class of spun forms run as cold bytecode,
/// 106-149 us per call against 25 us on the carrier (SBR4 mhcons). This runs
/// the same first two steps through the JDK (its own `genericMethodType` cache
/// and `asType` cache, so a collector collects, a boxing adapter boxes and an
/// arity mismatch is the JDK's `WrongMethodTypeException`) and then enters the
/// adapted handle's `vmentry` directly with the elements as the arguments: what
/// `adapted.invokeExact(e0, .., en-1)` at `(Object * n)Object` is.
///
/// The first attempt at this door (SBR4) died with `InternalError: CratonVM
/// cannot invoke a java.lang.invoke.DirectMethodHandle` because it fed a real
/// handle to `mh_dispatch_from_door`, the carrier's dispatch: the real-handle
/// lanes are entered from the `invoke` / `invokeExact` doors only. This calls
/// those lanes.
///
/// Anything the lanes decline (no `vmentry` yet, the `foreign_vmentry_first`
/// shapes) runs the JDK's own body (`MethodHandle`'s, invokespecial).
///
/// JIT round 20, lane dispatch (`R16WmteFramesLines`): when the JDK's `asType`
/// THROWS (an arity or conversion `WrongMethodTypeException`), the lanes decline
/// and the JDK's own `invokeWithArguments` body runs the same `asType` again, so
/// the throwable's trace carries HotSpot's `MethodHandle.invokeWithArguments:735`
/// frame (and, for the `List` overload, `list`: its `invokeWithArguments:760`
/// frame outside it) instead of `asType` directly under the caller. Correctness
/// (a Java-visible trace), unconditional; the extra `asType` runs only on the
/// failing path.
pub(crate) fn mh_real_invoke_with_arguments_door(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    array: Option<ObjectRef>,
    list: Option<(usize, ObjectRef)>,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = array.map(|a| ctx.pin_native_root(a));
    let mut type_threw = false;
    let answer =
        mh_real_invoke_with_arguments_lanes(ctx, this_pin, this, arr_pin, array, &mut type_threw);
    let answer = match answer {
        Some(answer) => answer,
        None if type_threw && list.is_some() => {
            let this = ctx.read_native_pin(this_pin, this);
            let (list_pin, list) = list.expect("tested above");
            let list = ctx.read_native_pin(list_pin, list);
            ctx.invoke_special_bytecode_only(
                MH_CLASS_FOR_IWA_BODY,
                "invokeWithArguments",
                "(Ljava/util/List;)Ljava/lang/Object;",
                &[Value::Object(Some(this)), Value::Object(Some(list))],
            )
        }
        // JIT round 21 lane depth (correctness, unconditional; `i94` row
        // `L4W84InvokeWithArgumentsJumbo string-varargs-300-ints`): the body
        // this native stands for is `MethodHandle`'s OWN, and it is reached by
        // `super.invokeWithArguments(arguments)` from
        // `MethodHandleImpl.AsVarargsCollector.invokeWithArguments` (its
        // `ArrayStoreException` fallback on the jumbo path, and its <= 127
        // path). A VIRTUAL re-dispatch from here selected that override again
        // for a varargs collector receiver -- override, `super`, this native,
        // override ... -- a `StackOverflowError` where HotSpot's
        // `genericMethodType(300)` raises `IllegalArgumentException: bad
        // parameter count 300`. Run `MethodHandle`'s body with invokespecial
        // semantics: for every other receiver that is the body virtual
        // dispatch selects anyway (no other JDK handle class overrides it).
        // Interpreter round i4 wave 95 lane L4 fixed the same row on dev
        // (invokespecial for a real varargs collector only, virtual otherwise);
        // the round's dev merge kept this one unconditional form, which also
        // covers the `List` fallback above.
        None => {
            let this = ctx.read_native_pin(this_pin, this);
            let arr = match (array, arr_pin) {
                (Some(a), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, a))),
                _ => Value::Object(None),
            };
            ctx.invoke_special_bytecode_only(
                MH_CLASS_FOR_IWA_BODY,
                "invokeWithArguments",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(this)), arr],
            )
        }
    };
    ctx.unpin_native_roots(this_pin);
    answer
}

/// `invokeWithArguments` on a REAL `DirectMethodHandle` of a static, virtual or interface
/// method whose every argument already is what `asType(genericMethodType(n))` would convert
/// it to without a failure or a widening: the exact wrapper for a primitive parameter, an
/// instance (proven by `reference_arg_admitted`) or `null` for a reference one, a non-null
/// proven receiver. The adapter the JDK would build for that is `unbox, call, box`: this
/// does the same in place (the member's own call, the result boxed against its return), with
/// none of the adapter's `LambdaForm` entries (3-5 us each here). Anything else (a wider
/// wrapper, a failing cast, a `null` into a primitive, a null receiver, an array parameter)
/// declines to the JDK's own `asType`, which raises HotSpot's exceptions.
pub(crate) fn mh_real_iwa_direct(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    arr_pin: Option<usize>,
    array: Option<ObjectRef>,
    n: usize,
) -> Option<MethodCallResult> {
    let this_now = ctx.read_native_pin(this_pin, this);
    let record = real_handle_record(ctx, this_now)?;
    let target = record.direct.as_ref()?;
    if !matches!(target.ref_kind, 5 | 6 | 9) {
        return None;
    }
    let lead = usize::from(target.ref_kind != 6);
    if n != target.params.len() + lead {
        return None;
    }
    // Interpreter round i5 wave 109, lane L4 (`L4W109IwaReceiverCast`): the
    // receiver is cast to the HANDLE's receiver type (`type().parameterType(0)`,
    // what `asType(genericMethodType(n))` converts to), which is `refc` or,
    // for a protected member found from a subclass, the lookup class
    // (`restrictReceiver`), not the member's declaring class
    // (`target.class`). Judged against the member's class, an instance of the
    // declaring class that is not of `refc` / the caller was called where
    // HotSpot raises `ClassCastException: Cannot cast A to B`.
    // Interpreter round i7 wave 144, lane L4: the count and the first
    // parameter read in place (`count_descriptor_params`,
    // `first_descriptor_param`), where `split_descriptor_params` allocated a
    // `Vec<String>` of every parameter on each served call. Same answers.
    let receiver_desc: &str = if lead == 1 {
        if count_descriptor_params(&record.declared) != n {
            return None;
        }
        first_descriptor_param(&record.declared)?
    } else {
        ""
    };
    let elements: Vec<Value> = match (array, arr_pin) {
        (Some(a), Some(p)) => {
            let a = ctx.read_native_pin(p, a);
            (0..n).map(|i| ctx.get_array_element(a, i)).collect()
        }
        _ => Vec::new(),
    };
    let mut raw: Vec<Value> = Vec::with_capacity(n);
    for (i, value) in elements.iter().enumerate() {
        if i < lead {
            let Value::Object(Some(_)) = value else {
                return None;
            };
            if reference_arg_admitted(ctx, *value, receiver_desc) != Some(true) {
                return None;
            }
            raw.push(*value);
            continue;
        }
        let param = target.params[i - lead].as_str();
        match param.as_bytes().first().copied()? {
            b'L' => {
                let Value::Object(object) = value else {
                    return None;
                };
                if object.is_some()
                    && param != "Ljava/lang/Object;"
                    && reference_arg_admitted(ctx, *value, param) != Some(true)
                {
                    return None;
                }
                raw.push(*value);
            }
            b'[' => return None,
            primitive => {
                let Value::Object(Some(object)) = value else {
                    return None;
                };
                let wrapper = match primitive {
                    b'I' => "java/lang/Integer",
                    b'J' => "java/lang/Long",
                    b'Z' => "java/lang/Boolean",
                    b'B' => "java/lang/Byte",
                    b'S' => "java/lang/Short",
                    b'C' => "java/lang/Character",
                    b'F' => "java/lang/Float",
                    b'D' => "java/lang/Double",
                    _ => return None,
                };
                let cid = ctx.class_id_of_object(*object);
                if ctx.class_name_arc_of_id(cid).as_deref() != Some(wrapper) {
                    return None;
                }
                raw.push(crate::lang_class::unbox_value(&*ctx, *object));
            }
        }
    }
    // Wave 144 (lane L4): borrowed from the `Rc` record, which this call holds
    // (the per-call `String` clone is gone).
    let declared: &str = &record.declared;
    let result = mh_real_direct_fast(ctx, this_now, &raw, Some(declared), false, false)?;
    Some(auto_box_return(ctx, result, declared))
}

/// One element of [`mh_real_iwa_direct`]'s argument array, as the raw argument
/// `asType(genericMethodType(n))` would hand `target` without a failure or a
/// widening (`lead` = 1 for the receiver of a virtual / interface member).
/// `None` declines (an array parameter, a wider wrapper, a failing cast, a
/// `null` into a primitive or receiver). Reads only; no GC point. The receiver
/// is judged against `receiver_desc`, the handle's own receiver type (as
/// [`mh_real_iwa_direct`] does since interpreter round i5 wave 109).
fn mh_real_iwa_direct_element(
    ctx: &mut dyn NativeContext,
    target: &RealDirectTarget,
    receiver_desc: &str,
    lead: usize,
    i: usize,
    value: Value,
) -> Option<Value> {
    if i < lead {
        let Value::Object(Some(_)) = value else {
            return None;
        };
        if reference_arg_admitted(ctx, value, receiver_desc) != Some(true) {
            return None;
        }
        return Some(value);
    }
    let param = target.params.get(i - lead)?.as_str();
    match param.as_bytes().first().copied()? {
        b'L' => {
            let Value::Object(object) = value else {
                return None;
            };
            if object.is_some()
                && param != "Ljava/lang/Object;"
                && reference_arg_admitted(ctx, value, param) != Some(true)
            {
                return None;
            }
            Some(value)
        }
        b'[' => None,
        primitive => {
            let Value::Object(Some(object)) = value else {
                return None;
            };
            let wrapper = match primitive {
                b'I' => "java/lang/Integer",
                b'J' => "java/lang/Long",
                b'Z' => "java/lang/Boolean",
                b'B' => "java/lang/Byte",
                b'S' => "java/lang/Short",
                b'C' => "java/lang/Character",
                b'F' => "java/lang/Float",
                b'D' => "java/lang/Double",
                _ => return None,
            };
            let cid = ctx.class_id_of_object(object);
            if ctx.class_name_arc_of_id(cid).as_deref() != Some(wrapper) {
                return None;
            }
            Some(crate::lang_class::unbox_value(&*ctx, object))
        }
    }
}

/// `CRATONVM_MH_IWA_VARARGS_DIRECT` -- performance, default ON, `=0` kill (JIT
/// round 21, lane native; page
/// `jr21-iwa-small-varargs-call-runs-the-collector-lambda-forms-FIXED-20261015.md`).
fn mh_iwa_varargs_direct_enabled() -> bool {
    crate::nbflags().mh_iwa_varargs_direct
}

/// `AsVarargsCollector.invokeWithArguments`' `super.invokeWithArguments(..)`
/// (its `<= 127` path) on a collector whose fixed-arity `target` is a REAL
/// `DirectMethodHandle` of a static, virtual or interface method with a
/// REFERENCE-component trailing array: the JDK body builds
/// `asType(genericMethodType(n))` of the collector -- `asFixedArity()
/// .asCollector(arrayType, n - lead)` cast to `Object`s -- and runs its spun
/// `LambdaForm` entries (the collector's array fill, a cast per argument, the
/// member call, the box): ~25 us per call here, HotSpot well under 1 us. This
/// does what that adapter does in place when every argument passes WITHOUT a
/// failure: the leading arguments by [`mh_real_iwa_direct_element`]'s rules,
/// each trailing argument `null` or an instance of the component (the
/// adapter's `Class.cast`), collected into a fresh array of `arrayType`, then
/// the member's own call and the result boxed against its return. Anything
/// else (a primitive or array component, a failing cast, fewer arguments than
/// the leading parameters, a `null` argument array, n > 127, a target that is
/// not a direct method handle) declines to the JDK's own `asType` path, which
/// raises HotSpot's exceptions.
fn mh_real_iwa_varargs_direct(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    arr_pin: Option<usize>,
    array: Option<ObjectRef>,
    n: usize,
) -> Option<MethodCallResult> {
    if !mh_iwa_varargs_direct_enabled() || n > 127 {
        return None;
    }
    let (Some(arr), Some(arr_pin)) = (array, arr_pin) else {
        return None;
    };
    let this_now = ctx.read_native_pin(this_pin, this);
    if !mh_is_real_varargs_collector(&*ctx, this_now) {
        return None;
    }
    let Value::Object(Some(fixed)) = ctx.get_field_by_name(this_now, "target") else {
        return None;
    };
    let Value::Object(Some(array_type)) = ctx.get_field_by_name(this_now, "arrayType") else {
        return None;
    };
    let record = real_handle_record(ctx, fixed)?;
    let target = record.direct.as_ref()?;
    if !matches!(target.ref_kind, 5 | 6 | 9) {
        return None;
    }
    // The trailing parameter is `arrayType` itself (`makeVarargsCollector`
    // retypes the target when it differs, and then it is no direct handle).
    let trailing = target.params.last()?.as_str();
    let component = trailing.strip_prefix("[L")?.strip_suffix(';')?;
    let comp_cid = mirror_class_id(&*ctx, array_type)
        .and_then(|arr_cid| ctx.array_component_class_id(arr_cid))?;
    if ctx.class_name_arc_of_id(comp_cid).as_deref() != Some(component) {
        return None;
    }
    let lead = usize::from(target.ref_kind != 6) + target.params.len() - 1;
    if n < lead {
        return None;
    }
    let receiver_desc: &str = if target.ref_kind != 6 {
        first_descriptor_param(&record.declared)?
    } else {
        ""
    };
    // Every check is a read, before the one allocation.
    let arr_now = ctx.read_native_pin(arr_pin, arr);
    let mut raw: Vec<Value> = Vec::with_capacity(lead + 1);
    for i in 0..n {
        let value = ctx.get_array_element(arr_now, i);
        if i < lead {
            raw.push(mh_real_iwa_direct_element(
                ctx,
                target,
                receiver_desc,
                usize::from(target.ref_kind != 6),
                i,
                value,
            )?);
            continue;
        }
        let Value::Object(object) = value else {
            return None;
        };
        if object.is_some()
            && component != "java/lang/Object"
            && reference_arg_admitted(ctx, value, trailing.get(1..)?) != Some(true)
        {
            return None;
        }
    }
    let collected = n - lead;
    let fresh = ctx.try_new_ref_array(comp_cid, collected)?;
    // The allocation is the one GC point: re-read every reference from its pin.
    let fresh_pin = ctx.pin_native_root(fresh);
    let arr_now = ctx.read_native_pin(arr_pin, arr);
    let fresh = ctx.read_native_pin(fresh_pin, fresh);
    for (i, slot) in raw.iter_mut().enumerate() {
        if matches!(slot, Value::Object(_)) {
            *slot = ctx.get_array_element(arr_now, i);
        }
    }
    for j in 0..collected {
        let value = ctx.get_array_element(arr_now, lead + j);
        ctx.set_array_element(fresh, j, value);
    }
    raw.push(Value::Object(Some(fresh)));
    let this_now = ctx.read_native_pin(this_pin, this);
    let fixed = match ctx.get_field_by_name(this_now, "target") {
        Value::Object(Some(h)) => h,
        _ => {
            ctx.unpin_native_roots(fresh_pin);
            return None;
        }
    };
    ctx.unpin_native_roots(fresh_pin);
    let declared: &str = &record.declared;
    let result = mh_real_direct_fast(ctx, fixed, &raw, Some(declared), false, false)?;
    Some(auto_box_return(ctx, result, declared))
}

/// The first parameter of the method descriptor `desc` (`Lp/C;`, `[I`, `J`),
/// borrowed from it, or `None` for `()...` or a malformed descriptor.
/// Interpreter round i7 wave 144, lane L4.
pub(crate) fn first_descriptor_param(desc: &str) -> Option<&str> {
    let rest = desc.strip_prefix('(')?;
    let bytes = rest.as_bytes();
    let dims = bytes.iter().take_while(|&&b| b == b'[').count();
    let end = match *bytes.get(dims)? {
        b'L' => dims + rest[dims..].find(';')? + 1,
        b')' => return None,
        _ => dims + 1,
    };
    rest.get(..end)
}

/// The class whose own `invokeWithArguments` bodies
/// [`mh_real_invoke_with_arguments_door`] falls back to (invokespecial).
pub(crate) const MH_CLASS_FOR_IWA_BODY: &str = "java/lang/invoke/MethodHandle";

pub(crate) const IWA_ADAPT_MEMO_SLOTS: usize = 64;

pub(crate) struct IwaAdaptLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) n: usize,
    pub(crate) adapted: Option<ObjectRef>,
}

thread_local! {
    /// Per-thread memo of `this.asType(genericMethodType(n))` for [`mh_real_iwa_adapted`],
    /// keyed by the handle's address and `n`, valid for one collection epoch under one VM
    /// (the argument of [`REAL_HANDLE_MEMO`]: nothing moves or is freed within an epoch,
    /// and a handle's `type` is final, so the adapted handle of an equal address is the
    /// same object's). A context that reports VM identity `0` (the mocks) is never memoised.
    pub(crate) static IWA_ADAPT_MEMO: std::cell::RefCell<Vec<IwaAdaptLine>> =
        std::cell::RefCell::new(
            (0..IWA_ADAPT_MEMO_SLOTS)
                .map(|_| IwaAdaptLine { vm: 0, epoch: 0, addr: 0, n: 0, adapted: None })
                .collect(),
        );
}

/// `this.asType(MethodType.genericMethodType(n))` through the JDK's own bodies (its
/// `genericMethodType` cache and single-slot `asType` cache), memoised per collection
/// epoch so the two by-name Java calls are paid on a miss only. `Ok(None)` declines.
pub(crate) fn mh_real_iwa_adapted(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    n: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let vm = ctx.vm_identity();
    let this_now = ctx.read_native_pin(this_pin, this);
    if vm != 0 {
        let epoch = ctx.gc_address_epoch();
        let addr = this_now.as_ptr() as usize;
        let slot = ((addr >> 3) ^ n) & (IWA_ADAPT_MEMO_SLOTS - 1);
        let hit = IWA_ADAPT_MEMO.with(|memo| {
            let memo = memo.borrow();
            let line = &memo[slot];
            if line.vm == vm && line.epoch == epoch && line.addr == addr && line.n == n {
                line.adapted
            } else {
                None
            }
        });
        if hit.is_some() {
            return Ok(hit);
        }
    }
    let generic_mt = match ctx.invoke_static(
        "java/lang/invoke/MethodType",
        "genericMethodType",
        "(I)Ljava/lang/invoke/MethodType;",
        &[Value::Int(n as i32)],
    )? {
        Some(Value::Object(Some(mt))) => mt,
        _ => return Ok(None),
    };
    let generic_pin = ctx.pin_native_root(generic_mt);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let generic_cur = ctx.read_native_pin(generic_pin, generic_mt);
    // Wave 114: `mh_real_as_type_bytecode` reads an `asTypeCache` hit.
    let adapted = match mh_real_as_type_bytecode(ctx, this_cur, generic_cur)? {
        Some(Value::Object(Some(h))) => h,
        _ => return Ok(None),
    };
    if vm != 0 {
        // Keyed by where `this` is NOW and the epoch NOW: the call above may have collected.
        let this_after = ctx.read_native_pin(this_pin, this);
        let epoch = ctx.gc_address_epoch();
        let addr = this_after.as_ptr() as usize;
        let slot = ((addr >> 3) ^ n) & (IWA_ADAPT_MEMO_SLOTS - 1);
        IWA_ADAPT_MEMO.with(|memo| {
            memo.borrow_mut()[slot] = IwaAdaptLine {
                vm,
                epoch,
                addr,
                n,
                adapted: Some(adapted),
            };
        });
    }
    Ok(Some(adapted))
}

/// Is `this` a real `DirectMethodHandle$Accessor` (an instance field getter or
/// setter) whose first argument -- the base -- is null? Reads only.
pub(crate) fn mh_iwa_null_base_of_instance_accessor(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    arr_pin: Option<usize>,
    array: Option<ObjectRef>,
) -> bool {
    let (Some(a), Some(p)) = (array, arr_pin) else {
        return false;
    };
    let this_now = ctx.read_native_pin(this_pin, this);
    let accessor = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this_now))
        .is_some_and(|n| &*n == "java/lang/invoke/DirectMethodHandle$Accessor");
    if !accessor {
        return false;
    }
    let a = ctx.read_native_pin(p, a);
    matches!(ctx.get_array_element(a, 0), Value::Object(None))
}

pub(crate) fn mh_real_invoke_with_arguments_lanes(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    arr_pin: Option<usize>,
    array: Option<ObjectRef>,
    type_threw: &mut bool,
) -> Option<MethodCallResult> {
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let n = match (array, arr_pin) {
        (Some(a), Some(p)) => {
            let a = ctx.read_native_pin(p, a);
            ctx.array_length(a)
        }
        _ => 0,
    };
    // Interpreter round i4 wave 85, lane L4 (`L3W56FieldHandleNullBase` row
    // `getter-with-arguments`): an instance field accessor given a null base
    // raises in `DirectMethodHandle.checkBase`, and HotSpot's trace lists
    // `MethodHandle.invokeWithArguments` right below it. The lanes raise from
    // this native, which is no Java frame; the JDK's own body (the caller's
    // fallback) is. Only that raising shape declines, so no other call pays.
    if n >= 1 && mh_iwa_null_base_of_instance_accessor(ctx, this_pin, this, arr_pin, array) {
        return None;
    }
    // Interpreter round i6 wave 132, lane L4 (`CRATONVM_MH_IWA_SERVED_CUSTOMIZATION`,
    // parity, default on): the spread invoker's `checkCustomized(adapter)`, which
    // HotSpot runs once per call before the adapter is entered. The adapter is
    // the JDK's own `asType(genericMethodType(n))` (the `asTypeCache` entry, or
    // `this` itself), so it is asked here, ahead of the direct lane, which
    // never needed it. A decline below takes the count back: the JDK body that
    // then runs counts the call itself.
    let mut early_adapted = None;
    let mut counted = None;
    if crate::nbflags().mh_iwa_served_customization && ctx.vm_identity() != 0 {
        match mh_real_iwa_adapted(ctx, this_pin, this, n) {
            Ok(Some(h)) => {
                let pin = ctx.pin_native_root(h);
                match iwa_served_check_customized(ctx, pin, h) {
                    Ok(previous) => {
                        counted = previous;
                        early_adapted = Some((pin, h));
                    }
                    // `customize()` threw (it runs the JDK's `LambdaForm.customize`):
                    // the call raises it, as the spread form's `checkCustomized` would.
                    Err(e) => {
                        ctx.unpin_native_roots(pin);
                        return Some(Err(e));
                    }
                }
            }
            // A decline (no handle answered), or the JDK's `asType` threw
            // (HotSpot then raises before the spread invoker runs: no count).
            // Nothing is counted, and the lanes below run exactly as before
            // (they ask again, and decline or set `type_threw` as before).
            Ok(None) | Err(_) => {}
        }
    }
    // Interpreter round i7 wave 136, lane L4 (page
    // `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residuals 1-2):
    // the adapter the lanes run, noted on this call's reflective row for the
    // thread's walks (`NativeExceptionAccess::invoke_with_arguments_adapters`),
    // and withdrawn before its pin goes. Two `Vec` operations; no Java.
    if let Some((pin, _)) = early_adapted {
        ctx.note_invoke_with_arguments_adapter(pin, true);
    }
    let answer =
        mh_real_iwa_served_lanes(ctx, this_pin, this, arr_pin, array, n, early_adapted, type_threw);
    if let Some((pin, _)) = early_adapted {
        ctx.note_invoke_with_arguments_adapter(pin, false);
    }
    if let Some((pin, adapter)) = early_adapted {
        if answer.is_none() {
            if let Some(previous) = counted {
                let adapter = ctx.read_native_pin(pin, adapter);
                iwa_served_uncount(&*ctx, adapter, previous);
            }
        }
        ctx.unpin_native_roots(pin);
    }
    answer
}

/// `CRATONVM_MH_IWA_SERVED_CUSTOMIZATION`'s `checkCustomized(adapter)` for the
/// adapter pinned at `pin` (interpreter round i6 wave 132, lane L4; page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residual 2).
///
/// JDK 25's spread invoker of `invokeWithArguments` is the generic invoker's
/// form edited to spread its array (`Invokers.invokeHandleForm(..,
/// LF_GEN_INVOKER)`): `checkGenericType(adapter, generic)` (the same-type
/// `asType`, `adapter` itself), then `Invokers.checkCustomized(adapter)`, then
/// `invokeBasic`. `checkCustomized` does nothing for a form already customized,
/// else `MethodHandle.maybeCustomize`: `customizationCount + 1` below
/// `CUSTOMIZE_THRESHOLD`, `customize()` at it. Measured on HotSpot 25.0.3
/// (`L4W132IwaCustomization`): count `k` after `k <= 127` calls, customized at
/// the 128th, count 127 from then on; a target that throws and an adapter
/// that raises `ClassCastException` are counted, an `asType` that throws is
/// not.
///
/// * the default threshold: [`real_direct_lane_customization`]'s reads; a
///   count below it is raised here and its old value handed back (`Some`), so
///   a decline can take it back; the due call runs the JDK's own
///   `Invokers.maybeCustomize(adapter)` (`customize()`, Java);
/// * `CUSTOMIZE_THRESHOLD=-1`: the invoker form has no `checkCustomized`
///   (`CHECK_CUSTOM == -1`): nothing;
/// * any other threshold: the JDK's own `Invokers.checkCustomized(adapter)`
///   (Java on every call; the count it found is handed back, so a decline
///   takes it back unless the body customized the form).
pub(crate) fn iwa_served_check_customized(
    ctx: &mut dyn NativeContext,
    pin: usize,
    adapter: ObjectRef,
) -> Result<Option<i32>, MethodCallFailed> {
    let threshold = customize_threshold(&*ctx);
    if threshold != MH_CUSTOMIZE_THRESHOLD {
        // The JDK's own static (wave 136, lane L3; `customize_threshold`),
        // not a re-read of the property (whose `trim()` also accepted
        // `" -1"`, which `Integer.parseInt` refuses).
        if threshold == -1 {
            return Ok(None);
        }
        let adapter = ctx.read_native_pin(pin, adapter);
        // Wave 133 (lane L9 review): the count before the JDK's body raises
        // it, so a decline below takes it back (`iwa_served_uncount` writes
        // it unless the form was customized meanwhile); without it a declined
        // call was counted twice, once here and once by the JDK body.
        let previous = match ctx.get_field_by_name(adapter, "customizationCount") {
            Value::Int(count) => Some(count),
            _ => None,
        };
        ctx.invoke_static(
            "java/lang/invoke/Invokers",
            "checkCustomized",
            "(Ljava/lang/invoke/MethodHandle;)V",
            &[Value::Object(Some(adapter))],
        )?;
        return Ok(previous);
    }
    let adapter = ctx.read_native_pin(pin, adapter);
    match real_direct_lane_customization(&*ctx, adapter) {
        RealDirectCustomization::Customized => Ok(None),
        RealDirectCustomization::Count(count) => {
            ctx.set_field_by_name(adapter, "customizationCount", Value::Int(count + 1));
            Ok(Some(count))
        }
        RealDirectCustomization::Due => {
            ctx.invoke_static(
                "java/lang/invoke/Invokers",
                "maybeCustomize",
                "(Ljava/lang/invoke/MethodHandle;)V",
                &[Value::Object(Some(adapter))],
            )?;
            Ok(None)
        }
    }
}

/// Takes back the count [`iwa_served_check_customized`] raised from
/// `previous`, unless the form was customized since (by another thread: the
/// count then no longer matters, as on HotSpot). Pure reads and one write.
pub(crate) fn iwa_served_uncount(ctx: &dyn NativeContext, adapter: ObjectRef, previous: i32) {
    if matches!(
        real_direct_lane_customization(ctx, adapter),
        RealDirectCustomization::Count(_) | RealDirectCustomization::Due
    ) {
        ctx.set_field_by_name(adapter, "customizationCount", Value::Int(previous));
    }
}

/// The lanes of [`mh_real_invoke_with_arguments_lanes`] after the
/// customization count: the direct lane, then the adapted handle's lanes.
/// `early_adapted`: the pin (and the reference) of the adapter the count already asked for (the
/// same answer [`mh_real_iwa_adapted`] would give again).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mh_real_iwa_served_lanes(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    arr_pin: Option<usize>,
    array: Option<ObjectRef>,
    n: usize,
    early_adapted: Option<(usize, ObjectRef)>,
    type_threw: &mut bool,
) -> Option<MethodCallResult> {
    if let Some(answer) = mh_real_iwa_direct(ctx, this_pin, this, arr_pin, array, n) {
        return Some(answer);
    }
    if let Some(answer) = mh_real_iwa_varargs_direct(ctx, this_pin, this, arr_pin, array, n) {
        return Some(answer);
    }
    let adapted = match early_adapted {
        Some((pin, adapter)) => ctx.read_native_pin(pin, adapter),
        None => match mh_real_iwa_adapted(ctx, this_pin, this, n) {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            // JIT round 20, lane dispatch: decline, so the JDK's own
            // `invokeWithArguments` raises it with its own frame (see
            // `mh_real_invoke_with_arguments_door`).
            Err(_) => {
                *type_threw = true;
                return None;
            }
        },
    };
    // Nothing below allocates before the lanes take over: read the elements now.
    let elements: Vec<Value> = match (array, arr_pin) {
        (Some(a), Some(p)) => {
            let a = ctx.read_native_pin(p, a);
            (0..n).map(|i| ctx.get_array_element(a, i)).collect()
        }
        _ => Vec::new(),
    };
    let generic = generic_method_type_descriptor(n);
    if let Some(result) =
        mh_real_direct_fast(ctx, adapted, &elements, Some(&generic), false, false)
    {
        return Some(result);
    }
    mh_real_lambda_form_dispatch(ctx, adapted, &elements, Some(&generic), false)
}

// ---------------------------------------------------------------------------
// SBR5 lane avh: `bindTo` on a REAL handle
// ---------------------------------------------------------------------------
//
// The JDK's body is `bindArgumentL(0, type.leadingReferenceParameter().cast(x))`: a
// `LambdaFormEditor` transform lookup, a `MethodType` edit and a species factory call, 8-11
// us here (HotSpot 2.2 us, the carrier 3.5 us). The result is a pure function of the
// receiver except for the bound value: the same species class, `type`, `form` and the
// receiver's own bound arguments, the new value last
// (`BoundMethodHandle.copyWithExtendL` -> `new Species_*(type, form, args.., x)`). So the
// first bind of a receiver in a collection epoch runs the JDK's body and files its answer,
// and a repeat bind of the same receiver with a value of the same class (or `null`, whose
// cast cannot fail) allocates a fresh instance of that species with the same fields and
// the new value.

pub(crate) const REAL_BIND_MEMO_SLOTS: usize = 64;

#[derive(Clone, Copy)]
pub(crate) struct RealBindLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) filled: bool,
    pub(crate) proto: Option<ObjectRef>,
    pub(crate) form: Option<ObjectRef>,
    pub(crate) class: Option<ClassId>,
    pub(crate) x_class: Option<ClassId>,
    /// A second bound-value class the JDK body admitted for this same receiver in this
    /// same epoch (JIT round 21 wave 19, lane mhbind2; [`mh_bindto_real_memo_two_classes_enabled`]),
    /// or `None`. Both passed the receiver's `leadingReferenceParameter().cast(x)`, which is
    /// a function of the receiver's `final` `type` and the value's class only.
    pub(crate) x_class_alt: Option<ClassId>,
    /// The species' field slots, resolved when the line was filed (JIT round 21 wave 17,
    /// lane mhbind; MHB-3, [`mh_bindto_real_mint_slots_enabled`]), or `None`: the mint
    /// resolves each field by name.
    pub(crate) slots: Option<RealBindSlots>,
}

/// The most bound arguments a species' [`RealBindSlots`] keeps (a wider species mints
/// by name).
const REAL_BIND_MAX_ARGS: usize = 8;

/// The slots of a `BoundMethodHandle` species' `type`, `form` and `arg<T><i>` fields
/// (JIT round 21 wave 17, lane mhbind; MHB-3): the mint stores by slot, without a
/// `format!` and two by-name resolutions per bound argument per `bindTo`.
#[derive(Clone, Copy)]
pub(crate) struct RealBindSlots {
    type_slot: usize,
    form_slot: usize,
    args: [usize; REAL_BIND_MAX_ARGS],
    /// Each bound argument's field descriptor byte (its species letter:
    /// `L`, `I`, `J`, `F` or `D`), for the batched mint (j27 wave 2, lane
    /// INVOKE; [`mh_bindto_real_mint_batch_enabled`]).
    desc: [u8; REAL_BIND_MAX_ARGS],
    count: usize,
}

/// `CRATONVM_MH_BINDTO_REAL_MINT_BATCH` -- perf, default ON, `=0` off (j27 wave 2,
/// lane INVOKE; page `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`,
/// `bind-only`): a memo-hit mint ([`mh_real_bind_to_mint_slots`]) reads the
/// prototype's fields in ONE typed batch and stores the fresh species' fields in
/// ONE typed batch ([`real_bind_mint_batched`]), instead of one descriptor-aware
/// door per field (two reads and four stores for `Species_LL`, each paying its own
/// forwarding, corrupt-cell watch and descriptor lookup). Same values, same slots,
/// same order. Asked per hit: memoised.
fn mh_bindto_real_mint_batch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_REAL_MINT_BATCH");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The field descriptor byte of a `BoundMethodHandle` species letter, or `None`
/// for a letter no species uses.
fn species_letter_descriptor(letter: char) -> Option<u8> {
    match letter {
        'L' => Some(b'L'),
        'I' => Some(b'I'),
        'J' => Some(b'J'),
        'F' => Some(b'F'),
        'D' => Some(b'D'),
        _ => None,
    }
}

/// `CRATONVM_MH_BINDTO_REAL_MINT_SLOTS` -- perf, default ON, `=0` off (JIT round 21
/// wave 17, lane mhbind; MHB-3 of the r18 backlog, page
/// `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`): a repeat
/// `bindTo` of a REAL handle ([`mh_real_bind_to`]) mints its species instance by the
/// slots resolved when the memo line was filed ([`RealBindSlots`]). Read when a line is
/// filed (the JDK body's answer), never on a hit.
fn mh_bindto_real_mint_slots_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_REAL_MINT_SLOTS")
}

/// `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES` -- perf, default ON, `=0` off (JIT round 21
/// wave 19, lane mhbind2; page `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`,
/// "Round 21 wave 19"): a [`mh_real_bind_to`] memo line keeps a SECOND bound-value class.
/// A line held one class, so `h.bindTo(x)` over values of two alternating classes
/// (`R21MhbindRows bind-each-mixed`: `Acc`, `Acc2`, `Acc`, ...) missed on every call and ran
/// the JDK body (8-11 us here) each time, then re-filed the line for the other class. When
/// the JDK body answers for the same receiver (same VM, epoch and address as the line it
/// replaces), the replaced line's class is kept as the alternate: it passed the same
/// receiver's cast, whose answer depends only on the receiver's `final` `type` and the
/// value's class. Read when a line is filed, never on a hit.
fn mh_bindto_real_memo_two_classes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES")
}

/// The [`RealBindSlots`] of species `class` (named `BoundMethodHandle$Species_<letters>`),
/// or `None` when a field does not resolve or the species is wider than
/// [`REAL_BIND_MAX_ARGS`]. Cold: runs when a memo line is filed.
fn real_bind_slots_of(ctx: &dyn NativeContext, class: ClassId) -> Option<RealBindSlots> {
    let name = ctx.class_name_arc_of_id(class)?;
    let letters = name.strip_prefix(BMH_SPECIES_PREFIX)?;
    let count = letters.chars().count();
    if count == 0 || count > REAL_BIND_MAX_ARGS {
        return None;
    }
    let mut args = [0usize; REAL_BIND_MAX_ARGS];
    let mut desc = [b'L'; REAL_BIND_MAX_ARGS];
    for (i, ch) in letters.chars().enumerate() {
        args[i] = ctx.resolve_field_index_by_class_id(class, &format!("arg{ch}{i}"))?;
        desc[i] = species_letter_descriptor(ch)?;
    }
    Some(RealBindSlots {
        type_slot: ctx.resolve_field_index_by_class_id(class, "type")?,
        form_slot: ctx.resolve_field_index_by_class_id(class, "form")?,
        args,
        desc,
        count,
    })
}

thread_local! {
    /// Per-thread memo of [`mh_real_bind_to`] answers keyed by the receiver's address, valid
    /// for one collection epoch under one VM (the argument of [`REAL_HANDLE_MEMO`]).
    pub(crate) static REAL_BIND_MEMO: std::cell::RefCell<Vec<RealBindLine>> =
        std::cell::RefCell::new(
            (0..REAL_BIND_MEMO_SLOTS)
                .map(|_| RealBindLine {
                    vm: 0,
                    epoch: 0,
                    addr: 0,
                    filled: false,
                    proto: None,
                    form: None,
                    class: None,
                    x_class: None,
                    x_class_alt: None,
                    slots: None,
                })
                .collect(),
        );
}

pub(crate) fn mh_real_bind_to(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    recv: Value,
) -> Option<MethodCallResult> {
    if !mh_is_real_handle(&*ctx, this) || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let recv_obj = match recv {
        Value::Object(o) => o,
        _ => return None,
    };
    let epoch = ctx.gc_address_epoch();
    let addr = this.as_ptr() as usize;
    let slot = (addr >> 3) & (REAL_BIND_MEMO_SLOTS - 1);
    let line = REAL_BIND_MEMO.with(|memo| memo.borrow()[slot]);
    if line.filled && line.vm == vm && line.epoch == epoch && line.addr == addr {
        // JIT round 21 wave 17, lane mhbind (correctness, unconditional):
        // `class_id_of_object` of a reference ARRAY answers its component
        // class, so a `String[]` matched a line filed for a `String` (and the
        // other way round) and the mint skipped the JDK body's
        // `leadingReferenceParameter().cast(x)`: no `ClassCastException`
        // where HotSpot raises one, and a bound array called as a `String`
        // receiver. An array value never hits; a line filed for one keeps no
        // class (`x_class` below).
        let admitted = match recv_obj {
            None => true,
            Some(x) => {
                !ctx.object_is_array(x) && {
                    let c = Some(ctx.class_id_of_object(x));
                    // JIT round 21 wave 19 (lane mhbind2): or the line's second class
                    // (`None` unless `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES` filed one).
                    line.x_class == c || (line.x_class_alt.is_some() && line.x_class_alt == c)
                }
            }
        };
        if admitted {
            if let (Some(proto), Some(form), Some(class)) = (line.proto, line.form, line.class) {
                if let Some(slots) = line.slots {
                    return mh_real_bind_to_mint_slots(ctx, proto, form, class, recv_obj, &slots);
                }
                return mh_real_bind_to_mint(ctx, proto, form, class, recv_obj);
            }
        }
    }
    // The JDK's own body, then file its answer.
    let this_pin = ctx.pin_native_root(this);
    let recv_pin = recv_obj.map(|x| ctx.pin_native_root(x));
    let answer = ctx.invoke_virtual_bytecode_only(
        this,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        &[recv],
    );
    let this_now = ctx.read_native_pin(this_pin, this);
    if let Ok(Some(Value::Object(Some(result)))) = &answer {
        let cid = ctx.class_id_of_object(*result);
        let species = ctx
            .class_name_arc_of_id(cid)
            .is_some_and(|n| n.starts_with(BMH_SPECIES_PREFIX) && n.ends_with('L'));
        if species {
            if let Value::Object(Some(form)) = ctx.get_field_by_name(*result, "form") {
                let x_class = recv_obj
                    .map(|x| ctx.read_native_pin(recv_pin.unwrap_or(0), x))
                    .filter(|x| !ctx.object_is_array(*x))
                    .map(|x| ctx.class_id_of_object(x));
                let epoch = ctx.gc_address_epoch();
                let addr = this_now.as_ptr() as usize;
                let slot = (addr >> 3) & (REAL_BIND_MEMO_SLOTS - 1);
                // MHB-3 (JIT round 21 wave 17, lane mhbind): read here, when filing.
                let slots = if mh_bindto_real_mint_slots_enabled() {
                    real_bind_slots_of(&*ctx, cid)
                } else {
                    None
                };
                // JIT round 21 wave 19 (lane mhbind2;
                // `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES`): the line this one replaces,
                // when it was filed for this very receiver (same VM, epoch and address)
                // with another non-array class, keeps that class as the alternate. `line`
                // was read before the JDK body ran; an epoch that moved since fails the
                // compare.
                let x_class_alt = if x_class.is_some()
                    && line.filled
                    && line.vm == vm
                    && line.epoch == epoch
                    && line.addr == addr
                    && line.x_class.is_some()
                    && line.x_class != x_class
                    && mh_bindto_real_memo_two_classes_enabled()
                {
                    line.x_class
                } else {
                    None
                };
                let filed = RealBindLine {
                    vm,
                    epoch,
                    addr,
                    filled: true,
                    proto: Some(*result),
                    form: Some(form),
                    class: Some(cid),
                    x_class,
                    x_class_alt,
                    slots,
                };
                REAL_BIND_MEMO.with(|memo| memo.borrow_mut()[slot] = filed);
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    Some(answer)
}

/// A fresh instance of the species `class` with `proto`'s fields, `form` as its form and
/// `recv` as the last bound argument.
pub(crate) fn mh_real_bind_to_mint(
    ctx: &mut dyn NativeContext,
    proto: ObjectRef,
    form: ObjectRef,
    class: ClassId,
    recv: Option<ObjectRef>,
) -> Option<MethodCallResult> {
    let name = ctx.class_name_arc_of_id(class)?;
    let letters = name.strip_prefix(BMH_SPECIES_PREFIX)?;
    let last = letters.chars().count().checked_sub(1)?;
    let mut scope = NativeHandleScope::new(ctx);
    let proto_h = scope.root(proto);
    let form_h = scope.root(form);
    let recv_h = recv.map(|x| scope.root(x));
    let fresh = match scope.instantiate(
        class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let fresh_h = scope.root(fresh);
    let (proto, form) = (scope.get(&proto_h), scope.get(&form_h));
    let fresh = scope.get(&fresh_h);
    let ty = scope.get_field_by_name(proto, "type");
    scope.set_field_by_name(fresh, "type", ty);
    scope.set_field_by_name(fresh, "form", Value::Object(Some(form)));
    for (i, ch) in letters.chars().enumerate() {
        let field = format!("arg{ch}{i}");
        let value = if i == last {
            Value::Object(recv_h.as_ref().map(|h| scope.get(h)))
        } else {
            scope.get_field_by_name(proto, &field)
        };
        scope.set_field_by_name(fresh, &field, value);
    }
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

/// [`mh_real_bind_to_mint`] by the line's [`RealBindSlots`] (JIT round 21 wave 17, lane
/// mhbind; MHB-3, [`mh_bindto_real_mint_slots_enabled`]): the same instance, the same
/// stores in the same order, each by its slot. The slots were resolved on `class`, the
/// species this mints.
fn mh_real_bind_to_mint_slots(
    ctx: &mut dyn NativeContext,
    proto: ObjectRef,
    form: ObjectRef,
    class: ClassId,
    recv: Option<ObjectRef>,
    slots: &RealBindSlots,
) -> Option<MethodCallResult> {
    let last = slots.count.checked_sub(1)?;
    let mut scope = NativeHandleScope::new(ctx);
    let proto_h = scope.root(proto);
    let form_h = scope.root(form);
    let recv_h = recv.map(|x| scope.root(x));
    let fresh = match scope.instantiate(
        class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    // Nothing below allocates: the instance and the rooted values stay where they are.
    let (proto, form) = (scope.get(&proto_h), scope.get(&form_h));
    // j27 wave 2 (lane INVOKE; `CRATONVM_MH_BINDTO_REAL_MINT_BATCH`).
    if mh_bindto_real_mint_batch_enabled() {
        let recv = recv_h.as_ref().map(|h| scope.get(h));
        real_bind_mint_batched(&*scope, proto, form, fresh, recv, slots);
        return Some(Ok(Some(Value::Object(Some(fresh)))));
    }
    let ty = scope.get_field(proto, slots.type_slot);
    scope.set_field(fresh, slots.type_slot, ty);
    scope.set_field(fresh, slots.form_slot, Value::Object(Some(form)));
    for (i, slot) in slots.args[..slots.count].iter().enumerate() {
        let value = if i == last {
            Value::Object(recv_h.as_ref().map(|h| scope.get(h)))
        } else {
            scope.get_field(proto, *slot)
        };
        scope.set_field(fresh, *slot, value);
    }
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

/// The stores of [`mh_real_bind_to_mint_slots`] in two typed batches (j27 wave 2,
/// lane INVOKE; [`mh_bindto_real_mint_batch_enabled`]): `proto`'s `type` and every
/// bound argument but the last in one read, then `fresh`'s `type`, `form` and
/// arguments (the last one `recv`) in one store, in the per-field path's order.
/// `type` and `form` are references; each argument's descriptor is its species
/// letter ([`RealBindSlots::desc`]). Reads and stores only: no GC point.
pub(crate) fn real_bind_mint_batched(
    ctx: &dyn NativeContext,
    proto: ObjectRef,
    form: ObjectRef,
    fresh: ObjectRef,
    recv: Option<ObjectRef>,
    slots: &RealBindSlots,
) {
    let count = slots.count.min(REAL_BIND_MAX_ARGS);
    if count == 0 {
        return;
    }
    // Read: `type`, then `arg0 .. arg(count-2)` (the last one is `recv`).
    let mut reads = [(slots.type_slot, b'L'); REAL_BIND_MAX_ARGS];
    for i in 0..count - 1 {
        reads[i + 1] = (slots.args[i], slots.desc[i]);
    }
    let mut copied = [Value::Object(None); REAL_BIND_MAX_ARGS];
    ctx.get_fields_typed(proto, &reads[..count], &mut copied[..count]);
    // Store: `type`, `form`, then every argument.
    let mut writes = [(slots.type_slot, b'L'); REAL_BIND_MAX_ARGS + 2];
    let mut values = [Value::Object(None); REAL_BIND_MAX_ARGS + 2];
    values[0] = copied[0];
    writes[1] = (slots.form_slot, b'L');
    values[1] = Value::Object(Some(form));
    for i in 0..count {
        writes[i + 2] = (slots.args[i], slots.desc[i]);
        values[i + 2] = if i + 1 == count {
            Value::Object(recv)
        } else {
            copied[i + 1]
        };
    }
    ctx.set_fields_typed(fresh, &writes[..count + 2], &values[..count + 2]);
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 74, lane L4: `MethodType.makeImpl` intern memo
// (`CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`, default off)
// ---------------------------------------------------------------------------
//
// Under `--jdk-only` every `Lookup` finder is the JDK's own bytecode, and a
// field finder interns two `MethodType`s per call (`getMethodOrFieldType` ->
// `methodType(ftype)`, then `insertParameterTypes(0, refc)`), a method finder
// at least one. Each intern probe is `MethodType.makeImpl`: a primordial
// `MethodType`, `internTable.get` -> `ReferencedKeyMap.get` ->
// `removeStaleReferences` (`ReferenceQueue.poll`) -> a `StrongReferenceKey`
// -> `ConcurrentHashMap.get` (`MethodType.hashCode`: one identity-hash native
// per component; `tabAt`: an `Unsafe` acquire native; `equals` through
// `WeakReferenceKey.get` / `Objects.equals` / `Arrays.equals`), about thirty
// interpreted invocations and three to six native crossings for an answer
// that, for a type interned before, is a pure function of the components.
//
// HotSpot runs the same bytecode, JIT-compiled. Here, behind the switch, a
// reviewed `Intrinsic` over `makeImpl` (registered ONLY while the switch is on,
// so the off state costs nothing) answers a type this thread interned before,
// in the same VM and collection epoch, from a per-thread memo keyed by the
// ADDRESSES of the return-type mirror and of each parameter mirror (at most
// `MT_INTERN_MAX_PARAMS`). Every other call runs the JDK's own body
// (`invoke_special_bytecode_only*`), whose answer, when no collection ran
// during it, is filed.
//
// Why a hit is the JDK's answer, object for object:
// * `makeImpl` always returns the INTERNED instance for its components: the
//   `internTable.get` hit, or `internTable.intern(mt)`'s winner. Neither
//   `trusted` nor the caller's array identity changes which object that is
//   (`trusted` only picks the defensive copy and the checks of a type not yet
//   interned, and a filed type was interned).
// * Within one collection epoch nothing moves or is freed (the argument of
//   `REAL_HANDLE_MEMO`), so an equal mirror address is the same class, and the
//   intern table cannot lose the row: `ReferencedKeyMap` removes an entry only
//   when its `WeakReferenceKey` was cleared, which only a collection does.
// * Only a successful answer is filed. A call that throws (a `null` component:
//   `MethodType.hashCode`'s NPE; a `void` parameter: `checkPtypes`' IAE) is
//   declined before the memo (a `null` never makes a key) or never filed.
//
// What a hit skips that is not the answer: the primordial `MethodType` and
// `StrongReferenceKey` allocations (garbage at once), and the
// `removeStaleReferences` poll (cleared keys of EARLIER epochs are removed by
// the next call that runs the body; the map's contents seen through any
// `MethodType` API do not change). A debugger's breakpoint or method-entry
// event inside `makeImpl` does not fire on a hit, as for every `Intrinsic`.
// Proposal `interpreter-i74-L4-proposal-the-jdk-only-finder-mint-cost-model-DONE-20261014`;
// bench `tools/probes/interp/L4/L4W74FinderMintSplitBench.java`, probe
// `tools/probes/interp/L4/L4W74MethodTypeIntern.java`.
//
// Wave 75 (lane L4), after the host measured wave 74's memo: under the JIT it
// cost every finder row (a native-backed method is never compiled, so each
// miss ran the body interpreted and each compiled caller paid a crossing), so
// it now registers only in a VM WITHOUT a JIT
// (`mh_method_type_intern_memo_registers`); and without a JIT its 128-line
// direct-mapped memo missed on any working set over ~100 types
// (`mt-intern-varied`, 1024 types: +28%), each miss paying the crossing and
// a native re-entry on top of the body. The memo is now two-way
// set-associative with a mixed hash, and a thread whose misses keep hitting
// keys it evicted (a victim bitmap tells thrash from cold misses) grows it
// fourfold up to `MT_INTERN_MEMO_MAX_SLOTS`. Bench
// `tools/probes/interp/L4/L4W75InternMemoMissBench.java`.

/// `java/lang/invoke/MethodType`.
pub(crate) const MT_INTERN_CLASS: &str = "java/lang/invoke/MethodType";
/// `MethodType.makeImpl(Class<?> rtype, Class<?>[] ptypes, boolean trusted)`.
pub(crate) const MT_INTERN_DESC: &str = "(Ljava/lang/Class;[Ljava/lang/Class;Z)Ljava/lang/invoke/MethodType;";
/// Lines of a fresh [`MT_INTERN_MEMO`] (a power of two; two ways per set).
pub(crate) const MT_INTERN_MEMO_SLOTS: usize = 128;
/// The most lines a thread's memo grows to (a power of two). Wave 75: a
/// thread that keeps re-interning more types than its memo holds (the
/// thrash the victim bitmap counts) grows it fourfold, up to this cap
/// (~360 KiB of lines on a 64-bit host); a thread that interns a stream of
/// types once each (a boot) has no thrash and keeps [`MT_INTERN_MEMO_SLOTS`].
pub(crate) const MT_INTERN_MEMO_MAX_SLOTS: usize = 4096;
/// Bits of each victim bitmap generation (a power of two): one bit per hash
/// of a key evicted while live, so a miss can tell "evicted here before"
/// (thrash: a bigger memo would have answered) from "never seen" (a cold
/// miss). Two generations of [`MT_INTERN_VICTIM_GENERATION`] evictions each
/// keep the false-positive rate of a cold miss at most ~12% (6% x 2).
pub(crate) const MT_INTERN_VICTIM_BITS: usize = 16384;
/// Evictions per victim bitmap generation.
pub(crate) const MT_INTERN_VICTIM_GENERATION: usize = 1024;
/// The most parameter types a memo key holds; a longer type runs the body.
pub(crate) const MT_INTERN_MAX_PARAMS: usize = 6;

/// `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO` -- performance, default OFF,
/// `truthy_word`. Read at registration (the `Intrinsic` exists only while it
/// is on, and, since wave 75, only in a VM without a JIT:
/// [`mh_method_type_intern_memo_registers`]); see the section comment above.
pub(crate) fn mh_method_type_intern_memo_enabled() -> bool {
    crate::nbflags().mh_method_type_intern_memo
}

/// Does `r`'s VM get the `makeImpl` intrinsic? The switch, and no JIT.
///
/// Wave 75 (host, `L4W74FinderMintSplitBench --jdk-only`, 5 reps): with the
/// JIT on, the intrinsic made `mt-intern-varied` 4.4x slower and the finder
/// rows 1.2-2x slower while only the pure-repeat `mt-intern-*` rows gained. A
/// method backed by a registered native is never compiled
/// (`interpreter::execute`'s `registered_native_will_run` gate), so every
/// miss ran `makeImpl`'s body INTERPRETED through a native re-entry where the
/// JIT runs it compiled, and every compiled caller paid a native crossing for
/// what had been a compiled call. Without a JIT the body is interpreted
/// either way and only the miss penalty remains (the growing, two-way memo
/// below keeps re-interned types from missing).
///
/// Wave 80 (lane L4): "no JIT" is `interpreted_intrinsic_registers`, which is
/// exactly `!jit_enabled()` unless `CRATONVM_INTRINSIC_BODY_COMPILES` (default
/// off) is on; then a JIT VM registers it too and the registrar marks the row
/// body-compilable, so `makeImpl` still compiles and a miss reaches the
/// compiled body (proposal
/// `i77-L5-proposal-intrinsic-declines-reach-the-compiled-body-20261110`).
pub(crate) fn mh_method_type_intern_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_method_type_intern_memo_enabled()
        && r.interpreted_intrinsic_registers_row(MT_INTERN_BODY_ROW)
}

/// The memo's row token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (wave 80):
/// listed, a JIT VM registers it and marks `makeImpl` body-compilable without
/// the global switch.
pub(crate) const MT_INTERN_BODY_ROW: &str = "method-type-intern";

/// The components of one `makeImpl` call: the addresses of the return-type
/// mirror and of the first `len` parameter mirrors (the rest zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MtInternKey {
    pub(crate) rtype: usize,
    pub(crate) len: usize,
    pub(crate) params: [usize; MT_INTERN_MAX_PARAMS],
}

#[derive(Clone, Copy)]
pub(crate) struct MtInternLine {
    pub(crate) epoch: u64,
    pub(crate) key: MtInternKey,
    pub(crate) mt: ObjectRef,
}

/// One thread's `makeImpl` memo: two-way sets of lines, valid for one VM
/// (`vm`; another VM's first filing resets the memo) and, line by line, for
/// the collection epoch the line was filed in.
pub(crate) struct MtInternMemo {
    pub(crate) vm: usize,
    /// `2 * sets` lines; set `s` is lines `2s` (the more recent) and `2s+1`.
    pub(crate) lines: Vec<Option<MtInternLine>>,
    /// The current victim generation: one bit per hash of a key evicted
    /// while live (same epoch).
    pub(crate) victims: Vec<u64>,
    /// The previous generation (consulted, never set).
    pub(crate) victims_old: Vec<u64>,
    /// Evictions recorded in `victims`.
    pub(crate) victim_count: usize,
    /// Misses since the last growth or window reset.
    pub(crate) misses: usize,
    /// Of `misses`, those on a key a victim generation remembers.
    pub(crate) thrash: usize,
}

impl MtInternMemo {
    pub(crate) fn new() -> Self {
        Self {
            vm: 0,
            lines: vec![None; MT_INTERN_MEMO_SLOTS],
            victims: vec![0; MT_INTERN_VICTIM_BITS / 64],
            victims_old: vec![0; MT_INTERN_VICTIM_BITS / 64],
            victim_count: 0,
            misses: 0,
            thrash: 0,
        }
    }

    /// The first line of `hash`'s set.
    pub(crate) fn set_of(&self, hash: usize) -> usize {
        2 * (hash & (self.lines.len() / 2 - 1))
    }

    /// The victim bit of `hash` (other bits than the ones that pick a set).
    pub(crate) fn victim_bit(hash: usize) -> usize {
        hash.rotate_right(17) & (MT_INTERN_VICTIM_BITS - 1)
    }

    pub(crate) fn get(&mut self, vm: usize, epoch: u64, key: &MtInternKey) -> Option<ObjectRef> {
        if vm == 0 || self.vm != vm {
            return None;
        }
        let at = self.set_of(mt_intern_hash(key));
        let live =
            |l: &Option<MtInternLine>| matches!(l, Some(l) if l.epoch == epoch && l.key == *key);
        if live(&self.lines[at]) {
            return self.lines[at].map(|l| l.mt);
        }
        if live(&self.lines[at + 1]) {
            // The second way hit: it is now the more recent one.
            self.lines.swap(at, at + 1);
            return self.lines[at].map(|l| l.mt);
        }
        None
    }

    /// Record a miss on `key`: a key this memo evicted recently counts as
    /// thrash.
    pub(crate) fn note_miss(&mut self, vm: usize, key: &MtInternKey) {
        if vm == 0 || self.vm != vm {
            return;
        }
        if self.misses >= 4 * self.lines.len() {
            // A window with no growth: start the next one, so a long run of
            // cold misses cannot outvote a later cyclic phase.
            self.misses = 0;
            self.thrash = 0;
        }
        self.misses += 1;
        let bit = Self::victim_bit(mt_intern_hash(key));
        let mask = 1u64 << (bit % 64);
        if (self.victims[bit / 64] | self.victims_old[bit / 64]) & mask != 0 {
            self.thrash += 1;
        }
    }

    /// Grow when at least a quarter of the lines' worth of misses were
    /// thrash, and thrash is at least half of the misses (a cold stream's
    /// false positives stay far below that).
    pub(crate) fn should_grow(&self) -> bool {
        self.lines.len() < MT_INTERN_MEMO_MAX_SLOTS
            && self.thrash >= self.lines.len() / 4
            && 2 * self.thrash >= self.misses
    }

    pub(crate) fn put(&mut self, vm: usize, epoch: u64, key: MtInternKey, mt: ObjectRef) {
        if vm == 0 {
            return;
        }
        if self.vm != vm {
            *self = Self::new();
            self.vm = vm;
        }
        if self.should_grow() {
            self.grow(epoch);
        }
        let at = self.set_of(mt_intern_hash(&key));
        let line = Some(MtInternLine { epoch, key, mt });
        let reusable = |l: &Option<MtInternLine>| match l {
            None => true,
            Some(l) => l.epoch != epoch || l.key == key,
        };
        if reusable(&self.lines[at]) {
            self.lines[at] = line;
            return;
        }
        if !reusable(&self.lines[at + 1]) {
            // Both ways live: the older one is evicted.
            if let Some(old) = self.lines[at + 1] {
                self.note_victim(mt_intern_hash(&old.key));
            }
        }
        self.lines[at + 1] = self.lines[at];
        self.lines[at] = line;
    }

    pub(crate) fn note_victim(&mut self, hash: usize) {
        if self.victim_count >= MT_INTERN_VICTIM_GENERATION {
            std::mem::swap(&mut self.victims, &mut self.victims_old);
            self.victims.iter_mut().for_each(|w| *w = 0);
            self.victim_count = 0;
        }
        let bit = Self::victim_bit(hash);
        self.victims[bit / 64] |= 1u64 << (bit % 64);
        self.victim_count += 1;
    }

    /// Four times the lines (at most [`MT_INTERN_MEMO_MAX_SLOTS`]); the lines
    /// of epoch `epoch` are kept.
    pub(crate) fn grow(&mut self, epoch: u64) {
        let lines = (self.lines.len() * 4).min(MT_INTERN_MEMO_MAX_SLOTS);
        let old = std::mem::replace(&mut self.lines, vec![None; lines]);
        self.victims.iter_mut().for_each(|w| *w = 0);
        self.victims_old.iter_mut().for_each(|w| *w = 0);
        self.victim_count = 0;
        self.misses = 0;
        self.thrash = 0;
        for l in old.into_iter().flatten().filter(|l| l.epoch == epoch) {
            let at = self.set_of(mt_intern_hash(&l.key));
            if self.lines[at].is_none() {
                self.lines[at] = Some(l);
            } else if self.lines[at + 1].is_none() {
                self.lines[at + 1] = Some(l);
            }
        }
    }
}

thread_local! {
    /// Per-thread memo of `makeImpl` answers (see [`MtInternMemo`] and the
    /// section comment).
    pub(crate) static MT_INTERN_MEMO: std::cell::RefCell<MtInternMemo> =
        std::cell::RefCell::new(MtInternMemo::new());
    /// `MethodType`'s class id in the VM of identity `.0` (a boot class: never
    /// unloaded), so the body runs without a by-name class lookup.
    pub(crate) static MT_INTERN_CLASS_ID: std::cell::Cell<Option<(usize, ClassId)>> =
        const { std::cell::Cell::new(None) };
    /// `makeImpl`'s declared-method index in `MethodType` of the VM of
    /// identity `.0`, as the VM last handed it back through
    /// `invoke_special_bytecode_only_hinted` (wave 76, L4;
    /// `CRATONVM_NATIVE_OWN_BODY_HINT`, which the VM reads: off, it hands
    /// back `None` and nothing is kept).
    pub(crate) static MT_INTERN_BODY_HINT: std::cell::Cell<Option<(usize, ClassId, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// The memo key of a `makeImpl` call (`args` = `rtype`, `ptypes`, `trusted`),
/// or `None` when it has none: a `null` return type, array or element (the
/// body raises the NPE), a non-array, or more than [`MT_INTERN_MAX_PARAMS`]
/// parameters. Pure reads; no GC point.
pub(crate) fn mt_intern_key(ctx: &dyn NativeContext, args: &[Value]) -> Option<MtInternKey> {
    let Some(Value::Object(Some(rtype))) = args.first() else {
        return None;
    };
    let Some(Value::Object(Some(ptypes))) = args.get(1) else {
        return None;
    };
    if !ctx.object_is_array(*ptypes) {
        return None;
    }
    let len = ctx.array_length(*ptypes);
    if len > MT_INTERN_MAX_PARAMS {
        return None;
    }
    let mut params = [0usize; MT_INTERN_MAX_PARAMS];
    for (i, slot) in params.iter_mut().enumerate().take(len) {
        let Value::Object(Some(p)) = ctx.get_array_element(*ptypes, i) else {
            return None;
        };
        *slot = p.as_ptr() as usize;
    }
    Some(MtInternKey {
        rtype: rtype.as_ptr() as usize,
        len,
        params,
    })
}

/// A well-mixed hash of a key: mirror addresses are aligned and close
/// together, and the LOW bits pick the set, so they are mixed in (wave 74's
/// slot was a shifted XOR of the addresses).
pub(crate) fn mt_intern_hash(key: &MtInternKey) -> usize {
    let mut h = (key.rtype as u64) ^ (key.len as u64).rotate_left(29);
    for (i, p) in key.params.iter().enumerate().take(key.len) {
        h ^= (*p as u64).rotate_left(((i as u32) * 11 + 13) % 64);
    }
    // splitmix64's finalizer.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (h ^ (h >> 31)) as usize
}

/// The first line of `key`'s set in a fresh memo.
#[cfg(test)]
pub(crate) fn mt_intern_slot(key: &MtInternKey) -> usize {
    2 * (mt_intern_hash(key) & (MT_INTERN_MEMO_SLOTS / 2 - 1))
}

/// The filed answer for `key` in VM `vm` and collection epoch `epoch`.
#[cfg(test)]
pub(crate) fn mt_intern_memo_get(vm: usize, epoch: u64, key: &MtInternKey) -> Option<ObjectRef> {
    MT_INTERN_MEMO.with(|m| m.borrow_mut().get(vm, epoch, key))
}

/// File `mt` for `key`, read in epoch `epoch` (the caller has checked that no
/// collection ran since).
pub(crate) fn mt_intern_memo_put(vm: usize, epoch: u64, key: MtInternKey, mt: ObjectRef) {
    MT_INTERN_MEMO.with(|m| m.borrow_mut().put(vm, epoch, key, mt));
}

/// `MethodType`'s class id in VM `vm`, once a body's answer has told it.
pub(crate) fn mt_intern_class_id(vm: usize) -> Option<ClassId> {
    MT_INTERN_CLASS_ID
        .with(std::cell::Cell::get)
        .filter(|(v, _)| vm != 0 && *v == vm)
        .map(|(_, cid)| cid)
}

/// The JDK's own `makeImpl` body (this `Intrinsic` skips its own registration).
///
/// gci i18/vp: a tail call (`continuation_pin::tail_call`). The only caller,
/// [`mt_make_impl_intrinsic`] (a registered callback), returns the body's
/// answer unchanged, same method and descriptor; what it and this function do
/// after the call is filing a memo row, a class id and a method-index hint,
/// all caches a skipped store only costs a later miss on. The body interns
/// through a `ConcurrentHashMap` whose contended tree bins park
/// (`TreeBin.contendedLock`), which unmounts a virtual thread on HotSpot.
pub(crate) fn mt_make_impl_jdk_body(
    ctx: &mut dyn NativeContext,
    vm: usize,
    args: &[Value],
) -> MethodCallResult {
    let _transparent = cratonvm_native_api::continuation_pin::transparent();
    match mt_intern_class_id(vm) {
        Some(cid) => {
            // Wave 76 (L4): the re-entry by the method's declared index when
            // the VM's `CRATONVM_NATIVE_OWN_BODY_HINT` is on (it verifies the
            // index on every call and hands back the one to keep).
            let mut hint = MT_INTERN_BODY_HINT
                .with(std::cell::Cell::get)
                .filter(|(v, c, _)| *v == vm && *c == cid)
                .map(|(_, _, i)| i);
            let answer = ctx.invoke_special_bytecode_only_hinted(
                cid,
                &mut hint,
                MT_INTERN_CLASS,
                "makeImpl",
                MT_INTERN_DESC,
                args,
            );
            MT_INTERN_BODY_HINT.with(|h| h.set(hint.map(|i| (vm, cid, i))));
            answer
        }
        None => ctx.invoke_special_bytecode_only(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC, args),
    }
}

/// `MethodType.makeImpl(Class, Class[], boolean)` under
/// `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`: a memo hit, else the JDK's body
/// (its answer filed when no collection ran during it).
pub(crate) fn mt_make_impl_intrinsic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let key = if vm != 0 {
        mt_intern_key(&*ctx, args)
    } else {
        None
    };
    if let Some(key) = &key {
        let hit = MT_INTERN_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            let hit = m.get(vm, epoch, key);
            if hit.is_none() {
                m.note_miss(vm, key);
            }
            hit
        });
        if let Some(mt) = hit {
            return Ok(Some(Value::Object(Some(mt))));
        }
    }
    let answer = mt_make_impl_jdk_body(ctx, vm, args);
    if let (Some(key), Ok(Some(Value::Object(Some(mt))))) = (key, &answer) {
        // The key's addresses were read before the body ran (a GC point):
        // filed only when no collection ran since.
        if ctx.gc_address_epoch() == epoch {
            let mt = *mt;
            mt_intern_memo_put(vm, epoch, key, mt);
            if mt_intern_class_id(vm).is_none() {
                let cid = ctx.class_id_of_object(mt);
                MT_INTERN_CLASS_ID.with(|c| c.set(Some((vm, cid))));
            }
        }
    }
    answer
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 77, lane L4: a repeat `--jdk-only` field finder is
// minted from its first answer (`CRATONVM_MH_FINDER_MINT_MEMO`, default on
// since wave 78)
// ---------------------------------------------------------------------------
//
// `lk.findGetter(Box.class, "v", int.class)` costs ~31-37 us under `--jdk-only
// --nojit` (the JDK's `resolveOrFail` + `getDirectField` interpreted: ~165
// calls and ~14 native crossings, no pathological step; page
// `interpreter-i74-L4-proposal-the-jdk-only-finder-mint-cost-model-DONE-20261014`). Its answer
// is a pure function of inputs that are fixed within a collection epoch:
// the `Lookup`'s `lookupClass`, `prevLookupClass` and `allowedModes` (all the
// access check reads), and the `refc`, `name` and `type` objects. So behind the
// switch, in a `--jdk-only` VM over a real JDK WITHOUT a JIT (wave 75: an
// `Intrinsic` over JDK bytecode is never compiled, so the JIT arm loses), the
// four field finders are reviewed `Intrinsic`s: the first call with a key runs
// the JDK's own body (`invoke_virtual_bytecode_only` on the `Lookup`) and files
// its answer when it is EXACTLY a `DirectMethodHandle$Accessor` (instance
// finders) or `$StaticAccessor` (static finders) and no collection ran; a
// repeat call allocates a fresh `MemberName` with the filed member's six words
// and a fresh accessor of the filed class with the filed handle's words, every
// other field at its constructor default (`asTypeCache`, `asTypeSoftCache`,
// `customizationCount`, `updateInProgress`). HotSpot's finder also returns a
// fresh `MemberName` (`MemberName.Factory.resolve` clones) and a fresh handle,
// so identity matches: nothing is shared but the interned `type`, the cached
// `form` (`DirectMethodHandle.ACCESSOR_FORMS` never changes a filled slot) and
// the classes / static base, which the body would share too.
//
// Declined (the body runs, nothing filed): a null argument (the body raises
// the NPE), any throw (an access failure, `NoSuchFieldException`, a `final`
// field's setter), a collection during the body (the key's addresses were
// read before it), and `CRATONVM_DBG_MH_DISPATCH`. A debugger breakpoint
// inside the finder's body does not fire on a hit, as for every `Intrinsic`.
//
// A static finder's answer depends on its field's DECLARING class's
// initialization: the body picks the `_INIT` accessor form for an
// uninitialized holder (`DirectMethodHandle.shouldBeInitialized`). Wave 81
// (lane L4): such an answer is filed too. An uninitialized holder and an
// inherited field (the holder a superclass or interface of `refc`) were always
// declined, which under the JIT made every `findStaticGetter` of a class not
// yet used ~2 us SLOWER than no intrinsic at all (the decline re-enters the
// body through the native door). The line records the holder and its
// initialization state, both read before the body and checked unchanged after
// it; a hit is minted only while the holder's state is still the filed one
// (initialization is monotonic, so a filed "uninitialized" answer retires at
// the first hit after `<clinit>`, and the body re-files the initialized form).
// The minted handle takes the `form` the body answered at filing time, never
// the filed handle's current one, which its own first call may update
// (`ensureInitialized` -> `updateForm`) and customization may replace.
//
// The four triples are rows of `RETIRED_SHADOW_SBR4_MHCONS_TRIPLES`: the
// registry re-tags only a `Bridge` registration of a retired triple, so this
// `Intrinsic` (registered after the `--compatible` carriers, last write wins)
// owns the slot only in the VMs the switch admits, and `--compatible` keeps the
// carriers. The rows stay retired: in every VM with a JIT, and with the switch
// at `0`, the JDK's finder body runs, and with the memo a repeat call's answer
// is the body's own first answer, minted. `the native-registry census`
// `the_no_jit_strict_boot_keeps_only_the_finder_mint_memo_over_retired_rows`
// freezes the no-JIT strict boot's survivors at exactly these four (wave 78).
// Wave 81 (lane L4): the `finder-mint` row of
// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` is on by default
// (`cratonvm_native_api::registry::INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`), so the strict
// boot WITH a JIT registers the same four (each marked body-compilable, so the
// JDK finder bodies still compile), and `no_retired_triple_survives_the_strict_boot`
// allows exactly these four when the row and the memo are on.

/// `java/lang/invoke/MethodHandles$Lookup`.
pub(crate) const FINDER_MINT_LOOKUP: &str = "java/lang/invoke/MethodHandles$Lookup";
/// The four field finders' shared descriptor.
pub(crate) const FINDER_MINT_DESC: &str =
    "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";
/// Lines of a thread's finder memo (a power of two, direct-mapped).
pub(crate) const FINDER_MINT_MEMO_SLOTS: usize = 64;
/// The words of a resolved `MemberName` (JDK 25; `vmindex` / `vmtarget` are
/// HotSpot-injected and absent from this layout).
pub(crate) const FINDER_MINT_MEMBER_WORDS: &[&str] =
    &["clazz", "name", "type", "flags", "method", "resolution"];
/// The words a `DirectMethodHandle$Accessor` copies from the filed handle
/// (`member` is the fresh `MemberName`).
pub(crate) const FINDER_MINT_ACCESSOR_WORDS: &[&str] =
    &["type", "form", "crackable", "fieldType", "fieldOffset"];
/// The words a `DirectMethodHandle$StaticAccessor` copies.
pub(crate) const FINDER_MINT_STATIC_ACCESSOR_WORDS: &[&str] = &[
    "type",
    "form",
    "crackable",
    "fieldType",
    "staticBase",
    "staticOffset",
];

/// `CRATONVM_MH_FINDER_MINT_MEMO` -- performance, default ON since wave 78,
/// `truthy_word_default_true`.
/// Read at registration; see the section comment above.
pub(crate) fn mh_finder_mint_memo_enabled() -> bool {
    crate::nbflags().mh_finder_mint_memo
}

/// Does `r`'s VM get the four finder `Intrinsic`s? The switch, `--jdk-only`
/// over a real JDK, and no JIT -- or, wave 80, a JIT VM under
/// `CRATONVM_INTRINSIC_BODY_COMPILES` or the `finder-mint` row of
/// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (that row default ON since wave 81;
/// `interpreted_intrinsic_registers_row`), where [`register_finder_mint_memo`]
/// marks the four rows body-compilable so the finder bodies still compile.
pub(crate) fn mh_finder_mint_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_finder_mint_memo_enabled()
        && finder_mint_mode_admits(r)
        && r.interpreted_intrinsic_registers_row(FINDER_MINT_BODY_ROW)
}

/// The VMs whose `Lookup` finders run the JDK's own bodies and so may carry
/// the finder mint memos: `--jdk-only` over a real JDK, and (interpreter
/// round i5 wave 109, lane L4, `CRATONVM_MH_COMPAT_FINDER_MINT`, default off)
/// `--compatible` over a real JDK with the `JdkInvoke` row in force, where the
/// finder carriers are not registered (`compat_row_of_shadow` drops them) and
/// the same bodies run (page `i89-L4-method-finder-per-class-and-per-handle-cost`,
/// wave 109). The memos read no mode-specific state: the key, the filing rule
/// and the mint look at the `Lookup`, the arguments and the body's own answer.
/// The registrars register inside a `set_admitting_family_intrinsic` scope,
/// which the family drop keeps (any other registration of these triples is
/// refused there).
pub(crate) fn finder_mint_mode_admits(r: &NativeMethodRegistry) -> bool {
    use cratonvm_types::compat::CompatibilityMode;
    r.real_jdk()
        && match r.compatibility_mode() {
            CompatibilityMode::JdkOnly => true,
            CompatibilityMode::Compatible => {
                crate::nbflags().mh_compat_finder_mint
                    && r.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
            }
        }
}

/// The finder mint memo's row token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`
/// (wave 80; host: the four finders ~5x faster under the JIT with the body
/// compiling, where the intern memo's rows lose).
pub(crate) const FINDER_MINT_BODY_ROW: &str = "finder-mint";

/// One of the four `Lookup` field finders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldFinder {
    Getter,
    Setter,
    StaticGetter,
    StaticSetter,
}

impl FieldFinder {
    pub(crate) const ALL: [FieldFinder; 4] = [
        FieldFinder::Getter,
        FieldFinder::Setter,
        FieldFinder::StaticGetter,
        FieldFinder::StaticSetter,
    ];

    pub(crate) fn method_name(self) -> &'static str {
        match self {
            FieldFinder::Getter => "findGetter",
            FieldFinder::Setter => "findSetter",
            FieldFinder::StaticGetter => "findStaticGetter",
            FieldFinder::StaticSetter => "findStaticSetter",
        }
    }

    pub(crate) fn is_static(self) -> bool {
        matches!(self, FieldFinder::StaticGetter | FieldFinder::StaticSetter)
    }

    /// The class the body's answer must be, exactly, to be filed.
    pub(crate) fn accessor_class(self) -> &'static str {
        if self.is_static() {
            "java/lang/invoke/DirectMethodHandle$StaticAccessor"
        } else {
            "java/lang/invoke/DirectMethodHandle$Accessor"
        }
    }

    pub(crate) fn tag(self) -> u8 {
        match self {
            FieldFinder::Getter => 1,
            FieldFinder::Setter => 2,
            FieldFinder::StaticGetter => 3,
            FieldFinder::StaticSetter => 4,
        }
    }

    pub(crate) fn callback(self) -> fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult {
        match self {
            FieldFinder::Getter => mh_find_getter_mint,
            FieldFinder::Setter => mh_find_setter_mint,
            FieldFinder::StaticGetter => mh_find_static_getter_mint,
            FieldFinder::StaticSetter => mh_find_static_setter_mint,
        }
    }
}

/// A finder call's memo key: the finder, the `Lookup`'s three access words
/// (the classes by address, `0` for no previous lookup class), and the
/// addresses of `refc`, `name` and `type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FinderMintKey {
    pub(crate) finder: u8,
    pub(crate) lookup_class: usize,
    pub(crate) prev_lookup_class: usize,
    pub(crate) modes: i32,
    pub(crate) refc: usize,
    pub(crate) name: usize,
    pub(crate) ty: usize,
}

/// One filed answer: valid for VM `vm` in collection epoch `epoch` only (the
/// two objects are held by address, which no collection has moved or freed
/// while the epoch holds).
#[derive(Clone, Copy)]
pub(crate) struct FinderMintLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) key: FinderMintKey,
    pub(crate) proto: ObjectRef,
    pub(crate) proto_member: ObjectRef,
    pub(crate) accessor_class: ClassId,
    pub(crate) member_class: ClassId,
    /// The body's `form` at filing time (wave 81): a static form of
    /// `DirectMethodHandle.ACCESSOR_FORMS`, which never changes a filled slot.
    pub(crate) form: Value,
    /// A static finder's field holder and whether it was initialized when the
    /// answer was filed (wave 81); `None` for the instance finders.
    pub(crate) holder: Option<ClassId>,
    pub(crate) holder_initialized: bool,
}

thread_local! {
    /// Per-thread memo of field-finder answers (see the section comment).
    pub(crate) static FINDER_MINT_MEMO: std::cell::RefCell<Vec<Option<FinderMintLine>>> =
        std::cell::RefCell::new(vec![None; FINDER_MINT_MEMO_SLOTS]);
}

/// The memo key of a finder call (`args` = the `Lookup`, `refc`, `name`,
/// `type`), or `None` when an argument is null or the `Lookup`'s words do not
/// read as expected. Pure reads; no GC point.
pub(crate) fn finder_mint_key(
    ctx: &dyn NativeContext,
    finder: FieldFinder,
    args: &[Value],
) -> Option<FinderMintKey> {
    let (
        Some(Value::Object(Some(lookup))),
        Some(Value::Object(Some(refc))),
        Some(Value::Object(Some(name))),
        Some(Value::Object(Some(ty))),
    ) = (args.first(), args.get(1), args.get(2), args.get(3))
    else {
        return None;
    };
    let lookup_class = match ctx.get_field_by_name(*lookup, "lookupClass") {
        Value::Object(Some(c)) => c.as_ptr() as usize,
        _ => return None,
    };
    let prev_lookup_class = match ctx.get_field_by_name(*lookup, "prevLookupClass") {
        Value::Object(Some(c)) => c.as_ptr() as usize,
        Value::Object(None) => 0,
        _ => return None,
    };
    let modes = match ctx.get_field_by_name(*lookup, "allowedModes") {
        Value::Int(m) => m,
        _ => return None,
    };
    Some(FinderMintKey {
        finder: finder.tag(),
        lookup_class,
        prev_lookup_class,
        modes,
        refc: refc.as_ptr() as usize,
        name: name.as_ptr() as usize,
        ty: ty.as_ptr() as usize,
    })
}

/// The memo line of `key` (a mixed hash: the addresses are aligned and close).
pub(crate) fn finder_mint_slot(key: &FinderMintKey) -> usize {
    let mut h = (key.refc as u64)
        ^ (key.name as u64).rotate_left(17)
        ^ (key.ty as u64).rotate_left(31)
        ^ (key.lookup_class as u64).rotate_left(43)
        ^ (key.prev_lookup_class as u64).rotate_left(7)
        ^ u64::from(key.modes as u32).rotate_left(53)
        ^ u64::from(key.finder);
    // splitmix64's finalizer, as `mt_intern_hash`.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((h ^ (h >> 31)) as usize) & (FINDER_MINT_MEMO_SLOTS - 1)
}

/// File `handle`, the body's answer to `key`, when it is exactly the
/// finder's accessor class holding a `MemberName` (and, for a static finder,
/// a field whose declaring class is `predicted_holder`, the holder read before
/// the body, still in the initialization state it had then). The caller has
/// checked that no collection ran since the key was read.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finder_mint_file(
    ctx: &mut dyn NativeContext,
    vm: usize,
    epoch: u64,
    key: FinderMintKey,
    finder: FieldFinder,
    handle: ObjectRef,
    predicted_holder: Option<ClassId>,
    holder_initialized_before: bool,
) {
    let accessor_class = ctx.class_id_of_object(handle);
    if ctx.class_name_arc_of_id(accessor_class).as_deref() != Some(finder.accessor_class()) {
        return;
    }
    let Value::Object(Some(member)) = ctx.get_field_by_name(handle, "member") else {
        return;
    };
    let member_class = ctx.class_id_of_object(member);
    if ctx.class_name_arc_of_id(member_class).as_deref() != Some("java/lang/invoke/MemberName") {
        return;
    }
    let mut holder = None;
    if finder.is_static() {
        let Value::Object(Some(clazz)) = ctx.get_field_by_name(member, "clazz") else {
            return;
        };
        holder = ctx.class_id_from_mirror(clazz);
        // The holder the body resolved is the one read before it, and its
        // initialization did not change while the body ran (wave 81).
        let admitted = holder.is_some()
            && holder == predicted_holder
            && holder.is_some_and(|h| ctx.is_class_initialized(h) == holder_initialized_before);
        if !admitted {
            return;
        }
    }
    let form = ctx.get_field_by_name(handle, "form");
    if !matches!(form, Value::Object(Some(_))) {
        return;
    }
    let line = FinderMintLine {
        vm,
        epoch,
        key,
        proto: handle,
        proto_member: member,
        accessor_class,
        member_class,
        form,
        holder,
        holder_initialized: holder_initialized_before,
    };
    let slot = finder_mint_slot(&key);
    FINDER_MINT_MEMO.with(|m| m.borrow_mut()[slot] = Some(line));
}

/// A fresh accessor and `MemberName`, word for word `line`'s. `None` when an
/// allocation fails (the caller then runs the body, which raises it).
pub(crate) fn finder_mint(
    ctx: &mut dyn NativeContext,
    line: &FinderMintLine,
    finder: FieldFinder,
) -> Option<MethodCallResult> {
    let words = if finder.is_static() {
        FINDER_MINT_STATIC_ACCESSOR_WORDS
    } else {
        FINDER_MINT_ACCESSOR_WORDS
    };
    let mut scope = NativeHandleScope::new(ctx);
    let proto_h = scope.root(line.proto);
    let proto_member_h = scope.root(line.proto_member);
    // Wave 82 (lane L4): the filed form is rooted too. It is unmoved within
    // the epoch, but either allocation below may collect, and the form was
    // read back after them from the line's raw address.
    let form_h = match line.form {
        Value::Object(Some(form)) => scope.root(form),
        _ => return None,
    };
    let member = match scope.instantiate(
        line.member_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let member_h = scope.root(member);
    let proto_member = scope.get(&proto_member_h);
    let member = scope.get(&member_h);
    for word in FINDER_MINT_MEMBER_WORDS {
        let value = scope.get_field_by_name(proto_member, word);
        scope.set_field_by_name(member, word, value);
    }
    let fresh = match scope.instantiate(
        line.accessor_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let fresh_h = scope.root(fresh);
    let proto = scope.get(&proto_h);
    let member = scope.get(&member_h);
    let fresh = scope.get(&fresh_h);
    let form = Value::Object(Some(scope.get(&form_h)));
    for word in words {
        // Wave 81: the form the body answered, not the filed handle's current
        // one (see the section comment): a static form of `ACCESSOR_FORMS`.
        let value = if *word == "form" {
            form
        } else {
            scope.get_field_by_name(proto, word)
        };
        scope.set_field_by_name(fresh, word, value);
    }
    scope.set_field_by_name(fresh, "member", Value::Object(Some(member)));
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

/// A field finder under `CRATONVM_MH_FINDER_MINT_MEMO`: a minted repeat
/// answer, else the JDK's own body (its answer filed when admissible).
pub(crate) fn mh_finder_mint_intrinsic(
    ctx: &mut dyn NativeContext,
    finder: FieldFinder,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(lookup))) = args.first() else {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message: None },
        )));
    };
    let lookup = *lookup;
    // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4): reads only.
    use crate::mh_finder_census as census;
    let census_on = census::enabled();
    let census_all = census::phase_start();
    census::event("field-finder.entry");
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let key = if vm != 0 && !crate::nbflags().dbg_mh_dispatch {
        finder_mint_key(&*ctx, finder, args)
    } else {
        None
    };
    if key.is_none() {
        census::event("field-finder.key-refused");
    }
    if let Some(key) = &key {
        let slot = finder_mint_slot(key);
        let line = FINDER_MINT_MEMO.with(|m| m.borrow()[slot]);
        if census_on {
            census::event(match &line {
                None => "field-finder.line.empty-slot",
                Some(l) if l.vm != vm => "field-finder.line.other-vm",
                Some(l) if l.epoch != epoch => "field-finder.line.other-epoch",
                Some(l) if l.key != *key => "field-finder.line.other-key",
                Some(l)
                    if !l
                        .holder
                        .is_none_or(|h| ctx.is_class_initialized(h) == l.holder_initialized) =>
                {
                    "field-finder.line.holder-state-changed"
                }
                Some(_) => "field-finder.line.hit",
            });
        }
        // Wave 81: a static finder's line holds while its holder's
        // initialization state is the filed one.
        let line = line.filter(|l| {
            l.vm == vm
                && l.epoch == epoch
                && l.key == *key
                && l.holder
                    .is_none_or(|h| ctx.is_class_initialized(h) == l.holder_initialized)
        });
        if let Some(line) = line {
            let census_mint = census::phase_start();
            let minted = finder_mint(ctx, &line, finder);
            census::phase_end(census::Phase::FieldFinderMint, census_mint);
            if let Some(answer) = minted {
                census::phase_end(census::Phase::FieldFinder, census_all);
                return answer;
            }
            census::event("field-finder.line.hit-but-mint-allocation-failed");
        }
    }
    // A static finder files a field whose declaring class -- `refc`, or the
    // superclass / interface it inherits the field from -- is the one read
    // here, in the initialization state read here (see the section comment).
    let (predicted_holder, holder_initialized_before) = if finder.is_static() && key.is_some() {
        finder_mint_static_holder(&*ctx, args)
    } else {
        (None, false)
    };
    let census_body = census::phase_start();
    census::body_enter();
    let answer = ctx.invoke_virtual_bytecode_only(
        lookup,
        finder.method_name(),
        FINDER_MINT_DESC,
        &args[1..],
    );
    census::body_exit();
    census::phase_end(census::Phase::FieldFinderBody, census_body);
    if census_on && answer.is_err() {
        census::event("field-finder.body.threw");
    }
    if let (Some(key), Ok(Some(Value::Object(Some(handle))))) = (key, &answer) {
        // The key's addresses were read before the body ran (a GC point):
        // filed only when no collection ran since.
        let collected = ctx.gc_address_epoch() != epoch;
        if collected {
            census::event("field-finder.file-refused.collection-during-body");
        }
        if !collected {
            let handle = *handle;
            finder_mint_file(
                ctx,
                vm,
                epoch,
                key,
                finder,
                handle,
                predicted_holder,
                holder_initialized_before,
            );
        }
    }
    census::phase_end(census::Phase::FieldFinder, census_all);
    answer
}

/// The class a static finder's field resolves to (`refc` itself, or the
/// superclass / superinterface it inherits a static field from:
/// [`inherited_static_field_holder`]) and whether that class is initialized
/// now; `(None, false)` when `refc` or `name` cannot be read. Reads only, no
/// GC point (interpreter round i3 wave 81, lane L4).
pub(crate) fn finder_mint_static_holder(ctx: &dyn NativeContext, args: &[Value]) -> (Option<ClassId>, bool) {
    let Some(Value::Object(Some(refc))) = args.get(1) else {
        return (None, false);
    };
    let Some(refc_class) = ctx.class_id_from_mirror(*refc) else {
        return (None, false);
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(name) = name else {
        return (None, false);
    };
    let holder = inherited_static_field_holder(ctx, refc_class, &name).unwrap_or(refc_class);
    (Some(holder), ctx.is_class_initialized(holder))
}

/// `Lookup.findGetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_getter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::Getter, args)
}

/// `Lookup.findSetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_setter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::Setter, args)
}

/// `Lookup.findStaticGetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_static_getter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::StaticGetter, args)
}

/// `Lookup.findStaticSetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_static_setter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::StaticSetter, args)
}

/// Register the four finder `Intrinsic`s when `r`'s VM admits them
/// ([`mh_finder_mint_memo_registers`]); a no-op otherwise. States each
/// registration's kind (`register_with_kind`), so the ambient category is
/// neither read nor changed.
pub(crate) fn register_finder_mint_memo(r: &mut NativeMethodRegistry) {
    if !mh_finder_mint_memo_registers(r) {
        return;
    }
    // Wave 109 (`finder_mint_mode_admits`): a `--compatible` `JdkInvoke`
    // family drop keeps these stated `Intrinsic`s (closed after the loop).
    let admitting = r.set_admitting_family_intrinsic(true);
    for finder in FieldFinder::ALL {
        r.register_with_kind(
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            FINDER_MINT_DESC,
            finder.callback(),
            NativeKind::Intrinsic,
        );
        // Wave 80: a no-op unless `CRATONVM_INTRINSIC_BODY_COMPILES` or the
        // `finder-mint` row (on by default since wave 81).
        r.mark_body_compilable_intrinsic_row(
            FINDER_MINT_BODY_ROW,
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            FINDER_MINT_DESC,
        );
    }
    r.set_admitting_family_intrinsic(admitting);
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 82, lane L4: a repeat `--jdk-only` METHOD finder
// is minted from its first answer (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`,
// default off; proposal
// `i81-L4-proposal-a-repeat-method-finder-is-minted-from-its-first-answer`)
// ---------------------------------------------------------------------------
//
// The field mint above, per method finder. `findVirtual` / `findStatic` /
// `findSpecial` / `findConstructor` / `unreflect` run the JDK's finder
// bytecode on every call (host, wave 81, JIT: ~6.5-9.6 us; `find-getter`
// under the field mint ~1.75 us). Behind the switch, in a `--jdk-only` VM
// over a real JDK -- without a JIT, or with the `method-finder-mint` row of
// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (NOT a default row), which marks the
// five rows body-compilable so the JDK bodies still compile -- the five
// triples are reviewed `Intrinsic`s. The first call with a key runs the JDK's
// own body (`invoke_virtual_bytecode_only` on the `Lookup`) and files its
// answer; a repeat call allocates a fresh handle of the filed class with the
// filed handle's words and a fresh `MemberName` with the filed member's six
// words. HotSpot's finders build a new `MemberName` (`resolveOrFail` clones)
// and a new `DirectMethodHandle` per call, so identity matches.
//
// KEY: the finder, the `Lookup`'s `lookupClass`, `prevLookupClass` and
// `allowedModes` (everything the JDK's access checks read from the lookup,
// so a caller or mode the body would refuse never shares a line with one it
// admits), and the argument objects by address: `refc`, `name`, `type`, and
// `findSpecial`'s `specialCaller`; for `unreflect` the `Method` and its
// `override` bit (`isAccessible` picks `IMPL_LOOKUP`). Only a SUCCESS is
// filed, and access only grows (`addExports` / `addReads` / `addOpens` never
// take one away), so no filed answer is one a later body would refuse.
//
// FILED only when the answer's class is EXACTLY `DirectMethodHandle`,
// `$Special`, `$Interface` or `$Constructor` (the constructor finder only
// `$Constructor`, the others never), its `member` is a `MemberName` (and a
// `$Constructor`'s `initMethod` too), and neither `refc` nor the member's
// class is `MethodHandle` / `VarHandle` (the signature-polymorphic invokers,
// which the JDK caches and shares). That rule is checked on the ANSWER, never
// inferred from the request, and it declines by construction: a
// caller-sensitive method (`maybeBindCaller` wraps it), a varargs method
// (`setVarargs` -> `VarargsCollector`), and every invoker. A protected
// member's receiver restriction and `findSpecial`'s own (`restrictReceiver`)
// ARE filed: JDK 25's `viewAsType` answers a `DirectMethodHandle` (or
// `$Special` / `$Interface`) of the narrowed type with `crackable` false and
// the same `member`, all words the mint copies, and a pure function of the key.
//
// A STATIC method's answer depends on its declaring class's initialization
// (`preparedLambdaForm` picks `LF_INVSTATIC_INIT` while
// `shouldBeInitialized`), as the static field finders' do. The class read
// before the body (`refc` for `findStatic`, the `Method`'s class for a static
// `unreflect`) and its state are kept; after the body the member's holder is
// filed as uninitialized when it still is (initialization is monotonic, so it
// was throughout the body), or as initialized when the class read before was
// already initialized and is the holder or one of its subclasses (a class is
// initialized only after its superclasses, JVMS 5.5). A hit is minted only
// while the holder's state is still the filed one, and takes the `form` the
// body answered at filing time (a later `ensureInitialized` -> `updateForm`
// or customization of the filed handle never leaks into a mint).
//
// Declined (the body runs, nothing filed): a null argument (the body raises
// the NPE), any throw, a collection during the body, and
// `CRATONVM_DBG_MH_DISPATCH`. A debugger breakpoint inside the finder's body
// does not fire on a hit, as for every `Intrinsic`.
//
// The five triples are rows of `RETIRED_SHADOW_SBR4_MHCONS_TRIPLES`, as the
// field finders are: the `Intrinsic` owns the slot only in the VMs the switch
// admits, `--compatible` keeps its carriers (and its own nest-access check,
// `CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS`). `the native-registry census`
// allows exactly these five in the strict-boot gates when the switch (and,
// with a JIT, the row) admits them.

/// The `method-finder-mint` row token of `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`
/// (not in `INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`).
pub(crate) const METHOD_FINDER_MINT_BODY_ROW: &str = "method-finder-mint";
/// Lines of a thread's method finder memo (a power of two, direct-mapped).
pub(crate) const METHOD_FINDER_MINT_MEMO_SLOTS: usize = 64;
/// `findVirtual` / `findStatic`.
pub(crate) const METHOD_FINDER_DESC_NAMED: &str =
    "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// `findSpecial`.
pub(crate) const METHOD_FINDER_DESC_SPECIAL: &str = "(Ljava/lang/Class;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";
/// `findConstructor`.
pub(crate) const METHOD_FINDER_DESC_CONSTRUCTOR: &str =
    "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// `unreflect`.
pub(crate) const METHOD_FINDER_DESC_UNREFLECT: &str =
    "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;";

/// `CRATONVM_MH_METHOD_FINDER_MINT_MEMO` -- performance, default OFF,
/// `truthy_word`. Read at registration; see the section comment above.
pub(crate) fn mh_method_finder_mint_memo_enabled() -> bool {
    crate::nbflags().mh_method_finder_mint_memo
}

/// Does `r`'s VM get the five method finder `Intrinsic`s? The switch,
/// `--jdk-only` over a real JDK, and no JIT -- or a JIT VM under
/// `CRATONVM_INTRINSIC_BODY_COMPILES` or the `method-finder-mint` row, where
/// [`register_method_finder_mint_memo`] marks the rows body-compilable.
pub(crate) fn mh_method_finder_mint_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_method_finder_mint_memo_enabled()
        && finder_mint_mode_admits(r)
        && r.interpreted_intrinsic_registers_row(METHOD_FINDER_MINT_BODY_ROW)
}

/// One of the five `Lookup` method finders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MethodFinder {
    Virtual,
    Static,
    Special,
    Constructor,
    Unreflect,
}

impl MethodFinder {
    pub(crate) const ALL: [MethodFinder; 5] = [
        MethodFinder::Virtual,
        MethodFinder::Static,
        MethodFinder::Special,
        MethodFinder::Constructor,
        MethodFinder::Unreflect,
    ];

    pub(crate) fn method_name(self) -> &'static str {
        match self {
            MethodFinder::Virtual => "findVirtual",
            MethodFinder::Static => "findStatic",
            MethodFinder::Special => "findSpecial",
            MethodFinder::Constructor => "findConstructor",
            MethodFinder::Unreflect => "unreflect",
        }
    }

    pub(crate) fn descriptor(self) -> &'static str {
        match self {
            MethodFinder::Virtual | MethodFinder::Static => METHOD_FINDER_DESC_NAMED,
            MethodFinder::Special => METHOD_FINDER_DESC_SPECIAL,
            MethodFinder::Constructor => METHOD_FINDER_DESC_CONSTRUCTOR,
            MethodFinder::Unreflect => METHOD_FINDER_DESC_UNREFLECT,
        }
    }

    /// Distinct from the field finders' tags (1-4).
    pub(crate) fn tag(self) -> u8 {
        match self {
            MethodFinder::Virtual => 11,
            MethodFinder::Static => 12,
            MethodFinder::Special => 13,
            MethodFinder::Constructor => 14,
            MethodFinder::Unreflect => 15,
        }
    }

    pub(crate) fn callback(self) -> fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult {
        match self {
            MethodFinder::Virtual => mh_find_virtual_mint,
            MethodFinder::Static => mh_find_static_mint,
            MethodFinder::Special => mh_find_special_mint,
            MethodFinder::Constructor => mh_find_constructor_mint,
            MethodFinder::Unreflect => mh_unreflect_mint,
        }
    }
}

/// The exact class of a fileable answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DirectSpecies {
    Plain,
    Special,
    Interface,
    Constructor,
}

impl DirectSpecies {
    pub(crate) fn of_class_name(name: &str) -> Option<DirectSpecies> {
        match name {
            "java/lang/invoke/DirectMethodHandle" => Some(DirectSpecies::Plain),
            "java/lang/invoke/DirectMethodHandle$Special" => Some(DirectSpecies::Special),
            "java/lang/invoke/DirectMethodHandle$Interface" => Some(DirectSpecies::Interface),
            "java/lang/invoke/DirectMethodHandle$Constructor" => Some(DirectSpecies::Constructor),
            _ => None,
        }
    }

    /// The words a mint copies from the filed handle (JDK 25 layouts; `form`
    /// is the filed one, `member` / `initMethod` are fresh `MemberName`s, and
    /// `asTypeCache`, `asTypeSoftCache`, `customizationCount` and
    /// `updateInProgress` keep a fresh handle's defaults).
    pub(crate) fn words(self) -> &'static [&'static str] {
        const PLAIN: &[&str] = &["type", "form", "crackable"];
        const SPECIAL: &[&str] = &["type", "form", "crackable", "caller"];
        const INTERFACE: &[&str] = &["type", "form", "crackable", "refc"];
        const CONSTRUCTOR: &[&str] = &["type", "form", "crackable", "instanceClass"];
        match self {
            DirectSpecies::Plain => PLAIN,
            DirectSpecies::Special => SPECIAL,
            DirectSpecies::Interface => INTERFACE,
            DirectSpecies::Constructor => CONSTRUCTOR,
        }
    }
}

/// A method finder call's memo key: the finder, the `Lookup`'s three access
/// words, and up to four argument words (see the section comment).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MethodFinderMintKey {
    pub(crate) finder: u8,
    pub(crate) lookup_class: usize,
    pub(crate) prev_lookup_class: usize,
    pub(crate) modes: i32,
    pub(crate) a: usize,
    pub(crate) b: usize,
    pub(crate) c: usize,
    pub(crate) d: usize,
}

/// One filed answer, valid for VM `vm` in collection epoch `epoch` only.
#[derive(Clone, Copy)]
pub(crate) struct MethodFinderMintLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) key: MethodFinderMintKey,
    pub(crate) proto: ObjectRef,
    pub(crate) proto_member: ObjectRef,
    /// A `$Constructor`'s `initMethod`.
    pub(crate) proto_init: Option<ObjectRef>,
    pub(crate) handle_class: ClassId,
    pub(crate) member_class: ClassId,
    pub(crate) species: DirectSpecies,
    /// The body's `form` at filing time.
    pub(crate) form: Value,
    /// A static method's holder and whether it was initialized when filed.
    pub(crate) holder: Option<ClassId>,
    pub(crate) holder_initialized: bool,
}

thread_local! {
    /// Per-thread memo of method-finder answers (see the section comment).
    pub(crate) static METHOD_FINDER_MINT_MEMO: std::cell::RefCell<Vec<Option<MethodFinderMintLine>>> =
        std::cell::RefCell::new(vec![None; METHOD_FINDER_MINT_MEMO_SLOTS]);
}

/// The memo key of a method finder call (`args` = the `Lookup`, then the
/// finder's own arguments), or the reason there is none (an argument is null
/// or a word does not read as expected; the reason is counted under
/// `CRATONVM_DBG=mh-finder-census`). Pure reads; no GC point.
pub(crate) fn method_finder_mint_key(
    ctx: &dyn NativeContext,
    finder: MethodFinder,
    args: &[Value],
) -> Result<MethodFinderMintKey, &'static str> {
    let Some(Value::Object(Some(lookup))) = args.first() else {
        return Err("method-finder.key-refused.null-lookup");
    };
    let lookup_class = match ctx.get_field_by_name(*lookup, "lookupClass") {
        Value::Object(Some(c)) => c.as_ptr() as usize,
        _ => return Err("method-finder.key-refused.lookup-class-word"),
    };
    let prev_lookup_class = match ctx.get_field_by_name(*lookup, "prevLookupClass") {
        Value::Object(Some(c)) => c.as_ptr() as usize,
        Value::Object(None) => 0,
        _ => return Err("method-finder.key-refused.prev-lookup-class-word"),
    };
    let modes = match ctx.get_field_by_name(*lookup, "allowedModes") {
        Value::Int(m) => m,
        _ => return Err("method-finder.key-refused.allowed-modes-word"),
    };
    let obj = |i: usize| -> Result<usize, &'static str> {
        match args.get(i) {
            Some(Value::Object(Some(o))) => Ok(o.as_ptr() as usize),
            _ => Err("method-finder.key-refused.null-argument"),
        }
    };
    let (a, b, c, d) = match finder {
        MethodFinder::Virtual | MethodFinder::Static => (obj(1)?, obj(2)?, obj(3)?, 0),
        MethodFinder::Special => (obj(1)?, obj(2)?, obj(3)?, obj(4)?),
        MethodFinder::Constructor => (obj(1)?, 0, obj(2)?, 0),
        MethodFinder::Unreflect => {
            let Some(Value::Object(Some(method))) = args.get(1) else {
                return Err("method-finder.key-refused.null-argument");
            };
            let accessible = match ctx.get_field_by_name(*method, "override") {
                Value::Int(v) => usize::from(v != 0),
                _ => return Err("method-finder.key-refused.override-word"),
            };
            (method.as_ptr() as usize, accessible, 0, 0)
        }
    };
    Ok(MethodFinderMintKey {
        finder: finder.tag(),
        lookup_class,
        prev_lookup_class,
        modes,
        a,
        b,
        c,
        d,
    })
}

/// The memo line of `key` (a mixed hash, as [`finder_mint_slot`]).
pub(crate) fn method_finder_mint_slot(key: &MethodFinderMintKey) -> usize {
    let mut h = (key.a as u64)
        ^ (key.b as u64).rotate_left(17)
        ^ (key.c as u64).rotate_left(31)
        ^ (key.d as u64).rotate_left(23)
        ^ (key.lookup_class as u64).rotate_left(43)
        ^ (key.prev_lookup_class as u64).rotate_left(7)
        ^ u64::from(key.modes as u32).rotate_left(53)
        ^ u64::from(key.finder);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((h ^ (h >> 31)) as usize) & (METHOD_FINDER_MINT_MEMO_SLOTS - 1)
}

/// The class whose initialization a STATIC method's answer may depend on, read
/// before the body, and its state then: `refc` for `findStatic`, the
/// `Method`'s declaring class for an `unreflect` of a static method; `None`
/// for every other call. Reads only, no GC point.
pub(crate) fn method_finder_mint_init_probe(
    ctx: &dyn NativeContext,
    finder: MethodFinder,
    args: &[Value],
) -> Option<(ClassId, bool)> {
    let mirror = match finder {
        MethodFinder::Static => match args.get(1) {
            Some(Value::Object(Some(refc))) => *refc,
            _ => return None,
        },
        MethodFinder::Unreflect => {
            let Some(Value::Object(Some(method))) = args.get(1) else {
                return None;
            };
            let is_static = matches!(ctx.get_field_by_name(*method, "modifiers"), Value::Int(m) if m & 0x0008 != 0);
            if !is_static {
                return None;
            }
            match ctx.get_field_by_name(*method, "clazz") {
                Value::Object(Some(c)) => c,
                _ => return None,
            }
        }
        _ => return None,
    };
    let class = ctx.class_id_from_mirror(mirror)?;
    Some((class, ctx.is_class_initialized(class)))
}

/// Is `target` `from` or one of its superclasses? Bounded walk; reads only.
pub(crate) fn method_finder_mint_is_self_or_super(
    ctx: &dyn NativeContext,
    from: ClassId,
    target: ClassId,
) -> bool {
    let mut cur = Some(from);
    for _ in 0..64 {
        match cur {
            Some(c) if c == target => return true,
            Some(c) => cur = ctx.superclass_of(c),
            None => return false,
        }
    }
    false
}

/// Is `name` one of the two signature-polymorphic classes whose finder
/// answers are shared, cached invokers?
pub(crate) fn method_finder_mint_is_polymorphic_class(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("java/lang/invoke/MethodHandle") | Some("java/lang/invoke/VarHandle")
    )
}

/// File `handle`, the body's answer to `key`, when it is admissible (see the
/// section comment). The caller has checked that no collection ran since the
/// key was read. Answers what happened, as a `CRATONVM_DBG=mh-finder-census`
/// event label: `method-finder.file.filed`, or the refusal's reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn method_finder_mint_file(
    ctx: &mut dyn NativeContext,
    vm: usize,
    epoch: u64,
    key: MethodFinderMintKey,
    finder: MethodFinder,
    args: &[Value],
    handle: ObjectRef,
    probe: Option<(ClassId, bool)>,
) -> &'static str {
    let handle_class = ctx.class_id_of_object(handle);
    let handle_class_name = ctx.class_name_arc_of_id(handle_class);
    let Some(species) = handle_class_name
        .as_deref()
        .and_then(DirectSpecies::of_class_name)
    else {
        if crate::mh_finder_census::enabled() {
            crate::mh_finder_census::event_named(
                "method-finder.file-refused.not-direct-species-name",
                handle_class_name.as_deref().unwrap_or("?"),
            );
        }
        return "method-finder.file-refused.not-direct-species";
    };
    if (finder == MethodFinder::Constructor) != (species == DirectSpecies::Constructor) {
        return "method-finder.file-refused.constructor-species-mismatch";
    }
    if finder != MethodFinder::Unreflect {
        let Some(Value::Object(Some(refc))) = args.get(1) else {
            return "method-finder.file-refused.null-refc";
        };
        let refc_name = ctx
            .class_id_from_mirror(*refc)
            .and_then(|c| ctx.class_name_arc_of_id(c));
        if method_finder_mint_is_polymorphic_class(refc_name.as_deref()) {
            return "method-finder.file-refused.polymorphic-refc";
        }
    }
    let Value::Object(Some(member)) = ctx.get_field_by_name(handle, "member") else {
        return "method-finder.file-refused.no-member";
    };
    let member_class = ctx.class_id_of_object(member);
    if ctx.class_name_arc_of_id(member_class).as_deref() != Some("java/lang/invoke/MemberName") {
        return "method-finder.file-refused.member-not-membername";
    }
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(member, "clazz") else {
        return "method-finder.file-refused.member-no-clazz";
    };
    let Some(member_holder) = ctx.class_id_from_mirror(clazz) else {
        return "method-finder.file-refused.member-clazz-no-class";
    };
    if method_finder_mint_is_polymorphic_class(ctx.class_name_arc_of_id(member_holder).as_deref()) {
        return "method-finder.file-refused.polymorphic-holder";
    }
    let proto_init = if species == DirectSpecies::Constructor {
        let Value::Object(Some(init)) = ctx.get_field_by_name(handle, "initMethod") else {
            return "method-finder.file-refused.no-init-method";
        };
        if ctx.class_id_of_object(init) != member_class {
            return "method-finder.file-refused.init-method-class";
        }
        Some(init)
    } else {
        None
    };
    let member_static =
        matches!(ctx.get_field_by_name(member, "flags"), Value::Int(f) if f & 0x0008 != 0);
    let (holder, holder_initialized) = if member_static && species != DirectSpecies::Constructor {
        // A static answer needs the state read before the body.
        let Some((before_class, before_initialized)) = probe else {
            return "method-finder.file-refused.static-without-probe";
        };
        if !ctx.is_class_initialized(member_holder) {
            (Some(member_holder), false)
        } else if before_initialized
            && method_finder_mint_is_self_or_super(&*ctx, before_class, member_holder)
        {
            (Some(member_holder), true)
        } else {
            return "method-finder.file-refused.static-holder-initialized-during-body";
        }
    } else {
        (None, false)
    };
    let form = ctx.get_field_by_name(handle, "form");
    if !matches!(form, Value::Object(Some(_))) {
        return "method-finder.file-refused.null-form";
    }
    let line = MethodFinderMintLine {
        vm,
        epoch,
        key,
        proto: handle,
        proto_member: member,
        proto_init,
        handle_class,
        member_class,
        species,
        form,
        holder,
        holder_initialized,
    };
    // Interpreter round i5 wave 108, lane L4 (`CRATONVM_MH_METHOD_FINDER_SHAPE_MINT`,
    // default off): the same answer is also the shape line of its (finder,
    // lookup, name, type), from which a later class of the same shape is minted.
    if crate::nbflags().mh_method_finder_shape_mint {
        let outcome = method_finder_shape_file(&*ctx, &line, finder, args, member_holder);
        crate::mh_finder_census::event(outcome);
    }
    let slot = method_finder_mint_slot(&key);
    let evicted = METHOD_FINDER_MINT_MEMO.with(|m| m.borrow_mut()[slot].replace(line));
    match evicted {
        Some(old) if old.key != key => "method-finder.file.filed-evicting-another-key",
        Some(_) => "method-finder.file.filed-replacing-same-key",
        None => "method-finder.file.filed-into-empty-slot",
    }
}

/// The `LambdaForm.Kind` constant name of `form` (`kind.name`), when it reads
/// as one. Reads only, no GC point.
pub(crate) fn method_finder_form_kind_name(ctx: &dyn NativeContext, form: ObjectRef) -> Option<String> {
    let Value::Object(Some(kind)) = ctx.get_field_by_name(form, "kind") else {
        return None;
    };
    match ctx.get_field_by_name(kind, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// `CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE` (interpreter round i4 wave 90,
/// lane L4; default off): the re-filing of a STATIC method's line whose holder
/// was uninitialized when it was filed and is initialized now.
///
/// Such a line is refused by the holder-state filter (correctly: the JDK's
/// answer now differs). The JDK 25 answer for an initialized holder is the
/// filed answer with ONE word changed: `DirectMethodHandle.make` builds the
/// same plain `DirectMethodHandle` (same `type`, `member`, `crackable`), and
/// only `preparedLambdaForm` picks `LF_INVSTATIC` instead of
/// `LF_INVSTATIC_INIT` once `shouldBeInitialized` is false -- the barrier-free
/// form it had already precomputed and cached for the basic type. That exact
/// form is what the JDK's own `ensureInitialized` writes into the filed handle
/// (`updateForm(preparedLambdaForm(member))`, once `checkInitialized` saw the
/// holder fully initialized) on the handle's first call. So the line is
/// re-filed with the handle's CURRENT form, and only when that form is
/// provably that one:
///
/// * the filed form's kind is `DIRECT_INVOKE_STATIC_INIT` (the barrier form);
/// * the handle's current form is another object, uncustomized
///   (`customized == null`: `MethodHandle.customize` is the only other writer
///   of a direct handle's `form`), of kind `DIRECT_INVOKE_STATIC`.
///
/// A handle never invoked still holds the barrier form: declined, the body
/// runs and files the initialized answer itself (as with the switch off).
/// `Ok(None)` when the line needs no re-filing; `Err(reason)` (a
/// `CRATONVM_DBG=mh-finder-census` label) when it would but cannot be.
/// Reads only, no GC point: the caller has checked the line's VM, epoch and
/// key.
pub(crate) fn method_finder_mint_init_refile(
    ctx: &dyn NativeContext,
    line: &MethodFinderMintLine,
) -> Result<Option<MethodFinderMintLine>, &'static str> {
    let Some(holder) = line.holder else {
        return Ok(None);
    };
    if line.holder_initialized || !ctx.is_class_initialized(holder) {
        return Ok(None);
    }
    if line.species != DirectSpecies::Plain {
        return Err("method-finder.line.init-refile.declined.species");
    }
    let Value::Object(Some(filed_form)) = line.form else {
        return Err("method-finder.line.init-refile.declined.filed-form");
    };
    if method_finder_form_kind_name(ctx, filed_form).as_deref() != Some("DIRECT_INVOKE_STATIC_INIT")
    {
        return Err("method-finder.line.init-refile.declined.filed-form-kind");
    }
    let Value::Object(Some(current)) = ctx.get_field_by_name(line.proto, "form") else {
        return Err("method-finder.line.init-refile.declined.no-current-form");
    };
    if current.as_ptr() == filed_form.as_ptr() {
        return Err("method-finder.line.init-refile.declined.handle-never-invoked");
    }
    if !matches!(
        ctx.get_field_by_name(current, "customized"),
        Value::Object(None)
    ) {
        return Err("method-finder.line.init-refile.declined.customized");
    }
    if method_finder_form_kind_name(ctx, current).as_deref() != Some("DIRECT_INVOKE_STATIC") {
        return Err("method-finder.line.init-refile.declined.current-form-kind");
    }
    Ok(Some(MethodFinderMintLine {
        holder_initialized: true,
        form: Value::Object(Some(current)),
        ..*line
    }))
}

/// A fresh `MemberName`, word for word the rooted `proto`'s, rooted in `scope`.
/// `None` when the allocation fails.
pub(crate) fn method_finder_mint_member(
    scope: &mut NativeHandleScope<'_>,
    member_class: ClassId,
    proto: &cratonvm_native_api::NativeHandle,
) -> Option<cratonvm_native_api::NativeHandle> {
    let member = match scope.instantiate(
        member_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let member_h = scope.root(member);
    let proto = scope.get(proto);
    let member = scope.get(&member_h);
    for word in FINDER_MINT_MEMBER_WORDS {
        let value = scope.get_field_by_name(proto, word);
        scope.set_field_by_name(member, word, value);
    }
    Some(member_h)
}

/// A fresh handle of `line`'s class and fresh `MemberName`s, word for word
/// the filed ones. `None` when an allocation fails (the caller then runs the
/// body, which raises it).
pub(crate) fn method_finder_mint(
    ctx: &mut dyn NativeContext,
    line: &MethodFinderMintLine,
) -> Option<MethodCallResult> {
    let mut scope = NativeHandleScope::new(ctx);
    let proto_h = scope.root(line.proto);
    let proto_member_h = scope.root(line.proto_member);
    let proto_init_h = line.proto_init.map(|o| scope.root(o));
    // Rooted: the allocations below may collect.
    let form_h = match line.form {
        Value::Object(Some(form)) => scope.root(form),
        _ => return None,
    };
    let member_h = method_finder_mint_member(&mut scope, line.member_class, &proto_member_h)?;
    let init_h = match &proto_init_h {
        Some(h) => Some(method_finder_mint_member(&mut scope, line.member_class, h)?),
        None => None,
    };
    let fresh = match scope.instantiate(
        line.handle_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        _ => return None,
    };
    let fresh_h = scope.root(fresh);
    let proto = scope.get(&proto_h);
    let fresh = scope.get(&fresh_h);
    let form = Value::Object(Some(scope.get(&form_h)));
    for word in line.species.words() {
        // The form the body answered at filing time (see the section comment).
        let value = if *word == "form" {
            form
        } else {
            scope.get_field_by_name(proto, word)
        };
        scope.set_field_by_name(fresh, word, value);
    }
    let member = scope.get(&member_h);
    scope.set_field_by_name(fresh, "member", Value::Object(Some(member)));
    if let Some(h) = &init_h {
        let init = scope.get(h);
        scope.set_field_by_name(fresh, "initMethod", Value::Object(Some(init)));
    }
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

// ---------------------------------------------------------------------------
// `CRATONVM_MH_METHOD_FINDER_SHAPE_MINT` (interpreter round i5 wave 108, lane
// L4; default off, `truthy_word`; page
// `i89-L4-method-finder-per-class-and-per-handle-cost`).
//
// The method finder memo above keys on `refc` by address, so a `findStatic` /
// `findConstructor` of a class no finder answered before always runs the JDK
// body (~35-80 us on a batch-defined class, ~360 us right after its define,
// against HotSpot's 7-15 us), although the answer differs from the one filed
// for another class of the same shape only in `refc`. This switch keeps, per
// thread, the last filed line of each SHAPE -- the key with `refc` zeroed: the
// finder, the `Lookup`'s three access words, the name and type objects -- and
// mints class B's answer from class A's line when every input of the JDK
// body's Java-visible verdicts is provably the same for both:
//
// * `Lookup.checkSymbolicClass` -> `VerifyAccess.isClassAccessible` reads the
//   lookup words (in the key), `refc`'s raw access flags
//   (`Reflection.getClassAccessFlags`), its module and its package against the
//   lookup class's. B has A's defining loader (a user or application loader,
//   never the boot or platform one, so no `@CallerSensitive` or `@Hidden`
//   member), A's package name (so the same runtime package and module), A's
//   raw access flags and A's own `InnerClasses` flags; neither is hidden or
//   an array.
// * `MethodHandleNatives.resolve` finds the member each class DECLARES itself
//   (A's filed member names A as `clazz`; B's own declaration is asked with
//   the filed name and exact descriptor) with the same access flags, so the
//   same static-ness checks and the same `flags` word (the kind and
//   reference-kind bits are the filed member's; no caller-sensitive bit).
// * `checkAccess` -> `isMemberAccessible(refc, defc = refc, mods, ...)`: a
//   PUBLIC or package-private member (never private: nestmates differ per
//   class; never protected: the subclass test and `restrictProtectedReceiver`
//   read the class), judged against the same lookup class and package.
// * `DirectMethodHandle.make`: the same species and `crackable`; the form is
//   per basic type, and for `findStatic` per initialization state, which must
//   equal the filed line's (`preparedLambdaForm`'s `shouldBeInitialized`). A
//   constructor's type is `type().changeReturnType(B)`, interned by the same
//   JDK method the body calls (`makeAllocator`), and its `instanceClass` is B.
//
// The members are the filed ones word for word with `clazz` = B; they must
// carry no `method` (`ResolvedMethodName`) and a null `resolution` (resolved),
// or the shape is not filed. The mint is then filed as B's own line, so B's
// repeat is a plain hit. Declined (the body runs): any guard, another VM or
// collection epoch, a shape filed for B itself. Only `findStatic` and
// `findConstructor` (a virtual answer's type and form read the receiver
// class). A JDK body that would throw for B cannot be reached: every input it
// would throw on is one of the guards above.
//
// Census (`CRATONVM_DBG=mh-finder-census`): `method-finder.shape.filed`,
// `method-finder.shape.not-filed.<reason>`, `method-finder.shape.served`,
// `method-finder.shape.declined.<reason>`.

/// Lines of a thread's shape memo (a power of two, direct-mapped).
pub(crate) const METHOD_FINDER_SHAPE_SLOTS: usize = 16;

/// One filed shape: the line of the class that filed it (`line.key.a` is its
/// `refc` mirror), that class, and what its member resolved to.
#[derive(Clone)]
pub(crate) struct MethodFinderShapeLine {
    pub(crate) line: MethodFinderMintLine,
    pub(crate) refc: ClassId,
    pub(crate) name: std::sync::Arc<str>,
    pub(crate) descriptor: std::sync::Arc<str>,
    /// The member's declared access flags.
    pub(crate) access: u16,
}

thread_local! {
    /// Per-thread shape memo (see the section comment).
    pub(crate) static METHOD_FINDER_SHAPE_MEMO: std::cell::RefCell<Vec<Option<MethodFinderShapeLine>>> =
        std::cell::RefCell::new(vec![None; METHOD_FINDER_SHAPE_SLOTS]);
}

/// `key` with its `refc` word zeroed: the shape.
pub(crate) fn method_finder_shape_key(key: &MethodFinderMintKey) -> MethodFinderMintKey {
    MethodFinderMintKey { a: 0, ..*key }
}

pub(crate) fn method_finder_shape_slot(shape: &MethodFinderMintKey) -> usize {
    method_finder_mint_slot(shape) & (METHOD_FINDER_SHAPE_SLOTS - 1)
}

/// The class facts the shape guards compare: the defining loader (definite,
/// a user or application loader), the package name, the raw access flags and
/// the class's own `InnerClasses` flags. `None` when the class may not take
/// part (unknown loader, boot or platform loader, hidden, array, no name).
pub(crate) fn method_finder_shape_class_facts(
    ctx: &dyn NativeContext,
    class: ClassId,
) -> Option<(i32, std::sync::Arc<str>, u16, Option<u16>)> {
    let loader = ctx.loader_id_of_class_definite(class)?;
    if loader < 2 || ctx.is_class_hidden(class) {
        return None;
    }
    let name = ctx.class_name_arc_of_id(class)?;
    if name.is_empty() || name.starts_with('[') {
        return None;
    }
    let package: std::sync::Arc<str> = match name.rfind('/') {
        Some(i) => std::sync::Arc::from(&name[..i]),
        None => std::sync::Arc::from(""),
    };
    let own_inner = ctx
        .inner_classes(class)
        .into_iter()
        .find(|ic| *ic.0 == *name || ic.0.replace('.', "/") == *name)
        .map(|ic| ic.3);
    Some((loader, package, ctx.class_access_flags(class), own_inner))
}

/// `ACC_PRIVATE | ACC_PROTECTED`.
pub(crate) const SHAPE_ACC_PRIVATE_OR_PROTECTED: u16 = 0x0002 | 0x0004;

/// File `line` (just built by [`method_finder_mint_file`]) as its shape's
/// line, when it may serve another class (see the section comment). Answers a
/// census label. Reads only, no GC point.
pub(crate) fn method_finder_shape_file(
    ctx: &dyn NativeContext,
    line: &MethodFinderMintLine,
    finder: MethodFinder,
    args: &[Value],
    member_holder: ClassId,
) -> &'static str {
    match (finder, line.species) {
        (MethodFinder::Static, DirectSpecies::Plain)
        | (MethodFinder::Constructor, DirectSpecies::Constructor) => {}
        _ => return "method-finder.shape.not-filed.finder-or-species",
    }
    let Some(Value::Object(Some(refc_mirror))) = args.get(1) else {
        return "method-finder.shape.not-filed.null-refc";
    };
    let Some(refc) = ctx.class_id_from_mirror(*refc_mirror) else {
        return "method-finder.shape.not-filed.refc-no-class";
    };
    if member_holder != refc {
        return "method-finder.shape.not-filed.inherited-member";
    }
    if finder == MethodFinder::Static && line.holder != Some(refc) {
        return "method-finder.shape.not-filed.static-holder";
    }
    if method_finder_shape_class_facts(ctx, refc).is_none() {
        return "method-finder.shape.not-filed.class";
    }
    let mut members = vec![line.proto_member];
    members.extend(line.proto_init);
    for m in &members {
        if !matches!(ctx.get_field_by_name(*m, "method"), Value::Object(None)) {
            return "method-finder.shape.not-filed.member-method-word";
        }
        if !matches!(ctx.get_field_by_name(*m, "resolution"), Value::Object(None)) {
            return "method-finder.shape.not-filed.member-unresolved";
        }
    }
    let Value::Int(flags) = ctx.get_field_by_name(line.proto_member, "flags") else {
        return "method-finder.shape.not-filed.member-flags";
    };
    if flags & MN_CALLER_SENSITIVE != 0 {
        return "method-finder.shape.not-filed.caller-sensitive";
    }
    let name = match ctx.get_field_by_name(line.proto_member, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let Some(name) = name else {
        return "method-finder.shape.not-filed.member-name";
    };
    let descriptor = match ctx.get_field_by_name(line.proto_member, "type") {
        Value::Object(Some(mt)) => mhn_strict_method_descriptor(ctx, mt),
        _ => None,
    };
    let Some(descriptor) = descriptor else {
        return "method-finder.shape.not-filed.member-type";
    };
    let Some(access) = ctx.declared_method_access_flags(refc, &name, &descriptor) else {
        return "method-finder.shape.not-filed.not-declared";
    };
    if access & SHAPE_ACC_PRIVATE_OR_PROTECTED != 0 {
        return "method-finder.shape.not-filed.private-or-protected";
    }
    if (flags & 0xFFFF) as u16 != access {
        return "method-finder.shape.not-filed.flags-disagree";
    }
    let shape = method_finder_shape_key(&line.key);
    let slot = method_finder_shape_slot(&shape);
    let filed = MethodFinderShapeLine {
        line: *line,
        refc,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(descriptor),
        access,
    };
    METHOD_FINDER_SHAPE_MEMO.with(|m| m.borrow_mut()[slot] = Some(filed));
    "method-finder.shape.filed"
}

/// A fresh `MemberName`, word for word the rooted `proto`'s but for `clazz`,
/// which is the rooted `clazz`. `None` when the allocation fails.
pub(crate) fn method_finder_shape_member(
    scope: &mut NativeHandleScope<'_>,
    member_class: ClassId,
    proto: &cratonvm_native_api::NativeHandle,
    clazz: &cratonvm_native_api::NativeHandle,
) -> Option<cratonvm_native_api::NativeHandle> {
    let member_h = method_finder_mint_member(scope, member_class, proto)?;
    let member = scope.get(&member_h);
    let clazz = scope.get(clazz);
    scope.set_field_by_name(member, "clazz", Value::Object(Some(clazz)));
    Some(member_h)
}

/// `CRATONVM_MH_METHOD_FINDER_SHAPE_MINT`: class B's answer (`key`, `args`)
/// minted from its shape's line, or `None` (nothing allocated, no GC point
/// passed: the caller runs the body with `args` as they are). `probe` is
/// [`method_finder_mint_init_probe`]'s reading for this call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn method_finder_shape_mint(
    ctx: &mut dyn NativeContext,
    vm: usize,
    epoch: u64,
    key: MethodFinderMintKey,
    finder: MethodFinder,
    args: &[Value],
    probe: Option<(ClassId, bool)>,
) -> Option<MethodCallResult> {
    use crate::mh_finder_census as census;
    if !matches!(finder, MethodFinder::Static | MethodFinder::Constructor) {
        return None;
    }
    let shape = method_finder_shape_key(&key);
    let slot = method_finder_shape_slot(&shape);
    let filed = METHOD_FINDER_SHAPE_MEMO.with(|m| m.borrow()[slot].clone())?;
    if filed.line.vm != vm
        || filed.line.epoch != epoch
        || method_finder_shape_key(&filed.line.key) != shape
    {
        census::event("method-finder.shape.declined.no-line");
        return None;
    }
    if filed.line.key.a == key.a {
        census::event("method-finder.shape.declined.same-class");
        return None;
    }
    let Some(Value::Object(Some(b_mirror))) = args.get(1) else {
        return None;
    };
    let b_mirror = *b_mirror;
    let Some(b) = ctx.class_id_from_mirror(b_mirror) else {
        census::event("method-finder.shape.declined.refc-no-class");
        return None;
    };
    if b == filed.refc {
        census::event("method-finder.shape.declined.same-class");
        return None;
    }
    let (Some(a_facts), Some(b_facts)) = (
        method_finder_shape_class_facts(&*ctx, filed.refc),
        method_finder_shape_class_facts(&*ctx, b),
    ) else {
        census::event("method-finder.shape.declined.class");
        return None;
    };
    if a_facts != b_facts {
        census::event("method-finder.shape.declined.class-facts-differ");
        return None;
    }
    if ctx.declared_method_access_flags(b, &filed.name, &filed.descriptor) != Some(filed.access) {
        census::event("method-finder.shape.declined.member-differs");
        return None;
    }
    let holder = if finder == MethodFinder::Static {
        // The probe read B's state before this call; the form is per state.
        match probe {
            Some((c, initialized))
                if c == b
                    && initialized == filed.line.holder_initialized
                    && ctx.is_class_initialized(b) == initialized =>
            {
                Some(b)
            }
            _ => {
                census::event("method-finder.shape.declined.init-state");
                return None;
            }
        }
    } else {
        None
    };
    let line = filed.line;
    // From here on allocations (and, for a constructor, a Java call) may
    // collect: nothing below returns `None`.
    let mut scope = NativeHandleScope::new(ctx);
    let b_h = scope.root(b_mirror);
    let proto_h = scope.root(line.proto);
    let proto_member_h = scope.root(line.proto_member);
    let proto_init_h = line.proto_init.map(|o| scope.root(o));
    let Value::Object(Some(form)) = line.form else {
        return Some(Err(shape_mint_failed("form")));
    };
    let form_h = scope.root(form);
    // A constructor's type: `type().changeReturnType(B)`, as `makeAllocator`.
    let ctor_type_h = if finder == MethodFinder::Constructor {
        let proto = scope.get(&proto_h);
        let Value::Object(Some(a_type)) = scope.get_field_by_name(proto, "type") else {
            return Some(Err(shape_mint_failed("type")));
        };
        let b_now = scope.get(&b_h);
        match scope.invoke_virtual(
            a_type,
            "changeReturnType",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
            &[Value::Object(Some(b_now))],
        ) {
            Ok(Some(Value::Object(Some(t)))) => Some(scope.root(t)),
            Ok(_) => return Some(Err(shape_mint_failed("changeReturnType"))),
            Err(e) => return Some(Err(e)),
        }
    } else {
        None
    };
    let Some(member_h) =
        method_finder_shape_member(&mut scope, line.member_class, &proto_member_h, &b_h)
    else {
        return Some(Err(shape_mint_failed("member")));
    };
    let init_h = match &proto_init_h {
        Some(h) => match method_finder_shape_member(&mut scope, line.member_class, h, &b_h) {
            Some(m) => Some(m),
            None => return Some(Err(shape_mint_failed("initMethod"))),
        },
        None => None,
    };
    let fresh = match scope.instantiate(
        line.handle_class,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    ) {
        Ok(Some(o)) => o,
        Ok(None) => return Some(Err(shape_mint_failed("handle"))),
        Err(e) => return Some(Err(e)),
    };
    let fresh_h = scope.root(fresh);
    let proto = scope.get(&proto_h);
    let fresh = scope.get(&fresh_h);
    let form = Value::Object(Some(scope.get(&form_h)));
    let b_now = Value::Object(Some(scope.get(&b_h)));
    let ctor_type = ctor_type_h
        .as_ref()
        .map(|h| Value::Object(Some(scope.get(h))));
    for word in line.species.words() {
        let value = match (*word, &ctor_type) {
            ("form", _) => form,
            ("type", Some(t)) => *t,
            ("instanceClass", _) if line.species == DirectSpecies::Constructor => b_now,
            _ => scope.get_field_by_name(proto, word),
        };
        scope.set_field_by_name(fresh, word, value);
    }
    let member = scope.get(&member_h);
    scope.set_field_by_name(fresh, "member", Value::Object(Some(member)));
    let init = init_h.as_ref().map(|h| scope.get(h));
    if let Some(init) = init {
        scope.set_field_by_name(fresh, "initMethod", Value::Object(Some(init)));
    }
    // B's own line, so its repeat is a plain hit: filed only when no
    // collection ran since `key` was read (its words are addresses).
    if scope.gc_address_epoch() == epoch {
        let b_line = MethodFinderMintLine {
            key,
            proto: fresh,
            proto_member: member,
            proto_init: init,
            holder,
            ..line
        };
        let b_slot = method_finder_mint_slot(&key);
        METHOD_FINDER_MINT_MEMO.with(|m| m.borrow_mut()[b_slot] = Some(b_line));
    }
    census::event("method-finder.shape.served");
    Some(Ok(Some(Value::Object(Some(fresh)))))
}

/// The error a shape mint answers when an allocation after its first GC
/// point fails (the body cannot run then: `args` may be stale).
#[cold]
pub(crate) fn shape_mint_failed(what: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
        message: format!("Java heap space (method finder shape mint: {what})"),
    }))
}

/// A method finder under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`: a minted
/// repeat answer, else the JDK's own body (its answer filed when admissible).
pub(crate) fn mh_method_finder_mint_intrinsic(
    ctx: &mut dyn NativeContext,
    finder: MethodFinder,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(lookup))) = args.first() else {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message: None },
        )));
    };
    let lookup = *lookup;
    // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4): reads only.
    use crate::mh_finder_census as census;
    let census_on = census::enabled();
    let census_all = census::phase_start();
    if census_on {
        census::event(match finder {
            MethodFinder::Virtual => "method-finder.entry.findVirtual",
            MethodFinder::Static => "method-finder.entry.findStatic",
            MethodFinder::Special => "method-finder.entry.findSpecial",
            MethodFinder::Constructor => "method-finder.entry.findConstructor",
            MethodFinder::Unreflect => "method-finder.entry.unreflect",
        });
    }
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let key = if vm == 0 {
        census::event("method-finder.key-refused.vm-identity-zero");
        None
    } else if crate::nbflags().dbg_mh_dispatch {
        census::event("method-finder.key-refused.dbg-mh-dispatch");
        None
    } else {
        match method_finder_mint_key(&*ctx, finder, args) {
            Ok(key) => Some(key),
            Err(reason) => {
                census::event(reason);
                None
            }
        }
    };
    if let Some(key) = &key {
        let slot = method_finder_mint_slot(key);
        let line = METHOD_FINDER_MINT_MEMO.with(|m| m.borrow()[slot]);
        if census_on {
            census::event(match &line {
                None => "method-finder.line.empty-slot",
                Some(l) if l.vm != vm => "method-finder.line.other-vm",
                Some(l) if l.epoch != epoch => "method-finder.line.other-epoch",
                Some(l) if l.key != *key => "method-finder.line.other-key",
                Some(l)
                    if !l
                        .holder
                        .is_none_or(|h| ctx.is_class_initialized(h) == l.holder_initialized) =>
                {
                    if l.holder_initialized {
                        "method-finder.line.holder-state-changed.filed-initialized"
                    } else {
                        "method-finder.line.holder-state-changed.filed-uninitialized"
                    }
                }
                Some(_) => "method-finder.line.hit",
            });
        }
        // Wave 90 (`CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE`, default
        // on, `=0` off): a line filed before its static holder was initialized is
        // re-filed with the handle's barrier-free form when that form is
        // already the JDK's answer (see `method_finder_mint_init_refile`).
        let line = match line {
            Some(l)
                if crate::nbflags().mh_method_finder_mint_init_refile
                    && l.vm == vm
                    && l.epoch == epoch
                    && l.key == *key =>
            {
                match method_finder_mint_init_refile(&*ctx, &l) {
                    Ok(Some(refiled)) => {
                        census::event("method-finder.line.init-refile.served");
                        METHOD_FINDER_MINT_MEMO.with(|m| m.borrow_mut()[slot] = Some(refiled));
                        Some(refiled)
                    }
                    Ok(None) => Some(l),
                    Err(reason) => {
                        census::event(reason);
                        Some(l)
                    }
                }
            }
            other => other,
        };
        let line = line.filter(|l| {
            l.vm == vm
                && l.epoch == epoch
                && l.key == *key
                && l.holder
                    .is_none_or(|h| ctx.is_class_initialized(h) == l.holder_initialized)
        });
        if let Some(line) = line {
            let census_mint = census::phase_start();
            let minted = method_finder_mint(ctx, &line);
            census::phase_end(census::Phase::MethodFinderMint, census_mint);
            if let Some(answer) = minted {
                census::phase_end(census::Phase::MethodFinder, census_all);
                return answer;
            }
            census::event("method-finder.line.hit-but-mint-allocation-failed");
        }
    }
    let probe = if key.is_some() {
        method_finder_mint_init_probe(&*ctx, finder, args)
    } else {
        None
    };
    // Interpreter round i5 wave 108, lane L4 (`CRATONVM_MH_METHOD_FINDER_SHAPE_MINT`,
    // default off): a class of a filed shape is minted from that shape's line.
    // A decline passed no GC point, so `args` still hold.
    if let Some(key) = key {
        if crate::nbflags().mh_method_finder_shape_mint {
            // Wave 110: the census scope `shape-mint` (class work, re-entries
            // and frames inside the mint are named for it).
            let minted = {
                let _scope = census::named_scope("shape-mint");
                method_finder_shape_mint(ctx, vm, epoch, key, finder, args, probe)
            };
            if let Some(answer) = minted {
                census::phase_end(census::Phase::MethodFinder, census_all);
                return answer;
            }
        }
    }
    let census_body = census::phase_start();
    census::body_enter();
    let answer = ctx.invoke_virtual_bytecode_only(
        lookup,
        finder.method_name(),
        finder.descriptor(),
        &args[1..],
    );
    census::body_exit();
    census::phase_end(census::Phase::MethodFinderBody, census_body);
    if census_on {
        census::event(match &answer {
            Ok(Some(Value::Object(Some(_)))) => "method-finder.body.answered-handle",
            Ok(_) => "method-finder.body.answered-non-handle",
            Err(_) => "method-finder.body.threw",
        });
    }
    if let (Some(key), Ok(Some(Value::Object(Some(handle))))) = (key, &answer) {
        // The key's addresses (and `args`) were read before the body ran (a
        // GC point): filed only when no collection ran since.
        if ctx.gc_address_epoch() == epoch {
            let handle = *handle;
            let census_file = census::phase_start();
            let outcome = method_finder_mint_file(ctx, vm, epoch, key, finder, args, handle, probe);
            census::phase_end(census::Phase::MethodFinderFile, census_file);
            census::event(outcome);
        } else {
            census::event("method-finder.file-refused.collection-during-body");
        }
    }
    census::phase_end(census::Phase::MethodFinder, census_all);
    answer
}

/// `Lookup.findVirtual` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_virtual_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Virtual, args)
}

/// `Lookup.findStatic` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_static_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Static, args)
}

/// `Lookup.findSpecial` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_special_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Special, args)
}

/// `Lookup.findConstructor` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub(crate) fn mh_find_constructor_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Constructor, args)
}

/// `Lookup.unreflect` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub(crate) fn mh_unreflect_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Unreflect, args)
}

/// Register the five method finder `Intrinsic`s when `r`'s VM admits them
/// ([`mh_method_finder_mint_memo_registers`]); a no-op otherwise. States each
/// registration's kind, so the ambient category is neither read nor changed.
pub(crate) fn register_method_finder_mint_memo(r: &mut NativeMethodRegistry) {
    if !mh_method_finder_mint_memo_registers(r) {
        return;
    }
    // Wave 109 (`finder_mint_mode_admits`): a `--compatible` `JdkInvoke`
    // family drop keeps these stated `Intrinsic`s (closed after the loop).
    let admitting = r.set_admitting_family_intrinsic(true);
    for finder in MethodFinder::ALL {
        r.register_with_kind(
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            finder.descriptor(),
            finder.callback(),
            NativeKind::Intrinsic,
        );
        // A no-op unless `CRATONVM_INTRINSIC_BODY_COMPILES` or the
        // `method-finder-mint` row.
        r.mark_body_compilable_intrinsic_row(
            METHOD_FINDER_MINT_BODY_ROW,
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            finder.descriptor(),
        );
    }
    r.set_admitting_family_intrinsic(admitting);
}

/// `CRATONVM_JIT_MH_COMPILED_LINK` -- perf, default ON, `=0` off (JIT round 20
/// wave 19, lane pRefl; page
/// `docs/feature-designs/profile-guided-inlining.md`):
/// a compiled `invokeExact` site whose linker pair is in the VM's table, a
/// compiled `invokeBasic` on a real handle, and a compiled `linkToStatic` /
/// `linkToVirtual` / `linkToInterface` call their Java target as an ordinary
/// compiled call (`vm/src/jit/helpers.rs` `mh_compiled_link_redirect`)
/// instead of this crate's doors and their VM-side re-entry. Every shape the
/// helper does not prove declines to the door. Read per call (a `NativeFlags`
/// field load).
pub fn jit_mh_compiled_link_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_LEAN` -- perf, default ON, `=0` off (JIT
/// round 21 lane reflect2; page
/// `jr21-reflect2-method-invoke-per-call-link-work-FIXED-20261015.md`): the per-call
/// work of a compiled signature-polymorphic link without its allocations
/// (`vm/src/jit/helpers.rs` `mh_link_basic_agree`, [`mh_compiled_linker_pair`]'s
/// key, `mh_compiled_member`'s memo-first decode). A `NativeFlags` field load.
pub fn jit_mh_compiled_link_lean_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_lean
}

/// The VM linker-table key of a compiled `invokeExact` site of descriptor
/// `site` in caller class `caller`, built in one `String`: byte for byte
/// `method_type_table::class_keyed(&format!("linker:invokeExact:{site}"), &[caller])`,
/// the key [`mh_linked_invoker`] files the row under (a NUL separates the
/// descriptor from the class ids there; a unit test pins the equality).
pub(crate) fn jr21_reflect2_linker_key(site: &str, caller: u32) -> String {
    use std::fmt::Write as _;
    const PREFIX: &str = "linker:invokeExact:";
    let mut key = String::with_capacity(PREFIX.len() + site.len() + 12);
    key.push_str(PREFIX);
    key.push_str(site);
    key.push('\0');
    let _ = write!(key, "{caller}");
    key
}

/// The `form.vmentry` `MemberName` that `invokeBasic` on `handle` runs, for a
/// compiled site (`CRATONVM_JIT_MH_COMPILED_LINK`): exactly the first arm the
/// registered `invokeBasic` takes for a real handle
/// (`native_method_handle_invoke`'s downcall test, then [`mh_invoke_basic`]'s
/// [`mh_real_lf_entry_fast`] under the same switches). `None` wherever that
/// arm would not run first. Pure reads.
pub fn mh_compiled_basic_vmentry(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    if !mh_lf_vmentry_enabled()
        || crate::nbflags().dbg_mh_dispatch
        || crate::panama::is_downcall_handle(ctx, handle)
        || ctx.object_num_fields(handle) > MH_BOUND
    {
        return None;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(handle, "form") else {
        return None;
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        return None;
    };
    Some(entry)
}

/// The linker pair ([`mh_linked_invoker`]'s `(MemberName, appendix)`) the VM's
/// table holds for a compiled `invokeExact` site of descriptor `site` in class
/// `caller` (raw id), with whether a null first argument is safe to pass to
/// the linker (`CRATONVM_JIT_MH_COMPILED_LINK`). `None` -- the door's call --
/// unless the door itself would run that linker first, which a table row proves
/// for everything the door asks before it: the site was linked (the row is
/// only filed after `Invokers.methodHandleInvokeLinkerMethod` answered), its
/// descriptor is bootstrap-shareable (so `mh_site_access_refusal` has no
/// class to refuse and the row is keyed exactly as the door keys it). The
/// handle must be a real JDK handle that is not a downcall handle and has a
/// form; a `DirectMethodHandle` or a `DelegatingMethodHandle` (whose target may
/// be one) is left to the door when the first argument is null, because the
/// door raises an interface handle's null-receiver NPE before the linker runs
/// ([`mh_interface_null_receiver_npe`]). Pure reads. A site that is not
/// shareable is answered only under `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE`
/// (default off), from the door's basic-member and site-type rows
/// ([`mh_compiled_linker_user_site`]).
pub fn mh_compiled_linker_pair(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    site: &str,
    caller: u32,
    first_is_null: bool,
) -> Option<(ObjectRef, ObjectRef, bool)> {
    if caller == 0
        || ctx.object_num_fields(handle) > MH_BOUND
        || crate::panama::is_downcall_handle(ctx, handle)
        || !matches!(
            ctx.get_field_by_name(handle, "form"),
            Value::Object(Some(_))
        )
    {
        return None;
    }
    // JIT round 21 lane reflect2 (`CRATONVM_JIT_MH_COMPILED_LINK_LEAN`): the
    // key in one `String` (`jr21_reflect2_linker_key`), not two.
    let lean = crate::nbflags().jit_mh_compiled_link_lean;
    let key = if lean {
        jr21_reflect2_linker_key(site, caller)
    } else {
        cratonvm_native_api::method_type_table::class_keyed(
            &format!("linker:invokeExact:{site}"),
            &[caller],
        )
    };
    // JIT round 21 lane reflect (`CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST`,
    // default ON): the door ([`mh_linked_invoker`]) files a row under this key
    // only after the shareable check passed, and nothing else writes the key,
    // so a row found proves it; the check (a descriptor parse and a class
    // lookup per component) ran on every compiled `Method.invoke`. `=0` asks
    // it first, as before.
    let shareable = (crate::nbflags().jit_mh_compiled_link_pair_first
        && ctx.method_handle_linker_get(&key).is_some())
        || bound_method_type_shareable(ctx, site);
    if !shareable && !crate::nbflags().jit_mh_compiled_link_user_site {
        return None;
    }
    if !shareable {
        // Interpreter round i5 wave 104, lane L4: a handle one of the door's
        // before-linker lanes serves keeps the door (see
        // [`mh_user_site_lane_serves`]). Wave 106: asked by class id first
        // ([`mh_user_site_lane_serves_handle`]), so a lane-served handle
        // pays no class-manager read beyond the switch-off arm's.
        if mh_user_site_lane_serves_handle(ctx, handle) {
            return None;
        }
        let null_first_ok = ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .as_deref()
            .is_some_and(|name| name.starts_with(BMH_SPECIES_PREFIX));
        return mh_compiled_linker_user_site(ctx, site, caller)
            .map(|(member, appendix)| (member, appendix, null_first_ok));
    }
    // `first_is_null`: the call's first argument word is null; when it is
    // not, the caller does not read the answer's third field, and under
    // `CRATONVM_JIT_MH_COMPILED_LINK_LEAN` it is not computed (`true`).
    let null_first_ok = (lean && !first_is_null)
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .as_deref()
            .is_some_and(|name| name.starts_with(BMH_SPECIES_PREFIX));
    let pair = ctx.method_handle_linker_get(&key)?;
    if ctx.array_length(pair) != 2 {
        return None;
    }
    let Value::Object(Some(member)) = ctx.get_array_element(pair, 0) else {
        return None;
    };
    let Value::Object(Some(appendix)) = ctx.get_array_element(pair, 1) else {
        return None;
    };
    Some((member, appendix, null_first_ok))
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO` -- perf, default ON, `=0` off
/// (JIT round 21 lane reflect3; page
/// `jr21-reflect3-method-invoke-steady-state-string-work-20261014.md`): the
/// compiled link's basic-type agreement is memoised on the member's record
/// ([`crate::MhCompiledMember::agree_memo`]). A `NativeFlags` field load.
pub fn jit_mh_compiled_link_agree_memo_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_agree_memo
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN` -- perf, default ON, `=0` off (JIT
/// round 21 lane reflect4, REFLECT3-21-2; page
/// `jr21-reflect3-method-invoke-steady-state-string-work-20261014.md`): a
/// compiled `invokeBasic` / `linkToStatic` link whose target is a static
/// member of a hidden class (a spun `LambdaForm$MH`) calls its compiled body
/// by class id ([`crate::MhCompiledMember::bind_checked_hidden`]) instead of
/// the `invokeBasic` door. A `NativeFlags` field load.
pub fn jit_mh_compiled_link_hidden_enabled() -> bool {
    crate::nbflags().jit_mh_compiled_link_hidden
}

/// [`mh_compiled_linker_pair`] memoised on the handle's per-epoch
/// [`RealHandleRecord`] (JIT round 21 lane reflect3,
/// `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO`, default ON, `=0` off;
/// REFLECT2-21-4 of `jit-r21-reflect2-proposals.md`): a compiled
/// `invokeExact` site built the linker key `String` and probed the VM's
/// locked linker table on every `Method.invoke`.
///
/// The same answer: every input of [`mh_compiled_linker_pair`] is fixed for
/// a key of (handle address in this epoch, site text, caller class): the
/// handle's field count and class (the carrier and downcall tests, the
/// species test of the null-first rule) are its class's, its `form` is never
/// set back to null once a handle has one, and a linker row is first-writer-
/// wins, rooted, and dropped only when its caller class unloads, which a
/// collection does (a new epoch, a new record). Any decline is not memoised
/// (the next call asks again, as before), nor is anything for a handle the
/// record model does not decode or a context with VM identity `0`.
pub fn mh_compiled_linker_pair_memo(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &'static str,
    caller: u32,
    first_is_null: bool,
) -> Option<(ObjectRef, ObjectRef, bool)> {
    if !crate::nbflags().jit_mh_compiled_link_pair_memo || caller == 0 || ctx.vm_identity() == 0 {
        return mh_compiled_linker_pair(&*ctx, handle, site, caller, first_is_null);
    }
    let record = real_handle_record(ctx, handle);
    if let Some(r) = record.as_ref() {
        if let Some(hit) = &*r.linker_pair.borrow() {
            if hit.caller == caller && *hit.site == *site {
                return Some((hit.member, hit.appendix, !first_is_null || hit.bmh_species));
            }
        }
    }
    let answer = mh_compiled_linker_pair(&*ctx, handle, site, caller, first_is_null)?;
    if let Some(record) = record {
        let bmh_species = ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .is_some_and(|name| name.starts_with(BMH_SPECIES_PREFIX));
        *record.linker_pair.borrow_mut() = Some(LinkerPairMemo {
            site: Box::from(site),
            caller,
            member: answer.0,
            appendix: answer.1,
            bmh_species,
        });
    }
    Some(answer)
}

/// `CRATONVM_JIT_REFLECT_ACCESSOR_LINK` -- perf, default OFF, opt-in (JIT
/// round 21 wave 1, lane reflect; `vm/src/jit/helpers.rs`
/// `reflect_accessor_link_redirect`): the compiled `invokeExact` site of
/// `DirectMethodHandleAccessor.invokeImpl` calls the reflected method as an
/// ordinary compiled call, unboxing its arguments and boxing its result in the
/// helper, instead of the linker chain (`invokeExact_MT` -> `invokeBasic` ->
/// the spun `LambdaForm$MH` adapter -> `DirectMethodHandle$Holder` ->
/// `linkTo*`). Read per call (a `NativeFlags` field load).
pub fn jit_reflect_accessor_link_enabled() -> bool {
    crate::nbflags().jit_reflect_accessor_link
}

/// `CRATONVM_JIT_REFLECT_CTOR_LINK` -- perf, default OFF, opt-in (JIT round
/// 21 wave 1, lane reflect; REFLECT21-3): `vm/src/jit/helpers.rs`
/// `reflect_ctor_link_redirect`. Read per call (a `NativeFlags` field load).
pub fn jit_reflect_ctor_link_enabled() -> bool {
    crate::nbflags().jit_reflect_ctor_link
}

/// The `MemberName` of the direct method handle a `DirectMethodHandleAccessor`'s
/// `target` adapts, for that accessor's compiled `invokeExact` site of
/// descriptor `site` (JIT round 21 wave 1, lane reflect;
/// [`jit_reflect_accessor_link_enabled`]). `MethodHandleAccessorFactory`
/// builds every such `target` as `makeSpecializedTarget(dmh)`: `dmh.asType` of
/// the generic type, after `dropArguments(dmh, 0, Object)` for a static method.
/// `asType` (`makePairwiseConvertByEditor`) and `dropArguments` both
/// `rebind()` the direct handle into a `BoundMethodHandle` species whose
/// `argL0` is that direct handle and whose further `L` arguments are the
/// conversions; with no conversion at all (`viewAsType`) the target is a
/// direct handle itself. So: the handle's declared type must be `site`
/// (`invokeExact`'s own type check passes), and the direct handle is the
/// handle itself or its `argL0`. The conversions are not decoded here: for
/// such a target they are exactly `asType`'s from the generic type to the
/// member's own, which the VM side applies (and declines wherever one could
/// fail). `None` for anything else -- a carrier handle, another species
/// shape, a field or constructor handle. Pure reads; no allocation, no GC
/// point.
///
/// Only meaningful at the accessor's own site, whose `target` field nothing
/// but the factory writes: the same structure built by another combinator
/// (`filterArguments`) would carry different conversions, so the caller
/// screens the site's class first.
pub fn mh_reflect_accessor_member(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<ObjectRef> {
    if crate::nbflags().dbg_mh_dispatch || crate::panama::is_downcall_handle(&*ctx, handle) {
        return None;
    }
    let record = real_handle_record(ctx, handle)?;
    if record.declared != site {
        return None;
    }
    let direct = if record.direct.is_some() {
        handle
    } else {
        let species = ctx
            .class_name_arc_of_id(ctx.class_id_of_object(handle))
            .is_some_and(|name| {
                name.strip_prefix(BMH_SPECIES_PREFIX)
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b == b'L'))
            });
        if !species {
            return None;
        }
        let Value::Object(Some(inner)) = ctx.get_field_by_name(handle, "argL0") else {
            return None;
        };
        if real_handle_record(ctx, inner)?.direct.is_none() {
            return None;
        }
        inner
    };
    match ctx.get_field_by_name(direct, "member") {
        Value::Object(Some(member)) => Some(member),
        _ => None,
    }
}

/// [`mh_reflect_accessor_member`] for `DirectConstructorHandleAccessor`'s
/// `target` (JIT round 21 wave 1, lane reflect; REFLECT21-3,
/// [`jit_reflect_ctor_link_enabled`]): `makeConstructorHandle(dmh)` is
/// `dmh.asType` of the generic type over a `DirectMethodHandle$Constructor`
/// (no `dropArguments`), so the constructor handle is the target itself
/// (`viewAsType`, no conversion) or its species' `argL0`. Answers the
/// constructor's `MemberName` and the handle's `instanceClass` (the class
/// `newInvokeSpecial` allocates; for a serialization constructor it is not
/// the constructor's own class, which the caller refuses). Pure reads.
pub fn mh_reflect_ctor_accessor_member(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<(ObjectRef, ClassId)> {
    const CTOR_DMH: &str = "java/lang/invoke/DirectMethodHandle$Constructor";
    if crate::nbflags().dbg_mh_dispatch || crate::panama::is_downcall_handle(&*ctx, handle) {
        return None;
    }
    let record = real_handle_record(ctx, handle)?;
    if record.declared != site {
        return None;
    }
    let class_of = |ctx: &dyn NativeContext, h: ObjectRef| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(h))
    };
    let direct = match class_of(&*ctx, handle) {
        Some(name) if &*name == CTOR_DMH => handle,
        Some(name)
            if name
                .strip_prefix(BMH_SPECIES_PREFIX)
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b == b'L')) =>
        {
            let Value::Object(Some(inner)) = ctx.get_field_by_name(handle, "argL0") else {
                return None;
            };
            if class_of(&*ctx, inner).as_deref() != Some(CTOR_DMH) {
                return None;
            }
            inner
        }
        _ => return None,
    };
    let Value::Object(Some(member)) = ctx.get_field_by_name(direct, "member") else {
        return None;
    };
    let Value::Object(Some(instance_mirror)) = ctx.get_field_by_name(direct, "instanceClass")
    else {
        return None;
    };
    let instance = ctx.class_id_from_mirror(instance_mirror)?;
    Some((member, instance))
}

/// Is `class` (a handle's class name) one the door's before-linker lanes
/// serve at a user-class `invokeExact` site, with their switches as they are?
/// A real field accessor (`mh_real_field_before_linker`), a plain
/// `DirectMethodHandle` (`mh_real_direct_before_linker`, kinds 5 / 6) and a
/// `Species_LL` (`mh_real_bound_field_before_linker`, `bindTo` of an
/// accessor). Interpreter round i5 wave 104, lane L4 (page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`,
/// "Wave 104"): the w103b A/B of `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE`
/// measured exactly these handles 1.35-2.2x SLOWER through the compiled linker
/// chain (`invokeExact_MT` -> `invokeBasic` -> holder form -> `linkTo*` /
/// `Unsafe`, each hop a compiled-call redirect with by-name field reads, and
/// the shared JDK holder forms profiled by every handle) than through the
/// door's one-native lane, so [`mh_compiled_linker_pair`] leaves them to the
/// door. A lane that then declines (type row miss, null base, ...) runs the
/// linker as with the switch off. Pure, no allocation.
pub(crate) fn mh_user_site_lane_serves(class: &str) -> bool {
    let flags = crate::nbflags();
    if flags.dbg_mh_dispatch {
        return false;
    }
    let field_lane = flags.mh_real_field_before_linker && mh_real_field_record_enabled();
    match class {
        "java/lang/invoke/DirectMethodHandle$Accessor"
        | "java/lang/invoke/DirectMethodHandle$StaticAccessor" => field_lane,
        "java/lang/invoke/DirectMethodHandle" => flags.mh_real_direct_before_linker,
        BMH_SPECIES_LL => field_lane && flags.mh_real_bound_field_before_linker,
        _ => false,
    }
}

thread_local! {
    /// Interpreter round i5 wave 106, lane L4: the handle classes
    /// [`mh_user_site_lane_serves`] answered `true` for on this thread, as
    /// `(vm identity, class id)`, most recent first. Positive answers only:
    /// the four lane classes are `java.lang.invoke` classes of the boot
    /// loader, never unloaded, so an id stays theirs for the VM's life; the
    /// answer reads only per-process flags.
    pub(crate) static USER_SITE_LANE_CLASSES: std::cell::Cell<[(usize, u32); 4]> =
        const { std::cell::Cell::new([(0, 0); 4]) };
}

/// [`mh_user_site_lane_serves`] for `handle`'s class, asked by class id
/// before the name (page `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`,
/// "Wave 106"). The w105b A/B of `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE`
/// still read the field rows 1.03-1.20x slower with the switch on although
/// both arms then take the door: the only work the on arm added per call
/// was this question, which read the handle's class NAME under the class
/// manager's read lock on every compiled call. A hit here is one header
/// read and a thread-local probe. Pure reads, no allocation.
pub(crate) fn mh_user_site_lane_serves_handle(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    let cid = ctx.class_id_of_object(handle);
    let raw = cid.as_u32();
    if vm != 0
        && USER_SITE_LANE_CLASSES.with(|c| c.get().iter().any(|&(v, id)| v == vm && id == raw))
    {
        return true;
    }
    let serves = ctx
        .class_name_arc_of_id(cid)
        .as_deref()
        .is_some_and(mh_user_site_lane_serves);
    if serves && vm != 0 && raw != 0 {
        USER_SITE_LANE_CLASSES.with(|c| {
            let mut lines = c.get();
            lines.rotate_right(1);
            lines[0] = (vm, raw);
            c.set(lines);
        });
    }
    serves
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE` -- performance, default OFF,
/// `truthy_word` (interpreter round i5 wave 103, lane L4; page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`, "Wave 103").
///
/// [`mh_compiled_linker_pair`] for a compiled `invokeExact` site whose
/// descriptor names a class outside the shareable set (an application
/// class: `(LBox;)I`), which no `linker:` pair row may hold. The door's
/// basic-member hit ([`mh_linked_invoker`], `CRATONVM_MH_LINKER_BASIC_MEMBER`)
/// already runs such a site from two rows: the linker's `MemberName` filed per
/// caller, name and BASIC type ([`linker_basic_descriptor`]; a JDK form's
/// entry, which names no application class) and the site's `MethodType` from
/// the caller loader's site-type row ([`mh_site_type_row_for`]: the object the
/// factory interned and the row roots). This answers the same pair, so the
/// compiled site calls the linker form directly, as HotSpot's `invokehandle`
/// does, instead of the door on every call.
///
/// Both rows are filed only by the door's own miss, after it linked the site
/// for this caller. What the door asks before the linker and the rows do not
/// prove is asked or declined here:
/// - `mh_site_access_refusal`: it can refuse only a site naming a class two
///   loaders define (`NameLookup::Ambiguous`); such a site is declined, so the
///   door keeps raising it (no frame walk is needed to decline);
/// - `mh_interface_null_receiver_npe`: the caller's `null_first_ok` (a null
///   first argument reaches the linker only for a bound species);
/// - the door's lanes ahead of the linker (`mh_real_field_before_linker`,
///   `mh_real_direct_before_linker`, ...) are skipped; they answer what the
///   linker's chain answers, minus its hidden frames, which the compiled call
///   keeps as HotSpot does.
///
/// `None` (the door) for a site wider than the cached form, a caller whose
/// loader is not a definite built-in one (no site-type row), or before the
/// door's first call filed the rows. Pure reads.
pub(crate) fn mh_compiled_linker_user_site(
    ctx: &dyn NativeContext,
    site: &str,
    caller: u32,
) -> Option<(ObjectRef, ObjectRef)> {
    if !crate::nbflags().mh_linker_basic_member {
        return None;
    }
    if mh_descriptor_any_loader_name(site, |name| {
        matches!(
            ctx.classify_class_name(name),
            cratonvm_native_api::NameLookup::Ambiguous { .. }
        )
    }) {
        return None;
    }
    let basic = linker_basic_descriptor(site)?;
    let key = cratonvm_native_api::method_type_table::class_keyed(
        &format!("linker-basic:invokeExact:{basic}"),
        &[caller],
    );
    let member = ctx.method_handle_linker_get(&key)?;
    let appendix = mh_site_type_row_for(ctx, ClassId::new(caller), site)?;
    Some((member, appendix))
}

/// The interpreter's answer for an interpreted `invokeExact` it may push the
/// linker frame of (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, `--nojit`; see
/// [`mh_interp_exact_link`]).
#[derive(Clone, Copy, Debug)]
pub enum InterpExactLink {
    /// The door's linker pair `(MemberName, appendix)` and whether a null
    /// first argument may reach the linker, as [`mh_compiled_linker_pair`]
    /// answers it, from a bootstrap-shareable site's `linker:` row: a rooted
    /// row that stays while the caller class lives, so the caller may
    /// remember it within one address epoch (re-asking
    /// [`mh_interp_exact_handle_admits`] per call).
    Pair(ObjectRef, ObjectRef, bool),
    /// The same answer for a user-class site (`CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE`):
    /// for this call only, since the door's per-call loader-ambiguity question
    /// is asked again on every call.
    PairOnce(ObjectRef, ObjectRef, bool),
    /// The door, for this call only (no row filed yet, a handle without a
    /// form, ...): ask again next call.
    Door,
    /// The door, and it stays the door for this handle class at this site
    /// while nothing else changes (a before-linker lane serves the class, or
    /// the site can hold no pair row with the user-site switch off). The
    /// caller may remember it; a stale "door" only costs the door's call.
    DoorForGood,
}

/// Interpreter round i6 wave 130, lane L4 (page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 130"):
/// [`mh_compiled_linker_pair`] for the interpreter's linker-frame push, with
/// two differences that make the push neutral outside the `invokeWithArguments`
/// body arm:
///
/// * a handle whose class one of the door's before-linker lanes serves
///   ([`mh_user_site_lane_serves_handle`]: a field accessor, a plain
///   `DirectMethodHandle`, a `bindTo` of an accessor) keeps the door at EVERY
///   site, shareable ones included. The w124 A/B measured the push 3-8x
///   slower on exactly those (`static-getter-warm-exact` 313 -> 2463 ns,
///   `bound-getter-warm-exact`, `static-warm-exact`, `jdk-site-warm-exact`):
///   the door answers them with one native, the pushed linker chain
///   interprets five or six `LambdaForm` / `Invokers` frames. The JIT's
///   compiled-site link keeps its own rule ([`mh_compiled_linker_pair`],
///   unchanged);
/// * the answer says whether a door decision is stable
///   ([`InterpExactLink::DoorForGood`]), so the interpreter can remember it
///   and not re-parse the site descriptor on every call (the w124
///   `getter-warm-exact` / `setter-warm-exact` rows paid ~220 ns a call for
///   that parse before declining).
///
/// Pure reads (no allocation beyond the shareability parse, no Java).
pub fn mh_interp_exact_link(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    site: &str,
    caller: u32,
) -> InterpExactLink {
    if caller == 0 || ctx.object_num_fields(handle) > MH_BOUND {
        return InterpExactLink::DoorForGood;
    }
    if mh_user_site_lane_serves_handle(ctx, handle) {
        // Interpreter round i8 (page i147-orch;
        // `CRATONVM_MH_INTERP_EXACT_BOUND_SPECIES_PER_HANDLE`): the bound-field
        // lane serves a `Species_LL` only when it is a `bindTo` of a field
        // accessor. Any other one (the `asType` adapter of a typed reflective
        // constructor) is the linker's, and a class-wide door made it re-enter
        // the interpreter from the door's native on every call.
        if !crate::nbflags().mh_interp_exact_bound_species_per_handle
            || !bound_species_ll(ctx, handle)
        {
            return InterpExactLink::DoorForGood;
        }
        if bound_species_may_be_field_lane(ctx, handle) {
            return InterpExactLink::Door;
        }
    }
    let shareable = bound_method_type_shareable(ctx, site);
    if !shareable && !crate::nbflags().jit_mh_compiled_link_user_site {
        return InterpExactLink::DoorForGood;
    }
    match mh_compiled_linker_pair(ctx, handle, site, caller, true) {
        Some((member, appendix, null_first_ok)) if shareable => {
            InterpExactLink::Pair(member, appendix, null_first_ok)
        }
        Some((member, appendix, null_first_ok)) => {
            InterpExactLink::PairOnce(member, appendix, null_first_ok)
        }
        None => InterpExactLink::Door,
    }
}

/// The per-object half of [`mh_compiled_linker_pair`]'s handle admission
/// (not a carrier, not a downcall handle, a `form`), asked again on every call
/// the interpreter answers from its remembered pair (the pair itself is a
/// function of the handle's class, the site and the caller). Pure reads.
/// Interpreter round i6 wave 130, lane L4.
pub fn mh_interp_exact_handle_admits(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    ctx.object_num_fields(handle) <= MH_BOUND
        && !crate::panama::is_downcall_handle(ctx, handle)
        && matches!(
            ctx.get_field_by_name(handle, "form"),
            Value::Object(Some(_))
        )
        // Round i8: a pair remembered for a `Species_LL` handle class (an
        // `asType` adapter) does not take a `bindTo` of a field accessor of the
        // same class away from the bound-field lane.
        && !(crate::nbflags().mh_interp_exact_bound_species_per_handle
            && bound_species_ll(ctx, handle)
            && mh_user_site_lane_serves_handle(ctx, handle)
            && bound_species_may_be_field_lane(ctx, handle))
}

thread_local! {
    /// `(vm identity, class id)` of `BoundMethodHandle$Species_LL`, once
    /// [`bound_species_ll`] has seen it (a boot `java.lang.invoke` class,
    /// never unloaded). Interpreter round i8.
    pub(crate) static BMH_SPECIES_LL_ID: std::cell::Cell<(usize, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// Whether `handle` is a `BoundMethodHandle$Species_LL`: by class id after the
/// first positive answer, else by name. Pure reads. Interpreter round i8
/// (page i147-orch).
pub(crate) fn bound_species_ll(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    let raw = ctx.class_id_of_object(handle).as_u32();
    let (seen_vm, seen) = BMH_SPECIES_LL_ID.with(|c| c.get());
    if vm != 0 && seen_vm == vm && seen != 0 {
        return raw == seen;
    }
    let is = ctx
        .class_name_arc_of_id(ClassId::new(raw))
        .as_deref()
        == Some(BMH_SPECIES_LL);
    if is && vm != 0 && raw != 0 {
        BMH_SPECIES_LL_ID.with(|c| c.set((vm, raw)));
    }
    is
}

/// Whether [`mh_real_bound_field_before_linker`] can serve the `Species_LL`
/// `handle` at all: it serves only `accessor.bindTo(base)` of a real
/// INSTANCE field accessor, whose bound `argL0` is a
/// `DirectMethodHandle$Accessor` (its record then needs the field, and a
/// static accessor is refused). Pure reads. Interpreter round i8 (page
/// i147-orch).
pub(crate) fn bound_species_may_be_field_lane(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    let Value::Object(Some(bound)) = ctx.get_field_by_name(handle, "argL0") else {
        return false;
    };
    ctx.class_name_arc_of_id(ctx.class_id_of_object(bound))
        .as_deref()
        == Some("java/lang/invoke/DirectMethodHandle$Accessor")
}

/// Charge the span from `start` (a `mh_finder_census::phase_start`, `None`
/// unless the census is on) to the timed row `linker <name> <what>`.
/// Interpreter round i4 wave 95, lane L4.
pub(crate) fn linker_census(name: &str, what: &str, start: Option<std::time::Instant>) {
    if let Some(start) = start {
        crate::mh_finder_census::note_timed("linker", &format!("{name} {what}"), start);
    }
}

/// The site-type table's row for the call site `call_site` of the class
/// `caller` (raw reads, no Java, no allocation): the `MethodType` that
/// [`mh_site_method_type`] answers for that caller on a table hit, which is
/// the one `MethodType.fromMethodDescriptorString` interned when the row was
/// filed (the row roots it, so the intern set still answers that object).
/// `None` when the table is off, the caller's loader is not a definite
/// built-in one, or no row was filed yet (a site's first link files it).
/// Interpreter round i4 wave 95, lane L4.
pub(crate) fn mh_site_type_row_for(
    ctx: &dyn NativeContext,
    caller: ClassId,
    call_site: &str,
) -> Option<ObjectRef> {
    if !mh_site_type_table_enabled() {
        return None;
    }
    let loader = ctx.loader_id_of_class_definite(caller)?;
    let key = mh_site_type_key(Some(loader), call_site)?;
    ctx.bound_method_type_get(&key)
}

/// `MethodType.basicType()` of the call site `desc`, as a descriptor (every
/// reference and array `Object`, every subword primitive `int`, the return
/// included), when the JDK linker for it is the CACHED form of that basic
/// type: `Invokers.methodHandleInvokeLinkerMethod` builds
/// `invokeHandleForm(mtype, false, which)` -- whose first step is
/// `mtype = mtype.basicType()` and whose answer is cached in that basic type's
/// `MethodTypeForm` -- only while `mtype.parameterSlotCount() <=
/// MAX_MH_ARITY - MH_LINKER_ARG_APPENDED` (253); a wider site gets a
/// customized, uncached form and no appendix, and answers `None` here. So the
/// linker's `MemberName` (`lform.vmentry`) is a function of this string and
/// the linker name alone. Pure. Interpreter round i4 wave 95, lane L4.
pub(crate) fn linker_basic_descriptor(desc: &str) -> Option<String> {
    fn basic(t: &str) -> Option<&'static str> {
        match t.as_bytes().first()? {
            b'L' | b'[' => Some("Ljava/lang/Object;"),
            b'Z' | b'B' | b'S' | b'C' | b'I' => Some("I"),
            b'J' => Some("J"),
            b'F' => Some("F"),
            b'D' => Some("D"),
            b'V' => Some("V"),
            _ => None,
        }
    }
    let (params, ret) = split_descriptor_params(desc)?;
    let mut slots = 0usize;
    let mut out = String::with_capacity(2 + params.len() * 2 + 18);
    out.push('(');
    for p in &params {
        let b = basic(p)?;
        if b == "V" {
            return None;
        }
        slots += if matches!(b, "J" | "D") { 2 } else { 1 };
        out.push_str(b);
    }
    if slots > 253 {
        return None;
    }
    out.push(')');
    out.push_str(basic(&ret)?);
    Some(out)
}

/// `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER` -- performance, default OFF,
/// `truthy_word` (interpreter round i4 wave 95, lane L4; page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`).
///
/// A REAL field accessor (`DirectMethodHandle$Accessor` / `$StaticAccessor`,
/// what the `Lookup` field finders answer in both modes) called by
/// `invokeExact` / `invoke` is served from its [`RealFieldTarget`] record
/// ahead of [`mh_linked_invoker`], which otherwise runs the JDK's invoker
/// linker and the handle's whole `LambdaForm` chain on every call (and, at a
/// site naming an application class, `Invokers.methodHandleInvokeLinkerMethod`
/// and three full root refreshes too: ~40 us per warm getter call against
/// HotSpot's 3-4 ns, `L4W94FieldLinkFirst`).
///
/// Served only when nothing that chain runs can be seen from Java:
/// * the site's `MethodType`, as the caller resolves it (the site-type table's
///   row, [`mh_site_type_row_for`]), IS the handle's `type` -- so the linker's
///   `checkExactType` (`invokeExact`) passes and `checkGenericType`'s
///   `asType` (`invoke`) answers the handle itself, and no access, loader or
///   cast refusal of the doors can fire (the access refusal is asked anyway);
/// * an instance accessor's base is non-null (`checkBase`'s NPE and its trace
///   stay the chain's);
/// * a static accessor's holder is initialized (`ensureInitialized` would run
///   nothing);
/// * a reference setter's value is `null` or of the field's own class (the
///   form's `checkCast` cannot fail), or the field is `Object`;
/// * the native stack is not exhausted (the chain's `StackOverflowError`).
///
/// The field read or write is then [`mh_real_field_access_resolved`]'s, the
/// access [`mh_real_field_record_fast`] makes (its volatile fences included).
/// What the chain did that this skips: its hidden `LambdaForm` frames (no
/// Java code runs below them for a field access, so no walk can list them),
/// and `Invokers.checkCustomized`'s count, which customizes the handle's
/// `form` after 127 calls (a private field). `None` declines; the linker
/// runs, nothing disturbed.
pub(crate) fn mh_real_field_before_linker(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    armed_caller: u32,
) -> Option<MethodCallResult> {
    if !crate::nbflags().mh_real_field_before_linker
        || !mh_real_field_record_enabled()
        || crate::nbflags().dbg_mh_dispatch
    {
        return None;
    }
    let site = call_site?;
    // A carrier (this model's slots) never reaches the linker either.
    if ctx.object_num_fields(this) > MH_BOUND {
        return None;
    }
    let census = crate::mh_finder_census::phase_start();
    let record = real_handle_record(ctx, this)?;
    let field = record.field.as_ref()?;
    if site != record.declared {
        return None;
    }
    if !real_field_call_admits(&*ctx, this, field, extra) {
        return None;
    }
    // Interpreter round i5 wave 100, lane L4 (`CRATONVM_MH_REAL_FIELD_SITE_MEMO`;
    // bench `tools/probes/interp/L4/L4W100FieldSiteMemoBench.java`): a
    // (handle, caller, site) that passed the two site checks in this
    // collection epoch passes them again (see [`real_lane_site_admits`]).
    if !real_lane_site_admits(&*ctx, this, site, armed_caller) {
        return None;
    }
    // JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`):
    // the skipped linker's `checkCustomized` count; the due call is the
    // linker's. Nothing below declines.
    lane_check_customized(&*ctx, this)?;
    let raw = real_field_raw_return(field, site);
    let answer = mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        extra,
        raw,
    );
    let answer = if raw {
        real_field_raw_answer(ctx, answer, &field.fdesc)
    } else {
        answer
    };
    if let Some(start) = census {
        crate::mh_finder_census::note_timed("lane", "field-before-linker", start);
    }
    Some(answer)
}

/// `CRATONVM_MH_REAL_FIELD_RAW_RETURN` -- performance, default OFF,
/// `truthy_word` (interpreter round i5 wave 108, lane L4; page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`, "Wave 108").
///
/// The before-linker field lanes boxed a primitive getter's value
/// (`box_value_canonical`: the `IntegerCache` bound and table, or an
/// allocation outside the cache) and the caller unboxed it again
/// (`unbox_poly_return_checked` / `_vh` at the compiled carrier door, the
/// interpreter's stackless door, `invoke_or_native`). The direct lane has
/// handed a primitive of the site's own return type back RAW since round 13
/// (`CRATONVM_MH_DIRECT_LANE_RAW_RETURN`, default on), through the same two
/// natives, so every route that reaches them already keeps a raw primitive
/// as is. With the switch on, a GETTER (`REF_getField` / `REF_getStatic`) of
/// a primitive field whose site returns exactly that primitive answers the
/// raw value; any other shape is boxed as before.
pub(crate) fn real_field_raw_return(field: &RealFieldTarget, site: &str) -> bool {
    crate::nbflags().mh_real_field_raw_return
        && matches!(field.ref_kind, 1 | 2)
        && field.fdesc.len() == 1
        && return_type_desc(site) == field.fdesc
}

/// A raw getter answer of [`real_field_raw_return`], kept raw only when its
/// `Value` variant is the descriptor's own (as `mh_direct_lane_return` keeps
/// it), else boxed as the lane did before.
pub(crate) fn real_field_raw_answer(
    ctx: &mut dyn NativeContext,
    answer: MethodCallResult,
    fdesc: &str,
) -> MethodCallResult {
    match answer {
        Ok(Some(v))
            if matches!(
                (fdesc, &v),
                ("I" | "Z" | "B" | "S" | "C", Value::Int(_))
                    | ("J", Value::Long(_))
                    | ("F", Value::Float(_))
                    | ("D", Value::Double(_))
            ) =>
        {
            Ok(Some(v))
        }
        Ok(Some(v)) => Ok(Some(crate::lang_class::box_value_canonical(ctx, v, fdesc))),
        other => other,
    }
}

/// `CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER` -- performance, default OFF,
/// `truthy_word` (interpreter round i5 wave 102, lane L4; page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`, "Wave 102").
///
/// The method half of [`mh_real_field_before_linker`]: a REAL plain
/// `java/lang/invoke/DirectMethodHandle` of a STATIC (`REF_invokeStatic`) or
/// VIRTUAL (`REF_invokeVirtual`) method -- what `findStatic` / `findVirtual` of
/// a non-varargs, non-caller-sensitive, non-private method answer in both
/// modes -- called by `invokeExact` / `invoke` is handed to
/// [`mh_real_direct_fast`] ahead of [`mh_linked_invoker`]. The linker runs
/// `Invokers$Holder.invokeExact_MT` (`checkExactType`, `checkCustomized`),
/// `invokeBasic` and the handle's `DirectMethodHandle$Holder` form
/// (`invokeStatic` / `invokeVirtual`, plus `ensureInitialized` for a static
/// form that needs it), interpreted, before `linkToStatic` / `linkToVirtual`
/// reaches the member: `L4W102DirectMethodWarmBench`.
///
/// Served only when nothing that chain does can be seen from Java, the same
/// conditions as the field lane:
/// * the site's `MethodType` as the caller resolves it (the site-type row) IS
///   the handle's `type` (so `checkExactType` passes, `invoke`'s `asType`
///   answers the handle itself, no loader cast can fail) and the site has no
///   access refusal (the wave-100 site memo remembers both, keyed by handle
///   and caller);
/// * a virtual call's receiver is non-null (`linkToVirtual`'s NPE and its
///   trace stay the chain's);
/// * a static member's holder is initialized (`ensureInitialized` runs
///   nothing; its `<clinit>` frames stay the chain's);
/// * the native stack is not exhausted.
///
/// `$Special` (kind 7), `$Interface` (kind 9, `checkReceiver`) and
/// `$Constructor` are not served. What the chain did that this skips: its
/// hidden `LambdaForm` frames -- now with Java code below them, so a
/// `StackWalker` with `SHOW_HIDDEN_FRAMES` (or `-XX:+ShowHiddenFrames`
/// traces) inside the target would list two frames fewer; ordinary walks and
/// `Throwable` traces filter them -- and `checkCustomized`'s count. `None`
/// declines; the linker runs, nothing disturbed.
pub(crate) fn mh_real_direct_before_linker(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    armed_caller: u32,
) -> Option<MethodCallResult> {
    // The exact door's tail token, taken first (a decline must not leave it armed) and
    // re-armed for the one dispatch below that is this lane's last act.
    let door_tail = take_mh_tail();
    // Interpreter round i6 wave 117, lane L4 (`CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`,
    // default off): the lane also runs, without its own switch, while this VM's
    // "hidden frames never observed" latch holds (no hidden-frame walk yet, no
    // `-XX:+ShowHiddenFrames`, no agent, debugger or JVMTI listener), and then
    // keeps `checkCustomized`'s count as the skipped linker would
    // ([`real_direct_lane_customization_admits`]).
    let latched = !crate::nbflags().mh_real_direct_before_linker;
    if latched
        && !(crate::nbflags().mh_real_direct_hidden_latch && ctx.hidden_frames_never_observed())
    {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let site = call_site?;
    if ctx.object_num_fields(this) > MH_BOUND {
        return None;
    }
    let census = crate::mh_finder_census::phase_start();
    let record = real_handle_record(ctx, this)?;
    let target = record.direct.as_ref()?;
    if site != record.declared || !matches!(target.ref_kind, 5 | 6) {
        return None;
    }
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        != Some("java/lang/invoke/DirectMethodHandle")
    {
        return None;
    }
    let takes_receiver = target.ref_kind == 5;
    if extra.len() != target.params.len() + usize::from(takes_receiver) {
        return None;
    }
    if takes_receiver {
        if !matches!(extra.first(), Some(Value::Object(Some(_)))) {
            return None;
        }
    } else {
        let owner = target.owner?;
        if !ctx.is_class_initialized(owner) {
            return None;
        }
    }
    if crate::lang_reflect::reflective_native_stack_exhausted() {
        return None;
    }
    if !real_lane_site_admits(&*ctx, this, site, armed_caller) {
        return None;
    }
    // The latched arm counts the call as the linker's `checkCustomized` does,
    // BEFORE the target runs (a nested call of the same handle sees the outer
    // one counted, as on the linker path). A decline of the lane (it declines
    // before any Java runs) takes the count back, since the linker that then
    // runs counts the call itself. At the threshold the lane declines, so the
    // linker customizes the handle exactly when it would have.
    //
    // JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`,
    // default ON): the switched-on arm counts too ([`lane_check_customized`]);
    // it skipped the count, so a direct handle served here was never
    // customized.
    let counted = if latched {
        match real_direct_lane_customization(&*ctx, this) {
            RealDirectCustomization::Customized => None,
            RealDirectCustomization::Count(count) => {
                ctx.set_field_by_name(this, "customizationCount", Value::Int(count + 1));
                Some(count)
            }
            RealDirectCustomization::Due => return None,
        }
    } else {
        lane_check_customized(&*ctx, this)?
    };
    // The site checks are done here (type identity makes the lane's
    // ambiguous-name decline moot), so the lane is asked as a JDK caller.
    set_mh_tail(door_tail);
    let Some(answer) = mh_real_direct_fast(ctx, this, extra, Some(site), false, false) else {
        // Review w3 item 2: the take-back undoes only our own increment.
        lane_uncount_customization(ctx, this, counted);
        return None;
    };
    if let Some(start) = census {
        crate::mh_finder_census::note_timed("lane", "direct-before-linker", start);
    }
    Some(answer)
}

/// What `Invokers.checkCustomized` (JDK 25, run by the `invokeExact_MT` linker
/// the latched direct lane skips) would do for a real handle: nothing for a
/// handle whose form is already customized (`form.customized != null`),
/// `MethodHandle.customize()` once `customizationCount` has reached
/// `CUSTOMIZE_THRESHOLD` (127, `MethodHandleStatics`), else count the call.
/// Interpreter round i6 wave 117, lane L4 (`CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`).
pub(crate) enum RealDirectCustomization {
    /// The form is customized: nothing to count.
    Customized,
    /// Not yet due: the current count, which the call raises by one.
    Count(i32),
    /// Due (or unreadable): the lane declines and the linker customizes.
    Due,
}

/// `CUSTOMIZE_THRESHOLD`'s JDK 25 default (`java.lang.invoke.MethodHandleStatics`).
pub(crate) const MH_CUSTOMIZE_THRESHOLD: i32 = 127;

/// [`RealDirectCustomization`] of `mh` (pure reads).
pub(crate) fn real_direct_lane_customization(ctx: &dyn NativeContext, mh: ObjectRef) -> RealDirectCustomization {
    let Value::Object(Some(form)) = ctx.get_field_by_name(mh, "form") else {
        return RealDirectCustomization::Due;
    };
    match ctx.get_field_by_name(form, "customized") {
        Value::Object(Some(_)) => return RealDirectCustomization::Customized,
        Value::Object(None) => {}
        _ => return RealDirectCustomization::Due,
    }
    // `-Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=<n>` moves the
    // threshold (or, `-1`, turns customization off): the lane leaves every
    // such VM's calls to the linker.
    if !customize_threshold_is_default(ctx) {
        return RealDirectCustomization::Due;
    }
    match ctx.get_field_by_name(mh, "customizationCount") {
        Value::Int(count) if count < MH_CUSTOMIZE_THRESHOLD => RealDirectCustomization::Count(count),
        _ => RealDirectCustomization::Due,
    }
}

/// JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`,
/// HotSpot parity, default ON, `=0` the old answer; page
/// `docs/feature-designs/class-resolution-and-caching.md`):
/// `Invokers.checkCustomized(mh)` for a before-linker lane that is about to
/// serve `mh`'s call in place of `Invokers$Holder.invokeExact_MT` (or the
/// generic invoker, whose `asType` answers `mh` itself at the site the lanes
/// admit). Measured with `R22RuntimeCtorCustomize`: HotSpot `mh-128
/// customized=true count=127`, CratonVM `count=1` for ever -- the first call
/// went to the linker (the instance class was not initialized yet, so the
/// constructor lane declined), every later one to the lane, which skipped the
/// count, so the handle was never customized and its frame stayed
/// `DirectMethodHandle$Holder.newInvokeSpecial`.
///
/// Cost per served call: the three slots are resolved once per VM
/// ([`lane_customize_slots`]); a customized handle costs two slot reads and
/// no write, an uncustomized one (at most 127 calls per handle) three reads
/// and one write.
///
/// `Some(Some(previous))`: counted (`previous + 1` written; a lane that then
/// declines writes `previous` back with [`lane_uncount_customization`], since
/// the linker that runs instead counts the call itself). `Some(None)`: nothing
/// to count (a customized form, or the switch off). `None`: the call is due
/// (`customize()` at `CUSTOMIZE_THRESHOLD`), unreadable, or under a
/// non-default threshold -- the lane declines and the JDK's own linker runs
/// `checkCustomized`, so the handle is customized exactly on HotSpot's call.
/// After that the lane serves the customized handle again (`Customized`).
pub fn lane_check_customized(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<Option<i32>> {
    if !crate::nbflags().mh_lanes_keep_customization {
        return Some(None);
    }
    let slots = lane_customize_slots(ctx, mh);
    match lane_customization(ctx, mh, slots) {
        RealDirectCustomization::Customized => Some(None),
        RealDirectCustomization::Count(count) => {
            lane_write_count(ctx, mh, slots, count + 1);
            Some(Some(count))
        }
        RealDirectCustomization::Due => lane_due_once(ctx, mh),
    }
}

pub(crate) const LANE_DUE_MEMO_SLOTS: usize = 16;

thread_local! {
    /// `(vm, address epoch, handle address)` of the handles whose due call a
    /// lane already declined to the linker on this thread ([`lane_due_once`]).
    pub(crate) static LANE_DUE_MEMO: std::cell::Cell<[(usize, u64, usize); LANE_DUE_MEMO_SLOTS]> =
        const { std::cell::Cell::new([(0, 0, 0); LANE_DUE_MEMO_SLOTS]) };
}

/// JIT round 22 wave 4, lane runtime (self-review of the wave-3 count): the
/// due call declines to the linker once per handle (per thread and address
/// epoch), whose `checkCustomized` then customizes it -- after which the lanes
/// see `Customized` and never ask again. A handle still uncustomized after
/// that (an unreadable form, or a customization the JDK could not complete)
/// would otherwise send EVERY later
/// call through the linker, a ~10-100x slowdown of the lane's call for the
/// handle's life; such a handle is served (`Some(None)`: no count) from the
/// second due call on. A mock VM (`vm_identity() == 0`) keeps the plain decline.
///
/// Review w5 item 1 (lane intrinsics): a non-default `CUSTOMIZE_THRESHOLD`
/// is NOT memoized. [`lane_customization`] answers `Due` for every call under
/// it (the lanes cannot count against a threshold they do not read), so the
/// linker must run every call to count it; a memo would stop the count at 1
/// and the handle would never be customized (HotSpot customizes it after
/// `CUSTOMIZE_THRESHOLD` calls). The slowdown is bounded by the threshold.
/// `-1` is served uncounted from the first call: checked against the JDK 25
/// source, `Invokers.invokeHandleForm` and `varHandleMethodGenericLinkerHandleForm` emit no
/// `checkCustomized` name when `CUSTOMIZE_THRESHOLD < 0` (`CHECK_CUSTOM = -1`),
/// so the generic / exact invokers the lanes stand in for never count. (The
/// only other caller of `maybeCustomize`, `CountingWrapper.countDown`, is not
/// on a lane's path.) The value is parsed as `Integer.parseInt` would
/// (`-01`, `-1`), whose `<clinit>` rejects anything below `-1`.
pub(crate) fn lane_due_once(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<Option<i32>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let threshold = customize_threshold(ctx);
    if threshold != MH_CUSTOMIZE_THRESHOLD {
        // The JDK's own value (wave 136, lane L3), not the property's.
        return (threshold == -1).then_some(None);
    }
    // Build w4 regression (R22W4RuntimeEdges `mh-fresh-thresholds`, the 128th
    // call from a compiled loop left the handle uncustomized): this used to
    // WRITE the memo on the first due ask. One call asks more than once -- the
    // compiled constant bind (`helpers::mh_bind_customization_serves`) declines
    // its due call to the door, whose before-linker lane then asked again,
    // found the line and served the call, so no linker ever ran. The memo is
    // now written only where the linker actually runs for the handle
    // ([`lane_note_due_linker_ran`], in [`mh_linked_invoker`]); every ask before
    // that declines.
    let key = (vm, ctx.gc_address_epoch(), mh.as_ptr() as usize);
    let slot = (key.2 >> 3) & (LANE_DUE_MEMO_SLOTS - 1);
    LANE_DUE_MEMO.with(|memo| (memo.get()[slot] == key).then_some(None))
}

/// The JDK linker is about to run for `mh` ([`mh_linked_invoker`]): when the
/// handle's call is due (uncustomized, count at the default threshold), file
/// it for [`lane_due_once`], so that if the linker's `checkCustomized` leaves
/// it uncustomized, later due calls are served instead of all paying the
/// linker. Nothing is filed for any other state, a mock VM, the switch off or
/// a non-default threshold (whose calls are all the linker's).
pub(crate) fn lane_note_due_linker_ran(ctx: &dyn NativeContext, mh: ObjectRef) {
    let vm = ctx.vm_identity();
    if vm == 0 || !crate::nbflags().mh_lanes_keep_customization || !customize_threshold_is_default(ctx) {
        return;
    }
    let slots = lane_customize_slots(ctx, mh);
    if !matches!(lane_customization(ctx, mh, slots), RealDirectCustomization::Due) {
        return;
    }
    let key = (vm, ctx.gc_address_epoch(), mh.as_ptr() as usize);
    let slot = (key.2 >> 3) & (LANE_DUE_MEMO_SLOTS - 1);
    LANE_DUE_MEMO.with(|memo| {
        let mut lines = memo.get();
        lines[slot] = key;
        memo.set(lines);
    });
}

/// Takes back [`lane_check_customized`]'s count (`previous`) when the lane
/// declines after counting, unless the form was customized meanwhile.
///
/// Review w3 item 2 (lane sync): a plain write of `previous` erased another
/// thread's increment made between the count and the take-back (HotSpot's
/// racy `++` never decrements), delaying customization. The take-back is now
/// a compare-and-swap from `previous + 1`: it undoes only our own increment,
/// and does nothing once another thread has counted on top of it (the linker
/// that then runs counts this call too, so the count is one high, as HotSpot's
/// can be under its own race; never a lost increment).
pub(crate) fn lane_uncount_customization(ctx: &mut dyn NativeContext, mh: ObjectRef, previous: Option<i32>) {
    let Some(previous) = previous else {
        return;
    };
    let slots = lane_customize_slots(&*ctx, mh);
    if !matches!(
        lane_customization(&*ctx, mh, slots),
        RealDirectCustomization::Count(_) | RealDirectCustomization::Due
    ) {
        return;
    }
    match slots {
        Some(slots) => {
            let _ = ctx.compare_and_swap_field(
                mh,
                slots.count,
                Value::Int(previous + 1),
                Value::Int(previous),
            );
        }
        // Mock VMs and unresolved layouts (no concurrency there to speak of):
        // the same rule by name.
        None => {
            if ctx.get_field_by_name(mh, "customizationCount") == Value::Int(previous + 1) {
                ctx.set_field_by_name(mh, "customizationCount", Value::Int(previous));
            }
        }
    }
}

/// The three slots [`lane_check_customized`] reads: `MethodHandle.form`,
/// `MethodHandle.customizationCount` and `LambdaForm.customized`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LaneCustomizeSlots {
    pub(crate) form: usize,
    pub(crate) count: usize,
    pub(crate) customized: usize,
}

thread_local! {
    /// `(vm_identity, slots)` of [`lane_customize_slots`]: resolved once per VM
    /// (the JDK's own `MethodHandle` / `LambdaForm` layout, fixed for the VM's
    /// life), as [`CUSTOMIZE_THRESHOLD_SETTLED`] keys its fact.
    pub(crate) static LANE_CUSTOMIZE_SLOTS: std::cell::Cell<(usize, Option<LaneCustomizeSlots>)> =
        const { std::cell::Cell::new((0, None)) };
}

/// JIT r22 retirement (lane inline; the wave-3 count's measured cost): the
/// `(form, customized, widest handle)` that [`lane_customize_slots`] already
/// resolved on this thread for VM `vm` -- no resolution, no context. The
/// compiled constant bind (`helpers::mh_bind_customization_serves`) reads the
/// two slots raw for its steady state, a customized handle, which is
/// [`lane_check_customized`]'s `Customized` arm (`Some(None)`): through the
/// context its two reads paid the native door's descriptor-aware field read
/// each, ~240 ns a call on `R22InlineMhCustomize`. `None` until the slow path
/// has resolved the slots once, and under
/// `CRATONVM_MH_LANES_KEEP_CUSTOMIZATION=0` (which serves uncounted anyway).
pub fn lane_customize_slots_resolved(vm: usize) -> Option<(usize, usize, usize)> {
    if vm == 0 || !crate::nbflags().mh_lanes_keep_customization {
        return None;
    }
    let (seen, cached) = LANE_CUSTOMIZE_SLOTS.with(|c| c.get());
    let slots = cached.filter(|_| seen == vm)?;
    Some((slots.form, slots.customized, MH_BOUND))
}

/// The slots the compiled constant bind counts an UNcustomized real handle's call
/// through, raw (j27 wave 2, lane INVOKE; [`lane_raw_count_slots`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaneRawCountSlots {
    /// `MethodHandle.form`.
    pub form: usize,
    /// `MethodHandle.customizationCount` (a `byte`).
    pub count: usize,
    /// `LambdaForm.customized`.
    pub customized: usize,
    /// The widest real handle (wider is this model's carrier).
    pub widest: usize,
}

/// `CRATONVM_JIT_MH_BIND_COUNT_RAW` -- perf, default ON, `=0` off (j27 wave 2, lane
/// INVOKE; page `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`,
/// `bind-each`): the compiled constant bind counts `Invokers.checkCustomized` for
/// an uncustomized real handle from raw slots ([`lane_raw_count_slots`]) instead of
/// building a context and paying the native door's descriptor-aware field read
/// four times (`lane_check_customized`). A shape hit (`bindTo` in a loop: a fresh,
/// never customized handle every call) took that context path on EVERY call.
/// Memoised (asked per counted call).
fn mh_bind_raw_count_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_BIND_COUNT_RAW");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// [`LaneRawCountSlots`] for VM `vm`, once THIS thread's counting path settled
/// both facts it would ask: the slots ([`lane_customize_slots`]) and a default
/// `CUSTOMIZE_THRESHOLD` ([`customize_threshold`]). `None` -- ask
/// [`lane_check_customized`] -- before that, under a non-default threshold, with
/// `CRATONVM_MH_LANES_KEEP_CUSTOMIZATION=0` or `CRATONVM_JIT_MH_BIND_COUNT_RAW=0`.
/// No resolution, no context.
pub fn lane_raw_count_slots(vm: usize) -> Option<LaneRawCountSlots> {
    if vm == 0 || !crate::nbflags().mh_lanes_keep_customization || !mh_bind_raw_count_enabled() {
        return None;
    }
    let (seen, threshold) = CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.get());
    if seen != vm || threshold != MH_CUSTOMIZE_THRESHOLD {
        return None;
    }
    let (seen, cached) = LANE_CUSTOMIZE_SLOTS.with(|c| c.get());
    let slots = cached.filter(|_| seen == vm)?;
    Some(LaneRawCountSlots {
        form: slots.form,
        count: slots.count,
        customized: slots.customized,
        widest: MH_BOUND,
    })
}

/// [`lane_customization`]'s `Count` arm on a raw `customizationCount` read of a
/// handle whose form is NOT customized: the count to write back (`count + 1`),
/// or `None` -- the call is due (`customize()` at `CUSTOMIZE_THRESHOLD`) or the
/// slot is unreadable, and [`lane_check_customized`] decides.
pub fn lane_raw_count_step(count: Value) -> Option<i32> {
    match count {
        Value::Int(count) if count < MH_CUSTOMIZE_THRESHOLD => Some(count + 1),
        _ => None,
    }
}

/// The slots of [`LaneCustomizeSlots`] for the real handle `mh`, resolved once
/// per VM (wave-3 tightening: the lanes read the count per served call, and
/// three by-name lookups a call were the cost), or `None` -- an unresolved
/// layout, a mock VM (`vm_identity() == 0`), or a handle wider than the real
/// layout (this model's carrier slots) -- where the by-name reads answer.
pub(crate) fn lane_customize_slots(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<LaneCustomizeSlots> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let (seen, cached) = LANE_CUSTOMIZE_SLOTS.with(|c| c.get());
    let slots = if seen == vm {
        cached
    } else {
        let resolved = (|| {
            Some(LaneCustomizeSlots {
                form: ctx.resolve_field_index("java/lang/invoke/MethodHandle", "form")?,
                count: ctx
                    .resolve_field_index("java/lang/invoke/MethodHandle", "customizationCount")?,
                customized: ctx.resolve_field_index("java/lang/invoke/LambdaForm", "customized")?,
            })
        })();
        // Review w3 item 2: only a resolved layout is cached; a `None` (the
        // first call ran before `LambdaForm` was linked) is asked again.
        if resolved.is_some() {
            LANE_CUSTOMIZE_SLOTS.with(|c| c.set((vm, resolved)));
        }
        resolved
    }?;
    let fields = ctx.object_num_fields(mh);
    (fields <= MH_BOUND && slots.form < fields && slots.count < fields).then_some(slots)
}

/// [`real_direct_lane_customization`] by slot: once the form is customized,
/// two slot reads and nothing else (the steady state of every served handle);
/// before that, the count too. Exactness forbids answering from the count
/// alone: HotSpot's count is 127 both on the due call and after `customize()`.
pub(crate) fn lane_customization(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    slots: Option<LaneCustomizeSlots>,
) -> RealDirectCustomization {
    let Some(slots) = slots else {
        return real_direct_lane_customization(ctx, mh);
    };
    let Value::Object(Some(form)) = ctx.get_field(mh, slots.form) else {
        return RealDirectCustomization::Due;
    };
    if slots.customized >= ctx.object_num_fields(form) {
        return RealDirectCustomization::Due;
    }
    match ctx.get_field(form, slots.customized) {
        Value::Object(Some(_)) => return RealDirectCustomization::Customized,
        Value::Object(None) => {}
        _ => return RealDirectCustomization::Due,
    }
    if !customize_threshold_is_default(ctx) {
        return RealDirectCustomization::Due;
    }
    match ctx.get_field(mh, slots.count) {
        Value::Int(count) if count < MH_CUSTOMIZE_THRESHOLD => RealDirectCustomization::Count(count),
        _ => RealDirectCustomization::Due,
    }
}

/// Writes `customizationCount` (by slot when resolved).
pub(crate) fn lane_write_count(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    slots: Option<LaneCustomizeSlots>,
    count: i32,
) {
    match slots {
        Some(slots) => ctx.set_field(mh, slots.count, Value::Int(count)),
        None => ctx.set_field_by_name(mh, "customizationCount", Value::Int(count)),
    }
}

thread_local! {
    /// `(vm_identity, threshold)` of [`customize_threshold`]: the JDK's own
    /// `MethodHandleStatics.CUSTOMIZE_THRESHOLD`, a fixed per-VM fact once its
    /// `<clinit>` has run, keyed by the VM.
    pub(crate) static CUSTOMIZE_THRESHOLD_SETTLED: std::cell::Cell<(usize, i32)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// A threshold no `Integer.parseInt` of the property gives the JDK: a value
/// `MethodHandleStatics.<clinit>` refuses (unparseable, or outside `[-1, 127]`).
pub(crate) const MH_CUSTOMIZE_THRESHOLD_UNUSABLE: i32 = i32::MIN;

/// `MethodHandleStatics.CUSTOMIZE_THRESHOLD` as the JDK holds it.
///
/// Interpreter round i7 wave 136, lane L3 (cross-review of wave 135 L4's
/// suspicion, page `i110-L4`): the answer used to be the system property,
/// cached per thread on its first ask. The JDK reads the property ONCE, in
/// `MethodHandleStatics.<clinit>`, so a `System.setProperty` after that moves
/// nothing on HotSpot, while here every thread whose first ask came after the
/// write took the new value (a `-1` written at run time made the lanes serve
/// handles uncounted, never customized). Now the JDK's own static answers once
/// the class is initialized, and only that answer is cached; before the
/// `<clinit>` (or in a mock) the property is parsed as the `<clinit>` would,
/// uncached. The `-D` value itself was always readable from the first call
/// (the VM's property store is filled before any Java runs).
pub(crate) fn customize_threshold(ctx: &dyn NativeContext) -> i32 {
    let vm = ctx.vm_identity();
    if vm != 0 {
        let (seen, threshold) = CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.get());
        if seen == vm {
            return threshold;
        }
        if let Some(cid) = ctx.class_id_by_name("java/lang/invoke/MethodHandleStatics") {
            if ctx.is_class_initialized(cid) {
                if let Some(index) = ctx.static_field_index_by_name(cid, "CUSTOMIZE_THRESHOLD") {
                    if let Value::Int(threshold) = ctx.get_static_field(cid, index) {
                        CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.set((vm, threshold)));
                        return threshold;
                    }
                }
            }
        }
    }
    match ctx.get_system_property("java.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD") {
        None => MH_CUSTOMIZE_THRESHOLD,
        // Parsed as `MethodHandleStatics.<clinit>`'s `Integer.parseInt` does
        // (`+127` and `0127` are the default too; wave 135, lane L4).
        Some(v) => match v.parse::<i32>() {
            // `<clinit>` refuses anything outside [-1, 127].
            Ok(n) if (-1..=MH_CUSTOMIZE_THRESHOLD).contains(&n) => n,
            _ => MH_CUSTOMIZE_THRESHOLD_UNUSABLE,
        },
    }
}

/// Is `MethodHandleStatics.CUSTOMIZE_THRESHOLD` this VM's default (127)?
pub(crate) fn customize_threshold_is_default(ctx: &dyn NativeContext) -> bool {
    customize_threshold(ctx) == MH_CUSTOMIZE_THRESHOLD
}

// ---------------------------------------------------------------------------
// Interpreter round i6 wave 132, lane L4: the invoker forms' per-call checks as
// `Intrinsic`s (`CRATONVM_MH_INVOKER_CHECKS_INTRINSIC`, default off, no-JIT VMs)
// ---------------------------------------------------------------------------
//
// Page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 132":
// every interpreted `Invokers$Holder.invokeExact_MT` (the `invokeExact` linker)
// and every spread / generic invoker form calls `Invokers.checkExactType(mh,
// type)` (`mh.type()` and a compare: two interpreted invocations) and
// `Invokers.checkCustomized(mh)` (`MethodHandleImpl.isCompileConstant`, a
// `form.customized` read and, until the form is customized,
// `Invokers.maybeCustomize` -> `MethodHandle.maybeCustomize`: four). The body
// arm of `invokeWithArguments` pays both for the spreader (whose count the
// spreader memo resets, so `maybeCustomize` runs on every call, as on HotSpot's
// fresh spreader) and `checkCustomized` again for the adapter: about eight
// interpreted invocations a call that leave no frame behind. Here each check is
// one native: the plain answer (the same type; a customized form; a count below
// the default threshold, raised by one) is given from field reads, and anything
// else (another type, so a `WrongMethodTypeException` with the JDK's own frames;
// a due customization; a non-default `CUSTOMIZE_THRESHOLD`; a null or carrier
// handle) runs the JDK's own body (`invokers_check_jdk_body`). No frame of
// either method is ever on a stack a walk or a throwable sees (both return
// before the call they guard), except a throwable raised inside them, which the
// body raises. Registered only in a VM without a JIT (an `Intrinsic` over JDK
// bytecode is never compiled: wave 75's `makeImpl` lesson; both are
// `@ForceInline` and the JIT inlines them), as `MethodType.makeImpl`'s.
// Removal: when the interpreter's own invocation cost makes the bodies cheap
// (the bench `L4W109InvokeWithArgumentsBodyBench` and `L4W102DirectMethodWarmBench`
// rows decide), or never if it is flipped on.

/// `java/lang/invoke/Invokers`.
pub(crate) const INVOKERS_CLASS: &str = "java/lang/invoke/Invokers";
/// `Invokers.checkCustomized(MethodHandle)`.
pub(crate) const INVOKERS_CHECK_CUSTOMIZED_DESC: &str = "(Ljava/lang/invoke/MethodHandle;)V";
/// `Invokers.checkExactType(MethodHandle, MethodType)`.
pub(crate) const INVOKERS_CHECK_EXACT_TYPE_DESC: &str =
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V";
/// The rows' token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (as `method-type-intern`).
pub(crate) const INVOKERS_CHECKS_BODY_ROW: &str = "invoker-checks";

/// Does `r`'s VM get the two invoker-check `Intrinsic`s? The switch, and no JIT
/// (`interpreted_intrinsic_registers_row`, as [`mh_method_type_intern_memo_registers`]).
pub(crate) fn mh_invoker_checks_registers(r: &NativeMethodRegistry) -> bool {
    crate::nbflags().mh_invoker_checks_intrinsic
        && r.interpreted_intrinsic_registers_row(INVOKERS_CHECKS_BODY_ROW)
}

/// The JDK's own `Invokers.<name>` body (static), for every call the two
/// `Intrinsic`s do not answer themselves.
pub(crate) fn invokers_check_jdk_body(
    ctx: &mut dyn NativeContext,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let _transparent = cratonvm_native_api::continuation_pin::transparent();
    ctx.invoke_special_bytecode_only("java/lang/invoke/Invokers", name, descriptor, args)
}

/// `Invokers.checkCustomized(MethodHandle mh)` (JDK 25):
/// `isCompileConstant(mh)` is `false` in an interpreted form; then nothing for
/// `mh.form.customized != null`, else `mh.maybeCustomize()`: the count raised by
/// one below `CUSTOMIZE_THRESHOLD`, `customize()` at it. The first two are
/// answered here ([`real_direct_lane_customization`]'s reads, the default
/// threshold only); the due call and everything unreadable run the body.
pub(crate) fn invokers_check_customized_intrinsic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(mh))) = args.first().copied() {
        if ctx.vm_identity() != 0 && !crate::nbflags().dbg_mh_dispatch {
            // Interpreter round i7 wave 135, lane L4 (page i110-L4, "Wave 135"):
            // the reads go by the per-VM slots the before-linker lanes resolve
            // once (`lane_customize_slots`; JIT round 22 wave 3), not by name.
            // Three by-name lookups a call were the `Intrinsic`'s own cost (the
            // ~300 ns small-`Intrinsic` field-read cost). An unresolved layout
            // or a handle wider than the real layout answers by name, as before.
            let slots = lane_customize_slots(&*ctx, mh);
            match lane_customization(&*ctx, mh, slots) {
                RealDirectCustomization::Customized => return Ok(None),
                RealDirectCustomization::Count(count) => {
                    lane_write_count(&*ctx, mh, slots, count + 1);
                    return Ok(None);
                }
                RealDirectCustomization::Due => {}
            }
        }
    }
    invokers_check_jdk_body(ctx, "checkCustomized", INVOKERS_CHECK_CUSTOMIZED_DESC, args)
}

/// `Invokers.checkExactType(MethodHandle mh, MethodType expected)` (JDK 25):
/// `mh.type()` (final: the `type` field) compared by identity with `expected`
/// (interned). The same object answers here; anything else (another type: the
/// body raises `newWrongMethodTypeException` with its own frames; a null
/// handle; a carrier whose `type` field is unset, which its `type()` native
/// fills) runs the body.
pub(crate) fn invokers_check_exact_type_intrinsic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(mh))), Some(Value::Object(Some(expected)))) =
        (args.first().copied(), args.get(1).copied())
    {
        if ctx.vm_identity() != 0 && !crate::nbflags().dbg_mh_dispatch {
            // Wave 135 (lane L4): by the per-VM `MethodHandle.type` slot.
            let actual = match invokers_mh_type_slot(&*ctx, mh) {
                Some(slot) => ctx.get_field(mh, slot),
                None => ctx.get_field_by_name(mh, "type"),
            };
            if let Value::Object(Some(actual)) = actual {
                if actual.as_ptr() == expected.as_ptr() {
                    return Ok(None);
                }
            }
        }
    }
    invokers_check_jdk_body(ctx, "checkExactType", INVOKERS_CHECK_EXACT_TYPE_DESC, args)
}

thread_local! {
    /// `(vm_identity, slot)` of [`invokers_mh_type_slot`]: `MethodHandle.type`'s
    /// slot, resolved once per VM (a fixed fact of the JDK's layout), as
    /// `LANE_CUSTOMIZE_SLOTS` keys its slots. Interpreter round i7 wave 135, lane L4.
    pub(crate) static INVOKERS_MH_TYPE_SLOT: std::cell::Cell<(usize, Option<usize>)> =
        const { std::cell::Cell::new((0, None)) };
}

/// `MethodHandle.type`'s slot for `mh`, or `None` (a mock VM, an unresolved
/// layout, a slot past `mh`'s fields): the caller then reads by name.
pub(crate) fn invokers_mh_type_slot(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<usize> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let (seen, cached) = INVOKERS_MH_TYPE_SLOT.with(|c| c.get());
    let slot = if seen == vm {
        cached
    } else {
        let resolved = ctx.resolve_field_index("java/lang/invoke/MethodHandle", "type");
        // Only a resolved slot is cached (as `lane_customize_slots`).
        if resolved.is_some() {
            INVOKERS_MH_TYPE_SLOT.with(|c| c.set((vm, resolved)));
        }
        resolved
    }?;
    (slot < ctx.object_num_fields(mh)).then_some(slot)
}

// ---------------------------------------------------------------------------
// Interpreter round i7 wave 137, lane L4: two frameless `LambdaForm` leaf
// helpers as `Intrinsic`s (`CRATONVM_MH_LF_LEAF_INTRINSICS`, default off,
// no-JIT VMs)
// ---------------------------------------------------------------------------
//
// Page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 137":
// after wave 132's invoker checks, the leaf calls of the body arm's chain that
// leave no frame behind are `MethodHandleImpl.checkSpreadArgument(av, n)` (the
// spread form's guard: `av` is an array of length `n`) and
// `DirectMethodHandle.internalMemberName(mh)` (`((DirectMethodHandle) mh).member`,
// called by every `DirectMethodHandle$Holder` form before its `linkTo*`). Each is
// one interpreted invocation (~400 ns under `--nojit`). Neither reads a field
// the wave-135 note warned about by name: the first reads the array's heap kind
// and length, the second one slot resolved once per VM. The plain answer is given
// here; anything else (a null array with `n != 0`, a length mismatch, a
// non-array, a receiver that is not a `DirectMethodHandle`) runs the JDK's own
// body (an `invoke_special_bytecode_only`), which raises with its own frames. Both are
// `@ForceInline` and return before the call they guard, so no walk sees them.
// Registered only in a VM without a JIT (`interpreted_intrinsic_registers_row`,
// row `lf-leaf-helpers`), as the invoker checks. Removal: as the invoker checks'.

/// `java/lang/invoke/MethodHandleImpl`.
pub(crate) const LF_LEAF_MHI_CLASS: &str = "java/lang/invoke/MethodHandleImpl";
/// `MethodHandleImpl.checkSpreadArgument(Object, int)`.
pub(crate) const LF_LEAF_CHECK_SPREAD_DESC: &str = "(Ljava/lang/Object;I)V";
/// `java/lang/invoke/DirectMethodHandle`.
pub(crate) const LF_LEAF_DMH_CLASS: &str = "java/lang/invoke/DirectMethodHandle";
/// `DirectMethodHandle.internalMemberName(Object)` (static, JDK 25).
pub(crate) const LF_LEAF_INTERNAL_MEMBER_DESC: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
/// The rows' token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`.
pub(crate) const LF_LEAF_BODY_ROW: &str = "lf-leaf-helpers";

/// Does `r`'s VM get the two leaf-helper `Intrinsic`s? The switch, and no JIT.
pub(crate) fn mh_lf_leaf_registers(r: &NativeMethodRegistry) -> bool {
    crate::nbflags().mh_lf_leaf_intrinsics && r.interpreted_intrinsic_registers_row(LF_LEAF_BODY_ROW)
}

/// `MethodHandleImpl.checkSpreadArgument(Object av, int n)` (JDK 25): returns
/// for `av == null && n == 0`, and for an array of length `n` (an `Object[]`
/// by `arraylength`, any other array by `Array.getLength`: the same answer).
/// Everything else throws in the body (`NullPointerException("null array
/// reference")`, `Array.getLength`'s `IllegalArgumentException` for a
/// non-array, `newIllegalArgumentException("array is not of length n")`).
pub(crate) fn mhi_check_spread_argument_intrinsic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if ctx.vm_identity() != 0 && !crate::nbflags().dbg_mh_dispatch {
        if let Some(Value::Int(n)) = args.get(1).copied() {
            match args.first().copied() {
                Some(Value::Object(None)) if n == 0 => return Ok(None),
                Some(Value::Object(Some(av))) => {
                    // By the heap kind: an array's header carries its
                    // COMPONENT's class id (see `mhp_wrapper_handle`).
                    let is_array = matches!(ctx.heap_kind_of(av), ObjectKind::Array);
                    if is_array && i32::try_from(ctx.array_length(av)).ok() == Some(n) {
                        return Ok(None);
                    }
                }
                _ => {}
            }
        }
    }
    // The JDK's own body, for every call not answered above.
    let _transparent = cratonvm_native_api::continuation_pin::transparent();
    ctx.invoke_special_bytecode_only(
        LF_LEAF_MHI_CLASS,
        "checkSpreadArgument",
        LF_LEAF_CHECK_SPREAD_DESC,
        args,
    )
}

thread_local! {
    /// `(vm_identity, DirectMethodHandle's class id + 1, member slot + 1, last
    /// admitted receiver class id + 1)` of [`dmh_member_slot`]; `0` words are
    /// unresolved. Fixed facts of the JDK's layout, per VM. The admitted class
    /// is `DirectMethodHandle` or one of its `java.base` subclasses (the class
    /// is package-private territory: no application class extends it), all
    /// boot classes that never unload, so the id cannot be reissued.
    pub(crate) static LF_LEAF_DMH_MEMBER: std::cell::Cell<(usize, u32, usize, u32)> =
        const { std::cell::Cell::new((0, 0, 0, 0)) };
}

/// `DirectMethodHandle.member`'s slot when `mh` is a `DirectMethodHandle`
/// (or a subclass: the checkcast's question), else `None` (a mock VM, an
/// unresolved layout, a carrier or any other object: the body answers).
pub(crate) fn dmh_member_slot(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<usize> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let (seen, dmh_plus, slot_plus, last_plus) = LF_LEAF_DMH_MEMBER.with(|c| c.get());
    let (dmh_plus, slot_plus, last_plus) = if seen == vm && dmh_plus != 0 && slot_plus != 0 {
        (dmh_plus, slot_plus, last_plus)
    } else {
        let dmh = ctx.class_id_by_name(LF_LEAF_DMH_CLASS)?;
        let slot = ctx.resolve_field_index(LF_LEAF_DMH_CLASS, "member")?;
        // Only a resolved layout is cached (as `lane_customize_slots`).
        let resolved = (dmh.as_u32().checked_add(1)?, slot.checked_add(1)?, 0);
        LF_LEAF_DMH_MEMBER.with(|c| c.set((vm, resolved.0, resolved.1, 0)));
        resolved
    };
    let dmh = ClassId::new(dmh_plus - 1);
    let slot = slot_plus - 1;
    let class = ctx.class_id_of_object(mh);
    let class_plus = class.as_u32().checked_add(1)?;
    if class_plus != last_plus {
        if class != dmh && !ctx.is_subclass(class, dmh) {
            return None;
        }
        LF_LEAF_DMH_MEMBER.with(|c| c.set((vm, dmh_plus, slot_plus, class_plus)));
    }
    (slot < ctx.object_num_fields(mh)).then_some(slot)
}

/// `DirectMethodHandle.internalMemberName(Object mh)` (JDK 25):
/// `((DirectMethodHandle) mh).member`. A `DirectMethodHandle` receiver answers
/// its `member` slot here; a null or any other receiver runs the body (whose
/// `checkcast` raises the `ClassCastException`, or answers through a null).
pub(crate) fn dmh_internal_member_name_intrinsic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(mh))) = args.first().copied() {
        if !crate::nbflags().dbg_mh_dispatch {
            if let Some(slot) = dmh_member_slot(&*ctx, mh) {
                if let member @ Value::Object(_) = ctx.get_field(mh, slot) {
                    return Ok(Some(member));
                }
            }
        }
    }
    // The JDK's own body, for every call not answered above.
    let _transparent = cratonvm_native_api::continuation_pin::transparent();
    ctx.invoke_special_bytecode_only(
        LF_LEAF_DMH_CLASS,
        "internalMemberName",
        LF_LEAF_INTERNAL_MEMBER_DESC,
        args,
    )
}

/// The per-call guards of [`mh_real_field_before_linker`] (and of
/// [`mh_real_bound_field_before_linker`]), for the real field `accessor` whose
/// record is `field`, called with `extra` (the base first for an instance
/// accessor, then the value for a setter): the arity, a non-null base, an
/// initialized holder, a reference value the setter's `checkCast` admits, and
/// native stack headroom. Pure reads. Split out of
/// [`mh_real_field_before_linker`] in interpreter round i5 wave 101 (lane L4),
/// unchanged.
pub(crate) fn real_field_call_admits(
    ctx: &dyn NativeContext,
    accessor: ObjectRef,
    field: &RealFieldTarget,
    extra: &[Value],
) -> bool {
    let is_static = matches!(field.ref_kind, 2 | 4);
    let is_put = matches!(field.ref_kind, 3 | 4);
    let lead = usize::from(!is_static);
    if extra.len() != lead + usize::from(is_put) {
        return false;
    }
    if !is_static && !matches!(extra.first(), Some(Value::Object(Some(_)))) {
        return false;
    }
    if let Some((class_id, _)) = field.target {
        if !ctx.is_class_initialized(class_id) {
            return false;
        }
    }
    if is_put && field.fdesc.len() != 1 && field.fdesc != "Ljava/lang/Object;" {
        match extra.get(lead) {
            Some(Value::Object(None)) => {}
            Some(Value::Object(Some(value))) => {
                let Value::Object(Some(member)) = ctx.get_field_by_name(accessor, "member") else {
                    return false;
                };
                let Value::Object(Some(ftype)) = ctx.get_field_by_name(member, "type") else {
                    return false;
                };
                let Some(fclass) = ctx.class_id_from_mirror(ftype) else {
                    return false;
                };
                if ctx.class_id_of_object(*value) != fclass {
                    return false;
                }
            }
            _ => return false,
        }
    }
    !crate::lang_reflect::reflective_native_stack_exhausted()
}

/// The class of `getter.bindTo(base)` for a real instance field accessor: the
/// JDK's `DirectMethodHandle.rebind()` reinvoker (`Species_L`) extended with the
/// bound value.
pub(crate) const BMH_SPECIES_LL: &str = "java/lang/invoke/BoundMethodHandle$Species_LL";

/// Does the call site `site` name the type of `declared` with its first
/// parameter dropped (what `bindTo` leaves of an instance accessor's type)?
/// Pure, no allocation.
pub(crate) fn bound_site_matches(declared: &str, site: &str) -> bool {
    let Some(params) = declared.strip_prefix('(') else {
        return false;
    };
    let bytes = params.as_bytes();
    let mut i = 0usize;
    while bytes.get(i) == Some(&b'[') {
        i += 1;
    }
    let first_len = match bytes.get(i) {
        Some(b'L') => match params[i..].find(';') {
            Some(semi) => i + semi + 1,
            None => return false,
        },
        Some(b')') | None => return false,
        Some(_) => i + 1,
    };
    site.strip_prefix('(')
        .is_some_and(|rest| rest == &params[first_len..])
}

/// One line of [`BOUND_FIELD_MEMO`]: the bound handle (`addr`) in `vm` and
/// collection `epoch` was proven to be `accessor.bindTo(base)` (both `Some`),
/// or proven NOT to be one (both `None`).
#[derive(Clone, Copy)]
pub(crate) struct BoundFieldLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) accessor: Option<ObjectRef>,
    pub(crate) base: Option<ObjectRef>,
}

pub(crate) const BOUND_FIELD_MEMO_SLOTS: usize = 64;

thread_local! {
    /// Per-thread record of bound handles proven to be `bindTo` of a real
    /// instance field accessor (interpreter round i5 wave 101, lane L4;
    /// `CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default ON, `=0` off). Keyed as
    /// [`REAL_HANDLE_MEMO`] is: nothing moves or is freed within one collection
    /// epoch, so an equal address in an equal epoch under the same VM is the
    /// same handle, and its `argL0` / `argL1` / `form` / `type` are final.
    pub(crate) static BOUND_FIELD_MEMO: std::cell::RefCell<Vec<BoundFieldLine>> =
        std::cell::RefCell::new(
            (0..BOUND_FIELD_MEMO_SLOTS)
                .map(|_| BoundFieldLine {
                    vm: 0,
                    epoch: 0,
                    addr: 0,
                    accessor: None,
                    base: None,
                })
                .collect(),
        );
}

/// Ways per set of [`BOUND_FIELD_MEMO`] (interpreter round i5 wave 102, lane
/// L4). The memo was direct-mapped by `(addr >> 3) & 63`, so two bound
/// handles called alternately at one site whose addresses collided evicted
/// each other on every call, and each miss re-ran the proof (pins, a full
/// root refresh and the JDK's `bindTo`): the w101b A/B measured
/// `bound-alternate-warm-exact` 2.9x WORSE than the switch off with the JIT
/// (21 -> 63 us), while every other row gained. Now 16 sets of 4 ways with
/// move-to-front LRU, and the set chosen by a multiplicative hash of the
/// address, so up to four handles sharing a set keep their records.
pub(crate) const BOUND_FIELD_MEMO_WAYS: usize = 4;

/// The first line of `addr`'s set.
pub(crate) fn bound_field_memo_set(addr: usize) -> usize {
    let sets = BOUND_FIELD_MEMO_SLOTS / BOUND_FIELD_MEMO_WAYS;
    let mixed = ((addr as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((mixed >> 32) as usize & (sets - 1)) * BOUND_FIELD_MEMO_WAYS
}

/// The memo line of the handle at `addr` in `vm` and collection `epoch`
/// (moved to the front of its set), or `None`.
pub(crate) fn bound_field_memo_find(vm: usize, epoch: u64, addr: usize) -> Option<BoundFieldLine> {
    let set = bound_field_memo_set(addr);
    BOUND_FIELD_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let ways = &mut memo[set..set + BOUND_FIELD_MEMO_WAYS];
        let at = ways
            .iter()
            .position(|l| l.vm == vm && l.epoch == epoch && l.addr == addr)?;
        let line = ways[at];
        if at != 0 {
            ways[..=at].rotate_right(1);
        }
        Some(line)
    })
}

/// File `line` at the front of its set: replacing the same handle's line
/// where present, else evicting the least recently used way.
pub(crate) fn bound_field_memo_file(line: BoundFieldLine) {
    let set = bound_field_memo_set(line.addr);
    BOUND_FIELD_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let ways = &mut memo[set..set + BOUND_FIELD_MEMO_WAYS];
        let at = ways
            .iter()
            .position(|l| l.vm == line.vm && l.addr == line.addr)
            .unwrap_or(BOUND_FIELD_MEMO_WAYS - 1);
        ways[..=at].rotate_right(1);
        ways[0] = line;
    });
}

/// `CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER` -- performance, default ON
/// since its triage (`=0` off) (interpreter round i5 wave 101, lane L4; item 1 of
/// `i100-L4-proposal-method-handle-warm-call-directions`, page
/// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`).
///
/// `getter.bindTo(box)` of a real instance field accessor called by
/// `invokeExact` / `invoke`, served from the accessor's [`RealFieldTarget`]
/// record with the bound value as the base, ahead of [`mh_linked_invoker`]
/// (which runs `Invokers$Holder.invokeExact_MT`, the species' reinvoker form
/// and the accessor's form, interpreted, on every call: ~19 us,
/// `L4W95FieldHandleWarm` `bound-getter-warm-exact`).
///
/// Why not a structural recogniser (wave 100's follow-up): a
/// `Species_LL{argL0 = accessor, argL1 = value}` of the right `type` can also
/// be `insertArguments(refSetter, 1, value)` (the VALUE bound, not the base)
/// or a fold / collect. The bound position is encoded only in the `form`.
/// `bindTo` itself is JDK bytecode in both modes over a real JDK (the
/// `JdkInvoke` row retires the `bindTo` registration), so no native sees the
/// bind happen. The record is therefore made at the handle's FIRST call
/// instead: the JDK's own `accessor.bindTo(argL1)` is run once, and the handle
/// is recorded only when the answer has the handle's species class and the
/// very same `form`, `type`, `argL0` and `argL1` objects. A bound method
/// handle's behaviour is a function of exactly those (its form reads its
/// species fields), so the handle then behaves as `accessor.bindTo(base)`
/// does, whatever built it. `LambdaFormEditor` caches the bind transform per
/// reinvoker form, so a second `bindTo` answers the same `form`; when it does
/// not (the soft cache was cleared, the handle was customized by 127 linker
/// calls) the handle is never recorded and keeps the linker: a decline, never
/// a wrong answer.
///
/// Then each call is served exactly as [`mh_real_field_before_linker`] serves
/// the unbound accessor called with `(base, args...)`: the site's `MethodType`
/// (the site-type row) IS the bound handle's `type`, no site access refusal,
/// a non-null base, a setter value the form's cast admits, stack headroom
/// ([`real_field_call_admits`]). What the chain did that this skips is the
/// same: its hidden `LambdaForm` frames (no Java code runs below them) and
/// `checkCustomized`'s count. New: the record's one `bindTo` runs JDK
/// bytecode inside the door on the handle's first call (a fresh handle and a
/// few objects; no class is loaded or initialized, since the handle's own
/// species and form already exist; a JVMTI method-entry agent would see one
/// `bindTo` frame the linker would not have run).
///
/// GC: the first call pins the handle, its two fields and the call's
/// arguments around the Java call and reads them back; when the record is
/// not made and a collection moved anything, the call is handed to
/// [`mh_linked_invoker`] with the read-back values (the native's own `args`
/// would be stale). The compiled site of a `java/`-only type (`()I`) is bound
/// to the linker pair by `mh_compiled_linker_pair` and never reaches here, so
/// with the JIT this serves interpreted callers only.
pub(crate) fn mh_real_bound_field_before_linker(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    name: &str,
    armed_caller: u32,
) -> Option<MethodCallResult> {
    if !crate::nbflags().mh_real_bound_field_before_linker
        || !crate::nbflags().mh_real_field_before_linker
        || !mh_real_field_record_enabled()
        || crate::nbflags().dbg_mh_dispatch
    {
        return None;
    }
    let site = call_site?;
    let vm = ctx.vm_identity();
    if vm == 0 || extra.len() > 1 || ctx.object_num_fields(this) > MH_BOUND {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    if !ctx
        .class_name_arc_of_id(cid)
        .is_some_and(|n| &*n == BMH_SPECIES_LL)
    {
        return None;
    }
    let census = crate::mh_finder_census::phase_start();
    let epoch = ctx.gc_address_epoch();
    let addr = this.as_ptr() as usize;
    if let Some(line) = bound_field_memo_find(vm, epoch, addr) {
        // A handle the JDK's `bindTo` did not reproduce (the `insertArguments`
        // look-alike) is remembered too, so it pays the proof once per epoch.
        let (Some(accessor), Some(base)) = (line.accessor, line.base) else {
            return None;
        };
        let answer = real_bound_field_serve(ctx, this, accessor, base, extra, site, armed_caller);
        if let (Some(start), Some(_)) = (census, &answer) {
            crate::mh_finder_census::note_timed("lane", "bound-field-before-linker", start);
        }
        return answer;
    }
    // Cheap declines before any Java: the fields, the accessor's record, the
    // site's shape. Pure reads.
    let Value::Object(Some(accessor)) = ctx.get_field_by_name(this, "argL0") else {
        return None;
    };
    let Value::Object(Some(base)) = ctx.get_field_by_name(this, "argL1") else {
        return None;
    };
    {
        let record = real_handle_record(ctx, accessor)?;
        let field = record.field.as_ref()?;
        if matches!(field.ref_kind, 2 | 4) || !bound_site_matches(&record.declared, site) {
            return None;
        }
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(this, "form") else {
        return None;
    };
    let Value::Object(Some(ty)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    // The record: the JDK's own `accessor.bindTo(base)`, once.
    let this_pin = ctx.pin_native_root(this);
    let accessor_pin = ctx.pin_native_root(accessor);
    let base_pin = ctx.pin_native_root(base);
    let form_pin = ctx.pin_native_root(form);
    let ty_pin = ctx.pin_native_root(ty);
    let (_, handles) = pin_mh_args(ctx, extra);
    ctx.refresh_root_snapshot();
    let answer = ctx.invoke_virtual_bytecode_only(
        accessor,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(base))],
    );
    let this_now = ctx.read_native_pin(this_pin, this);
    let accessor_now = ctx.read_native_pin(accessor_pin, accessor);
    let base_now = ctx.read_native_pin(base_pin, base);
    let form_now = ctx.read_native_pin(form_pin, form);
    let ty_now = ctx.read_native_pin(ty_pin, ty);
    let extra_now = read_pinned_mh_args(&*ctx, &handles, extra);
    let proven = match answer {
        Ok(Some(Value::Object(Some(bound)))) => {
            ctx.class_id_of_object(bound) == ctx.class_id_of_object(this_now)
                && matches!(ctx.get_field_by_name(bound, "form"), Value::Object(Some(f)) if f == form_now)
                && matches!(ctx.get_field_by_name(bound, "type"), Value::Object(Some(t)) if t == ty_now)
                && matches!(ctx.get_field_by_name(bound, "argL0"), Value::Object(Some(a)) if a == accessor_now)
                && matches!(ctx.get_field_by_name(bound, "argL1"), Value::Object(Some(b)) if b == base_now)
        }
        // A refusal of the JDK's `bindTo` (its cast) is no record; its
        // exception is dropped here, the linker answers the call.
        _ => false,
    };
    ctx.unpin_native_roots(this_pin);
    let epoch_now = ctx.gc_address_epoch();
    let moved = epoch_now != epoch;
    // The verdict, either way (a refusal line has no accessor and no base).
    let addr_now = this_now.as_ptr() as usize;
    bound_field_memo_file(BoundFieldLine {
        vm,
        epoch: epoch_now,
        addr: addr_now,
        accessor: proven.then_some(accessor_now),
        base: proven.then_some(base_now),
    });
    if proven {
        if let Some(answer) = real_bound_field_serve(
            ctx,
            this_now,
            accessor_now,
            base_now,
            &extra_now,
            site,
            armed_caller,
        ) {
            return Some(answer);
        }
    }
    if moved {
        // The native's own arguments are stale now: the linker takes the
        // read-back ones.
        return mh_linked_invoker(ctx, this_now, &extra_now, Some(site), name);
    }
    None
}

/// Serve the call of the bound handle `this`, recorded as
/// `accessor.bindTo(base)`, at `site` with `extra` (nothing for a getter, the
/// value for a setter), or `None` (a guard declined; nothing disturbed).
pub(crate) fn real_bound_field_serve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    accessor: ObjectRef,
    base: ObjectRef,
    extra: &[Value],
    site: &str,
    armed_caller: u32,
) -> Option<MethodCallResult> {
    let record = real_handle_record(ctx, accessor)?;
    let field = record.field.as_ref()?;
    if matches!(field.ref_kind, 2 | 4) || !bound_site_matches(&record.declared, site) {
        return None;
    }
    let mut buf = [Value::Object(Some(base)), Value::Object(None)];
    let len = match extra {
        [] => 1,
        [v] => {
            buf[1] = *v;
            2
        }
        _ => return None,
    };
    let full = &buf[..len];
    if !real_field_call_admits(&*ctx, accessor, field, full) {
        return None;
    }
    // The site checks, against the BOUND handle's own `type` (keyed by it in
    // the site memo, as the unbound lane keys its accessor).
    if !real_lane_site_admits(&*ctx, this, site, armed_caller) {
        return None;
    }
    // JIT round 22 wave 3, lane runtime: the BOUND handle is what the linker's
    // `checkCustomized` counts ([`lane_check_customized`]).
    lane_check_customized(&*ctx, this)?;
    // Wave 108 (`CRATONVM_MH_REAL_FIELD_RAW_RETURN`, default off).
    let raw = real_field_raw_return(field, site);
    let answer = mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        full,
        raw,
    );
    Some(if raw {
        real_field_raw_answer(ctx, answer, &field.fdesc)
    } else {
        answer
    })
}

/// `CRATONVM_JIT_MH_REAL_FIELD_LANE` (performance, default ON, `=0` off;
/// 2026-10-18, page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the WARM half of [`mh_real_field_before_linker`] and
/// [`mh_real_bound_field_before_linker`], for a compiled `invokeExact` /
/// `invoke` site that the JIT's dispatch helper answers without its native
/// funnel (`vm/src/jit/helpers.rs`, `try_jit_mh_real_field_lane`).
///
/// It serves exactly the calls those two lanes serve, by the same guards and
/// the same access, but only when every answer it needs is already on file, so
/// nothing here runs Java, walks a frame, allocates or passes a GC point:
/// * the handle's [`RealHandleRecord`] (pure reads on a miss);
/// * for a bound handle, its [`BOUND_FIELD_MEMO`] proof (the proof itself runs
///   the JDK's `bindTo`, so a handle with no line declines and its first call
///   files one through the native);
/// * the site checks from the [`REAL_FIELD_SITE_MEMO`] line filed under the
///   armed caller (`armed_caller`, the compiled site's declaring class, as the
///   carrier door arms it: [`real_lane_site_admits`]'s hint hit). A line is
///   filed only under the class a frame walk answered, so a hit means the
///   checks passed for that class with this handle and site;
/// * a getter's answer that needs no box: a primitive field read at a site
///   returning exactly that primitive, or a reference read. A setter returns
///   nothing.
///
/// `None` declines with nothing disturbed: the helper then takes the native
/// route, which serves the call as before (and files what was missing).
pub fn jit_real_field_lane_warm(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    site: &str,
    armed_caller: u32,
) -> Option<MethodCallResult> {
    let flags = crate::nbflags();
    if armed_caller == 0
        || !flags.mh_real_field_before_linker
        || !flags.mh_real_field_site_memo
        || !flags.mh_armed_caller
        || !mh_real_field_record_enabled()
        || flags.dbg_mh_dispatch
        || ctx.object_num_fields(this) > MH_BOUND
    {
        return None;
    }
    // `CRATONVM_DBG=mh-finder-census`: `lane jit-field-warm` (served calls).
    let census = crate::mh_finder_census::phase_start();
    let served = |answer: MethodCallResult| {
        if let Some(start) = census {
            crate::mh_finder_census::note_timed("lane", "jit-field-warm", start);
        }
        Some(answer)
    };
    // An unbound accessor first, as the natives ask the lanes.
    let record = real_handle_record(ctx, this);
    if let Some(field) = record.as_ref().and_then(|r| r.field.as_ref()) {
        let declared = record.as_ref().map(|r| &*r.declared);
        let raw = real_field_raw_return(field, site);
        if Some(site) != declared
            || !real_field_warm_answer_needs_no_box(field, raw)
            || !real_field_call_admits(&*ctx, this, field, extra)
            || !real_lane_site_hint_admits(&*ctx, this, site, armed_caller)
        {
            return None;
        }
        // JIT round 22 wave 3, lane runtime: the native lanes' count
        // ([`lane_check_customized`]); the due call takes the native route.
        lane_check_customized(&*ctx, this)?;
        return served(mh_real_field_access_resolved(
            ctx,
            field.ref_kind,
            field.offset,
            &field.fdesc,
            field.target,
            field.volatile,
            extra,
            raw,
        ));
    }
    // A proven `bindTo` of an instance accessor.
    if !flags.mh_real_bound_field_before_linker || extra.len() > 1 {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let line = bound_field_memo_find(vm, ctx.gc_address_epoch(), this.as_ptr() as usize)?;
    let (Some(accessor), Some(base)) = (line.accessor, line.base) else {
        return None;
    };
    let record = real_handle_record(ctx, accessor)?;
    let field = record.field.as_ref()?;
    let raw = real_field_raw_return(field, site);
    if matches!(field.ref_kind, 2 | 4)
        || !bound_site_matches(&record.declared, site)
        || !real_field_warm_answer_needs_no_box(field, raw)
    {
        return None;
    }
    let mut buf = [Value::Object(Some(base)), Value::Object(None)];
    let len = match extra {
        [] => 1,
        [v] => {
            buf[1] = *v;
            2
        }
        _ => return None,
    };
    let full = &buf[..len];
    if !real_field_call_admits(&*ctx, accessor, field, full)
        || !real_lane_site_hint_admits(&*ctx, this, site, armed_caller)
    {
        return None;
    }
    lane_check_customized(&*ctx, this)?;
    served(mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        full,
        raw,
    ))
}

/// Can [`jit_real_field_lane_warm`] never serve `this`, whatever it is called
/// with later? True for a handle that is not a real field accessor (no field
/// record) and not a `BoundMethodHandle$Species_LL`, the only class a bound
/// record is ever proven for: neither answer changes for the handle's life.
/// A `Species_LL` without a proof is NOT such a handle (its first native call
/// files the proof). Pure reads.
pub fn jit_real_field_lane_never_serves(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    if real_handle_record(ctx, this).is_some_and(|r| r.field.is_some()) {
        return false;
    }
    let cid = ctx.class_id_of_object(this);
    !ctx.class_name_arc_of_id(cid)
        .is_some_and(|n| &*n == BMH_SPECIES_LL)
}

/// Would [`jit_real_field_lane_warm`]'s answer for `field` be handed back
/// without a box? A setter (nothing returned), a reference getter, or a
/// primitive getter whose raw return applies (`raw`, [`real_field_raw_return`]
/// at the call's site).
pub(crate) fn real_field_warm_answer_needs_no_box(field: &RealFieldTarget, raw: bool) -> bool {
    match field.ref_kind {
        3 | 4 => true,
        _ => field.fdesc.len() != 1 || raw,
    }
}

/// [`real_lane_site_admits`] answered from the site memo under the armed
/// caller only: no frame walk, no site-type row, no refusal classification.
/// `false` when that line is not on file (the native route then runs the full
/// checks and files it).
pub(crate) fn real_lane_site_hint_admits(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    site: &str,
    armed_caller: u32,
) -> bool {
    real_field_site_memo_key(ctx, this, ClassId::new(armed_caller))
        .is_some_and(|key| real_field_site_memo_hit(key, site))
}

/// The two site checks of the before-linker lanes (field, bound field,
/// direct) for the handle `this` called at `site`: the site's `MethodType` as
/// the CALLER resolves it (the site-type row) IS `this.type`, and the site has
/// no access refusal. `true` admits; `false` declines (the linker runs).
///
/// The caller is the innermost frame's class (`frame_class_ids_tail(1)`, as
/// before wave 107). With the wave-100 site memo
/// (`CRATONVM_MH_REAL_FIELD_SITE_MEMO`) a (handle, caller, site) that passed
/// in this collection epoch passes again without the two checks.
///
/// Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`,
/// default OFF; page `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`):
/// `armed_caller` is the class id the arming door published with the site
/// (`poly_call_site::taken_caller`, `0` = none; the JIT's carrier door passes
/// its `JitInvokeInfo::declaring_class_id`). With the switch on, the memo is
/// probed FIRST under that class, and a hit admits without walking the stack
/// for the caller (under the JIT the walk is a compiled-frame walk: code-range
/// lookups and the inline chain of the innermost activation, the largest
/// per-call item left in the lane by reading). A line is only ever filed under
/// the class the WALK answered, so a hint hit means the checks passed for that
/// very class with this handle and site text; the hint is trusted only to
/// name the caller of this call, which is the class the JIT resolves the
/// site's constant-pool names through. A miss walks as before and, with the
/// census on (`CRATONVM_DBG=mh-finder-census`), counts `armed-caller.agree` /
/// `armed-caller.mismatch` against the walked caller, so a wrong hint (an
/// inlined site naming its outer method's class) shows there.
pub(crate) fn real_lane_site_admits(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    site: &str,
    armed_caller: u32,
) -> bool {
    let memo_on = crate::nbflags().mh_real_field_site_memo;
    let hinted = armed_caller != 0 && crate::nbflags().mh_armed_caller;
    if memo_on
        && hinted
        && real_field_site_memo_key(ctx, this, ClassId::new(armed_caller))
            .is_some_and(|key| real_field_site_memo_hit(key, site))
    {
        crate::mh_finder_census::event("armed-caller.hit");
        return true;
    }
    let Some(caller) = ctx.frame_class_ids_tail(1).0.first().copied() else {
        return false;
    };
    if hinted {
        crate::mh_finder_census::event(if caller.as_u32() == armed_caller {
            "armed-caller.agree"
        } else {
            "armed-caller.mismatch"
        });
    }
    let memo_key = if memo_on {
        real_field_site_memo_key(ctx, this, caller)
    } else {
        None
    };
    if memo_key.is_some_and(|key| real_field_site_memo_hit(key, site)) {
        return true;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return false;
    };
    if mh_site_type_row_for(ctx, caller, site) != Some(mt) {
        return false;
    }
    if mh_site_access_refusal(ctx, Some(site)).is_some() {
        return false;
    }
    if let Some(key) = memo_key {
        real_field_site_memo_fill(key, site);
    }
    true
}

/// The key of one [`REAL_FIELD_SITE_MEMO`] line: the VM, the collection epoch,
/// the handle's address and the caller's class id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RealFieldSiteKey {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) caller: u32,
}

pub(crate) struct RealFieldSiteLine {
    pub(crate) key: Option<RealFieldSiteKey>,
    pub(crate) site: Box<str>,
}

pub(crate) const REAL_FIELD_SITE_MEMO_SLOTS: usize = 64;

thread_local! {
    /// Per-thread memo of the (handle, caller, call site) triples whose site
    /// checks in [`mh_real_field_before_linker`] passed (interpreter round i5
    /// wave 100, lane L4; `CRATONVM_MH_REAL_FIELD_SITE_MEMO`, default off).
    ///
    /// Soundness, check by check:
    /// * the handle: [`REAL_HANDLE_MEMO`]'s argument -- nothing moves or is
    ///   freed within one collection epoch, so an equal address in an equal
    ///   epoch under the same VM is the same handle, and its `type` is final;
    /// * "the site-type row IS the handle's `type`": the caller's site type is
    ///   `MethodType.fromMethodDescriptorString(site, caller's loader)`, an
    ///   interned object the handle's own `type` field keeps alive, so once the
    ///   two were the same object they stay the same object (a row evicted from
    ///   the bounded table would be re-filed with that same object). Class ids
    ///   are never reused within a VM;
    /// * no site access refusal: HotSpot links an `invokehandle` site once and
    ///   keeps it; the caller's classes and the classes its loader resolved the
    ///   site's names to are fixed, so the accessibility answer is too.
    ///
    /// Only passes are remembered; a decline is asked again on every call.
    /// Direct-mapped by the handle's address; a miss costs what the lane cost
    /// before (plus one line write after a pass).
    pub(crate) static REAL_FIELD_SITE_MEMO: std::cell::RefCell<Vec<RealFieldSiteLine>> =
        std::cell::RefCell::new(
            (0..REAL_FIELD_SITE_MEMO_SLOTS)
                .map(|_| RealFieldSiteLine { key: None, site: Box::from("") })
                .collect(),
        );
}

/// The memo key of `this` called from `caller`; `None` for a context that
/// reports VM identity `0` (the mocks), which is never memoised.
pub(crate) fn real_field_site_memo_key(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    caller: ClassId,
) -> Option<RealFieldSiteKey> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    Some(RealFieldSiteKey {
        vm,
        epoch: ctx.gc_address_epoch(),
        addr: this.as_ptr() as usize,
        caller: caller.as_u32(),
    })
}

pub(crate) fn real_field_site_memo_slot(key: RealFieldSiteKey) -> usize {
    let mixed = (key.addr >> 3) ^ (key.caller as usize).wrapping_mul(0x9E37);
    mixed & (REAL_FIELD_SITE_MEMO_SLOTS - 1)
}

/// Did `key` pass its site checks at the call site `site` in this epoch?
pub(crate) fn real_field_site_memo_hit(key: RealFieldSiteKey, site: &str) -> bool {
    let slot = real_field_site_memo_slot(key);
    REAL_FIELD_SITE_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        line.key == Some(key) && &*line.site == site
    })
}

/// Remember that `key` passed its site checks at `site`.
pub(crate) fn real_field_site_memo_fill(key: RealFieldSiteKey, site: &str) {
    let slot = real_field_site_memo_slot(key);
    REAL_FIELD_SITE_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let line = &mut memo[slot];
        if &*line.site != site {
            line.site = Box::from(site);
        }
        line.key = Some(key);
    });
}

/// Execute the JDK's signature-polymorphic linker with its MethodType appendix.
/// The linker owns exact/inexact checks and customization, then invokes the
/// handle's real LambdaForm via invokeBasic. Carrier handles keep their door.
///
/// Interpreter round i4 wave 95, lane L4: `CRATONVM_DBG=mh-finder-census`
/// times each call by outcome (`linker <name> pair-hit` / `basic-hit` /
/// `miss-shareable` / `miss-user-site`) and its sub-phases (`site-type`,
/// `linker-method`, `link-to`). A site naming a class outside `java/` never
/// had a row (an appendix naming an application class must not be rooted), so
/// every call of it ran the miss: the site's type, the Java linker method and
/// three full root refreshes. Under `CRATONVM_MH_LINKER_BASIC_MEMBER` (default
/// off) such a site takes the linker's `MemberName` from a row keyed by the
/// caller, the name and the site's basic type ([`linker_basic_descriptor`];
/// the member is a JDK form's entry, which names no application class) and
/// its appendix from the site-type table ([`mh_site_type_row_for`]: the very
/// object the factory answers), and runs as a pair hit does.
pub(crate) fn mh_linked_invoker(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    site: Option<&str>,
    name: &str,
) -> Option<MethodCallResult> {
    let site = site?;
    if ctx.object_num_fields(this) > MH_BOUND
        || !ctx.method_exists(
            "java/lang/invoke/MethodType",
            "fromMethodDescriptorString",
            MH_SITE_TYPE_FACTORY_DESC,
        )
        || !matches!(ctx.get_field_by_name(this, "form"), Value::Object(Some(_)))
    {
        return None;
    }
    if crate::lang_reflect::reflective_native_stack_exhausted() {
        return Some(Err(RuntimeError::StackOverflowError.into()));
    }
    if let Some(refusal) = mh_site_access_refusal(&*ctx, Some(site)) {
        return Some(Err(refusal));
    }
    if let Some(npe) = mh_interface_null_receiver_npe(ctx, this, extra, Some(site)) {
        return Some(Err(npe));
    }
    // JIT round 22 wave 4 fix: the linker runs this call; a due handle is filed
    // so the lanes serve it if the linker's `checkCustomized` cannot customize it.
    // `invokeExact` only: its `invokeExact_MT` counts THIS handle, where the
    // generic invoker of `invoke` counts its `asType` answer, which at another
    // type is an adapter, leaving this handle due.
    if name == "invokeExact" {
        lane_note_due_linker_ran(&*ctx, this);
    }
    // The native reference-cast bridge still needs the defining-loader checks
    // even when the JDK linker supplies the visible Java activations.
    let ret_cast = if name == "invoke" {
        if let Some(refusal) = mh_invoke_loader_cast_refusal(ctx, this, extra, Some(site)) {
            return Some(Err(refusal));
        }
        mh_invoke_return_cast_target(ctx, this, Some(site))
    } else {
        None
    };
    let near = ctx.frame_class_ids_tail(1).0.first().copied();
    // The caller's loader determines every descriptor component. This distinct
    // namespace in the VM's rooted type table retains the linker's appendix
    // and MemberName, and is forgotten when its caller class unloads.
    // A strongly rooted appendix must not retain an application's loader.
    // Reuse the bound-type table's reviewed bootstrap/platform predicate;
    // sites naming user classes resolve afresh and never enter this cache.
    let key = if bound_method_type_shareable(&*ctx, site) {
        near.map(|id| {
            cratonvm_native_api::method_type_table::class_keyed(
                &format!("linker:{name}:{site}"),
                &[id.as_u32()],
            )
        })
    } else {
        None
    };
    // Interpreter round i4 wave 95, lane L4 (`CRATONVM_MH_LINKER_BASIC_MEMBER`):
    // the basic-type member row of a site no pair row may hold. Its own key
    // prefix: the row is the linker's `MemberName` itself, not an `Object[2]`
    // pair as a `linker:` row is.
    let basic_key = match (&key, near) {
        (None, Some(id)) if crate::nbflags().mh_linker_basic_member => {
            linker_basic_descriptor(site).map(|basic| {
                cratonvm_native_api::method_type_table::class_keyed(
                    &format!("linker-basic:{name}:{basic}"),
                    &[id.as_u32()],
                )
            })
        }
        _ => None,
    };
    let census = crate::mh_finder_census::phase_start();
    let mut outcome: &'static str = if key.is_some() {
        "miss-shareable"
    } else {
        "miss-user-site"
    };
    let budget = ctx.rebase_compiled_stack_budget(crate::lang_reflect::reflective_stack_bounds());
    let result = Some((|| {
        let (base, handles) = pin_mh_args(ctx, extra);
        let this_pin = ctx.pin_native_root(this);
        let answer = (|| {
            let cached = key
                .as_deref()
                .and_then(|key| ctx.method_handle_linker_get(key));
            // Wave 95: the basic-type row's member and the site-type table's
            // `MethodType` (pure reads; nothing below allocates or runs Java
            // before both are in `values`).
            let basic_hit = match (&cached, basic_key.as_deref(), near) {
                (None, Some(basic_key), Some(caller)) => mh_site_type_row_for(&*ctx, caller, site)
                    .and_then(|mt| {
                        let member = ctx.method_handle_linker_get(basic_key)?;
                        Some((mt, member))
                    }),
                _ => None,
            };
            if let Some((mt, member)) = basic_hit {
                outcome = "basic-hit";
                let mut values = vec![Value::Object(Some(ctx.read_native_pin(this_pin, this)))];
                values.extend(read_pinned_mh_args(ctx, &handles, extra));
                values.push(Value::Object(Some(mt)));
                values.push(Value::Object(Some(member)));
                // As a pair hit: no pin is read back across a Java call.
                if !crate::nbflags().mh_linker_hit_skips_root_refresh {
                    ctx.refresh_root_snapshot();
                }
                let link_clock = crate::mh_finder_census::phase_start();
                let answer = crate::native_method_handle_link_to(ctx, &values);
                linker_census(name, "link-to", link_clock);
                return answer;
            }
            let linker_hit = cached.is_some();
            if linker_hit {
                outcome = "pair-hit";
            }
            let pair = if let Some(pair) = cached {
                pair
            } else {
                // Resolve with the caller's JDK factory, including exact boot
                // callers and NCDFError link failures. The native descriptor
                // builder can mint stand-ins or fall through to another loader.
                let site_clock = crate::mh_finder_census::phase_start();
                ctx.refresh_root_snapshot();
                let mt = {
                    let mut scope = NativeHandleScope::new(ctx);
                    match mh_site_method_type(&mut scope, site)? {
                        Some(Value::Object(Some(mt))) => mt,
                        _ => {
                            return Err(RuntimeError::InternalError {
                                message: "cannot build invoker MethodType".into(),
                            }
                            .into())
                        }
                    }
                };
                linker_census(name, "site-type", site_clock);
                let linker_clock = crate::mh_finder_census::phase_start();
                let mt_pin = ctx.pin_native_root(mt);
                let appendix = ctx.new_array(ArrayElementType::Reference, 1);
                let appendix_pin = ctx.pin_native_root(appendix);
                let name_obj = ctx.create_string_uninterned(name);
                // A peer collection during reentrant compiled Java must see the
                // native pins in the deposited snapshot, not only on the Rust heap.
                ctx.refresh_root_snapshot();
                let member = ctx.invoke(
                    "java/lang/invoke/Invokers", "methodHandleInvokeLinkerMethod",
                    "(Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/MemberName;",
                    &[Value::Object(Some(name_obj)), Value::Object(Some(ctx.read_native_pin(mt_pin, mt))),
                      Value::Object(Some(ctx.read_native_pin(appendix_pin, appendix)))],
                )?;
                let member = match member {
                    Some(Value::Object(Some(m))) => m,
                    _ => {
                        return Err(RuntimeError::InternalError {
                            message: "missing invoker linker".into(),
                        }
                        .into())
                    }
                };
                linker_census(name, "linker-method", linker_clock);
                let member_pin = ctx.pin_native_root(member);
                // Wave 95: file the member alone under the site's basic type
                // (never the appendix: it is the site's own type, which names
                // an application class and is never rooted here). A linker that
                // answered no appendix was customized for a wide site, which
                // `linker_basic_descriptor` gives no key anyway. No allocation.
                if let Some(basic_key) = basic_key.as_deref() {
                    let appendix_now =
                        ctx.get_array_element(ctx.read_native_pin(appendix_pin, appendix), 0);
                    if matches!(appendix_now, Value::Object(Some(_))) {
                        let member_now = ctx.read_native_pin(member_pin, member);
                        let _ = ctx.method_handle_linker_put(basic_key, member_now);
                    }
                }
                let pair = ctx.new_array(ArrayElementType::Reference, 2);
                ctx.set_array_element(
                    pair,
                    0,
                    Value::Object(Some(ctx.read_native_pin(member_pin, member))),
                );
                ctx.set_array_element(
                    pair,
                    1,
                    ctx.get_array_element(ctx.read_native_pin(appendix_pin, appendix), 0),
                );
                key.as_deref()
                    .map_or(pair, |key| ctx.method_handle_linker_put(key, pair))
            };
            let pair_pin = ctx.pin_native_root(pair);
            let mut values = vec![Value::Object(Some(ctx.read_native_pin(this_pin, this)))];
            values.extend(read_pinned_mh_args(ctx, &handles, extra));
            let pair = ctx.read_native_pin(pair_pin, pair);
            values.push(ctx.get_array_element(pair, 1));
            values.push(ctx.get_array_element(pair, 0));
            // JIT round 20 wave 17, lane pRefl
            // (`CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH`, default ON, `=0`
            // keeps the refresh on every call; `R20PAllocEntryCheck`): this
            // refresh is a FULL deposit (every interpreter frame, a fresh
            // conservative scan of the compiled band, the off-frame roots), and
            // every `Method.invoke` paid it once through
            // `DirectMethodHandleAccessor.invokeImpl`'s `invokeExact`. After
            // this point the door reads none of its pins again: `values` is
            // handed to the linker as is, the two unpins only truncate, and the
            // `invoke` return cast holds a `ClassId`. So on a linker-table hit,
            // where no pin was read back across a Java call, nothing of this
            // door's needs publishing for the call. The miss path, which runs
            // `methodHandleInvokeLinkerMethod` with live pins, keeps both of its
            // refreshes and this one.
            if !(linker_hit && crate::nbflags().mh_linker_hit_skips_root_refresh) {
                ctx.refresh_root_snapshot();
            }
            let link_clock = crate::mh_finder_census::phase_start();
            let answer = crate::native_method_handle_link_to(ctx, &values);
            linker_census(name, "link-to", link_clock);
            answer
        })();
        ctx.unpin_native_roots(this_pin);
        if base != usize::MAX {
            ctx.unpin_native_roots(base);
        }
        match ret_cast {
            Some(target) => mh_invoke_return_cast(&*ctx, target, answer),
            None => answer,
        }
    })());
    ctx.restore_compiled_stack_budget(budget);
    linker_census(name, outcome, census);
    result
}

/// The `CRATONVM_DBG=mh-finder-census` clock of one `invoke` / `invokeExact`
/// native call on `this` (`None`, one flag read, unless the census is on).
/// Reads only. Interpreter round i4 wave 89, lane L4.
pub(crate) fn mh_invoke_census_clock(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    exact: bool,
) -> Option<crate::mh_finder_census::InvokeClock> {
    if !crate::mh_finder_census::enabled() {
        return None;
    }
    let class_name = ctx.class_name_arc_of_id(ctx.class_id_of_object(this));
    crate::mh_finder_census::invoke_clock(
        ctx.vm_identity(),
        ctx.gc_collection_count(),
        this.as_ptr() as usize,
        exact,
        class_name.as_deref(),
    )
}

/// Interpreter round i5 wave 104, lane L3 (hand-back of
/// `L3W104WalkOptionsThroughHandles` row `invokeWithArguments`, both modes):
/// the `invokeWithArguments` `Intrinsic`s run the target themselves and push
/// no frame, so every walk and throwable inside the target lacked HotSpot's
/// `java.lang.invoke.MethodHandle.invokeWithArguments` frame. Record the call
/// (`ReflectiveCallKind::MethodHandleInvokeWithArguments[List]`), as the
/// served `StackWalker` natives do, so a capture lists that JDK frame right
/// below the first frame the call pushed (`stackwalker::reflective_entries`);
/// a throwable the door raises itself gets nothing on top (the served-raise
/// chain names that frame). Not for a variable-arity collector, whose JDK
/// override's frames differ (fail-closed: as before). Entering runs no Java
/// and allocates nothing for these kinds, so `args` stay valid.
pub(crate) fn with_invoke_with_arguments_frames(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: cratonvm_native_api::ReflectiveCallKind,
    door: impl FnOnce(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> MethodCallResult {
    let listed = matches!(
        args.first(),
        Some(Value::Object(Some(this))) if !mh_is_real_varargs_collector(&*ctx, *this)
    );
    let mark = listed.then(|| ctx.enter_reflective_call(kind));
    let result = door(ctx, args);
    if let Some(mark) = mark {
        ctx.leave_reflective_call(mark);
    }
    result
}

/// `CRATONVM_MH_IWA_JDK_BODY` -- HotSpot frames vs performance, default OFF,
/// `truthy_word`, read per call (a `NativeFlags` field load). Interpreter round
/// i5 wave 109, lane L4 (page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, row
/// `invokeWithArguments hidden` of `L3W104WalkOptionsThroughHandles`): on a
/// REAL handle that is not a variable-arity collector (and not a VM-minted
/// downcall carrier, `mh_with_arguments_runs_real`), the two
/// `invokeWithArguments` `Intrinsic`s run the JDK's own body
/// (`invoke_virtual_bytecode_only`, so the `Intrinsic` is not re-entered for
/// this call) instead of the served door. The body is HotSpot's:
/// `genericMethodType(n)`, `asType`, then
/// `invokers().spreadInvoker(0).invokeExact(adapted, arguments)` -- the
/// `Invokers$Holder.invokeExact_MT` linker, the spread invoker's and the
/// `asType` adapter's spun `LambdaForm$MH.invoke` frames and the target's
/// `DirectMethodHandle$Holder` frame are then real activations, which a
/// `SHOW_HIDDEN_FRAMES` walk and a `-XX:+ShowHiddenFrames` throwable list as
/// HotSpot does (the served door runs the target with none of them and the
/// stand-in rows cannot name a spun class this VM never defined). The
/// `invokeWithArguments` frame itself is the body's own interpreter frame, so
/// no reflective mark is entered. `List` overload: its body calls the array
/// overload virtually, which comes back here. Costs the spun forms per call
/// (SBR4 measured 106-149 us against 25 us on the old carrier door; the bench
/// `tools/probes/interp/L4/L4W109InvokeWithArgumentsBodyBench.java` measures it
/// now). `None`: the served door as before.
pub(crate) fn mh_iwa_jdk_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    descriptor: &str,
) -> Option<MethodCallResult> {
    if !crate::nbflags().mh_iwa_jdk_body {
        return None;
    }
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    // A variable-arity collector reaches the array overload through its
    // override's `super.invokeWithArguments`: a virtual re-dispatch would land
    // in the override again (see `mh_real_invoke_with_arguments_door`).
    if !mh_with_arguments_runs_real(&*ctx, this) || mh_is_real_varargs_collector(&*ctx, this) {
        return None;
    }
    // Wave 110: the census scope `iwa-body` (`CRATONVM_DBG=mh-finder-census`).
    let _scope = crate::mh_finder_census::named_scope("iwa-body");
    // Interpreter round i6 wave 116, lane L4 (`CRATONVM_MH_IWA_NATIVE_STEPS`,
    // default off): the array body's steps without interpreting it.
    if crate::nbflags().mh_iwa_native_steps && descriptor == MH_IWA_ARRAY_DESC {
        if let Some(answer) = mh_iwa_native_steps(ctx, args) {
            return Some(answer);
        }
    }
    Some(ctx.invoke_virtual_bytecode_only(this, "invokeWithArguments", descriptor, &args[1..]))
}

/// `MethodHandle.invokeWithArguments(Object[])`'s descriptor.
pub(crate) const MH_IWA_ARRAY_DESC: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";

/// The call-site type of the array body's `spreadInvoker(0).invokeExact(adapted, arguments)`.
pub(crate) const MH_IWA_SPREAD_SITE: &str = "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;";

/// Interpreter round i6 wave 116, lane L4 (`CRATONVM_MH_IWA_NATIVE_STEPS`, default off; read only
/// under `CRATONVM_MH_IWA_JDK_BODY`; page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, re-entries 1 and 2): the JDK's
/// array body, `genericMethodType(n).invokers().spreadInvoker(0).invokeExact(asType(generic),
/// arguments)`, run step by step from here instead of re-entering its bytecode, which the
/// method's own `Intrinsic` keeps interpreted for good (`native_skip`):
///
/// * `asType(generic)`: [`mh_real_iwa_adapted`] (the JDK's `genericMethodType` and `asType`,
///   memoised per collection epoch; the `asTypeCache` read under
///   `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ`). A throw declines, so the JDK body raises it with
///   its own `invokeWithArguments` frame, as the served door does;
/// * the spread invoker: the JDK's own `invokers().spreadInvoker(0)`, or its filed answer
///   under `CRATONVM_MH_IWA_SPREADER_MEMO` ([`mh_iwa_spreader`]);
/// * `invokeExact`: the JDK linker itself ([`mh_linked_invoker`] at the body's call-site type),
///   so `Invokers$Holder.invokeExact_MT`, the spread invoker's and the adapter's spun
///   `LambdaForm$MH` frames are real activations, as from the body. The
///   `MethodHandle.invokeWithArguments` frame itself is the reflective mark the served door
///   enters ([`with_invoke_with_arguments_frames`]), listed below the first frame the call pushes.
///
/// `None` (the JDK body runs) on a decline before anything Java-visible happened.
pub(crate) fn mh_iwa_native_steps(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    if ctx.vm_identity() == 0 || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    let array = match args.get(1) {
        Some(Value::Object(Some(a))) => Some(*a),
        Some(Value::Object(None)) | None => None,
        Some(_) => return None,
    };
    let n = array.map_or(0, |a| ctx.array_length(a));
    let this_pin = ctx.pin_native_root(this);
    let array_pin = array.map(|a| ctx.pin_native_root(a));
    let adapted = match mh_real_iwa_adapted(ctx, this_pin, this, n) {
        Ok(Some(h)) => h,
        // A refusal, or the JDK's `asType` threw: its own body raises it again
        // with its frame (nothing Java-visible happened here).
        Ok(None) | Err(_) => {
            ctx.unpin_native_roots(this_pin);
            return None;
        }
    };
    let adapted_pin = ctx.pin_native_root(adapted);
    let spreader = match mh_iwa_spreader(ctx, n) {
        Ok(Some(s)) => s,
        Ok(None) => {
            ctx.unpin_native_roots(this_pin);
            return None;
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Some(Err(e));
        }
    };
    // Nothing below allocates or runs Java before the linker pins its own
    // arguments: read every reference now.
    let adapted = ctx.read_native_pin(adapted_pin, adapted);
    let arr = match (array, array_pin) {
        (Some(a), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, a))),
        _ => Value::Object(None),
    };
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let extra = [Value::Object(Some(adapted)), arr];
    // The door's own argument view, current (the calls above may have moved
    // `this` and the array): the mark reads its receiver.
    let door_args = [Value::Object(Some(this_now)), arr];
    Some(with_invoke_with_arguments_frames(
        ctx,
        &door_args,
        cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments,
        move |ctx, _| {
            // Wave 130 (lane L4, `CRATONVM_MH_IWA_LINK_MEMO`): the door's pair
            // hit from this thread's line ([`iwa_link_memo_call`]).
            let link_memo = iwa_link_memo_enabled(&*ctx);
            if link_memo {
                if let Some(answer) = iwa_link_memo_call(ctx, spreader, &extra) {
                    return answer;
                }
            }
            let answer = match mh_linked_invoker(
                ctx,
                spreader,
                &extra,
                Some(MH_IWA_SPREAD_SITE),
                "invokeExact",
            ) {
                Some(answer) => answer,
                None => mh_invoke_exact_door(ctx, spreader, &extra, Some(MH_IWA_SPREAD_SITE), true),
            };
            if link_memo {
                iwa_link_line_fill(&*ctx);
            }
            answer
        },
    ))
}

/// `CRATONVM_MH_IWA_LINK_MEMO` -- performance, default OFF, `truthy_word`
/// (`NativeFlags::mh_iwa_link_memo`), read only on the native steps' path
/// (`CRATONVM_MH_IWA_JDK_BODY` + `CRATONVM_MH_IWA_NATIVE_STEPS`) and only with
/// `CRATONVM_MH_IWA_SPREADER_MEMO` (the spreader it remembers is that switch's
/// rooted row). Interpreter round i6 wave 130, lane L4 (page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 130").
/// Off under the census (`CRATONVM_DBG=mh-finder-census`), so the census
/// keeps measuring the door's own rows, and under `dbg_mh_dispatch`.
pub(crate) fn iwa_link_memo_enabled(ctx: &dyn NativeContext) -> bool {
    let flags = crate::nbflags();
    flags.mh_iwa_link_memo
        && flags.mh_iwa_spreader_memo
        && !flags.dbg_mh_dispatch
        && ctx.vm_identity() != 0
        && !crate::mh_finder_census::enabled()
}

/// Lines of [`IWA_LINK_MEMO`] (each half).
pub(crate) const IWA_LINK_MEMO_SLOTS: usize = 4;

/// One line of [`IWA_LINK_MEMO`]: `key` is `(vm identity, address epoch,
/// arity n | caller class id)`; `a` / `b` are the spreader (and nothing) or
/// the linker's `(MemberName, appendix)`.
#[derive(Clone, Copy)]
pub(crate) struct IwaLinkLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) key: u64,
    pub(crate) a: Option<ObjectRef>,
    pub(crate) b: Option<ObjectRef>,
}

impl IwaLinkLine {
    const EMPTY: IwaLinkLine = IwaLinkLine {
        vm: 0,
        epoch: 0,
        key: u64::MAX,
        a: None,
        b: None,
    };
}

thread_local! {
    /// Interpreter round i6 wave 130, lane L4 (`CRATONVM_MH_IWA_LINK_MEMO`,
    /// default off): what the `invokeWithArguments` native steps looked up per
    /// call. `.0`: the `n`-argument spreader of `CRATONVM_MH_IWA_SPREADER_MEMO`'s
    /// rooted row; `.1`: the `linker:invokeExact:(MethodHandle,Object[])Object`
    /// row's `(MemberName, appendix)` for one caller class. Both are rooted rows
    /// of the VM's table, so an address stays the object's for one address
    /// epoch (which moves on every collection, a freed block included); a line
    /// of another VM or epoch is a miss. Per VM by key, like
    /// `IWA_ADAPT_MEMO`; holds no state Java can see.
    pub(crate) static IWA_LINK_MEMO: std::cell::Cell<IwaLinkLines> = const {
        std::cell::Cell::new((
            [IwaLinkLine::EMPTY; IWA_LINK_MEMO_SLOTS],
            [IwaLinkLine::EMPTY; IWA_LINK_MEMO_SLOTS],
        ))
    };
}

/// [`IWA_LINK_MEMO`]'s two halves: spreaders by arity, linker rows by caller.
pub(crate) type IwaLinkLines = (
    [IwaLinkLine; IWA_LINK_MEMO_SLOTS],
    [IwaLinkLine; IWA_LINK_MEMO_SLOTS],
);

pub(crate) fn iwa_link_line_get(
    ctx: &dyn NativeContext,
    spreaders: bool,
    key: u64,
) -> Option<(ObjectRef, Option<ObjectRef>)> {
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let slot = (key as usize) & (IWA_LINK_MEMO_SLOTS - 1); // Cast: the low bits of a small key
    let lines = IWA_LINK_MEMO.with(std::cell::Cell::get);
    let line = if spreaders { lines.0[slot] } else { lines.1[slot] };
    if line.vm == vm && line.epoch == epoch && line.key == key {
        line.a.map(|a| (a, line.b))
    } else {
        None
    }
}

pub(crate) fn iwa_link_line_put(
    ctx: &dyn NativeContext,
    spreaders: bool,
    key: u64,
    a: ObjectRef,
    b: Option<ObjectRef>,
) {
    let line = IwaLinkLine {
        vm: ctx.vm_identity(),
        epoch: ctx.gc_address_epoch(),
        key,
        a: Some(a),
        b,
    };
    let slot = (key as usize) & (IWA_LINK_MEMO_SLOTS - 1); // Cast: the low bits of a small key
    IWA_LINK_MEMO.with(|memo| {
        let mut lines = memo.get();
        if spreaders {
            lines.0[slot] = line;
        } else {
            lines.1[slot] = line;
        }
        memo.set(lines);
    });
}

/// The `n`-argument spreader [`mh_iwa_spreader`] filed, from this thread's line.
pub(crate) fn iwa_spreader_line_get(ctx: &dyn NativeContext, n: usize) -> Option<ObjectRef> {
    iwa_link_line_get(ctx, true, n as u64).map(|(s, _)| s) // Widening: usize -> u64
}

pub(crate) fn iwa_spreader_line_put(ctx: &dyn NativeContext, n: usize, spreader: ObjectRef) {
    iwa_link_line_put(ctx, true, n as u64, spreader, None); // Widening: usize -> u64
}

/// The class id the door keys its linker row by (`mh_linked_invoker`'s `near`).
pub(crate) fn iwa_link_caller(ctx: &dyn NativeContext) -> Option<u32> {
    ctx.frame_class_ids_tail(1)
        .0
        .first()
        .copied()
        .map(|id| id.as_u32())
        .filter(|&id| id != 0)
}

/// After the door ran the spreader's `invokeExact`: remember the linker row it
/// keys by this caller (filed by its first call, read by every pair hit), if
/// there is one. Pure reads.
pub(crate) fn iwa_link_line_fill(ctx: &dyn NativeContext) {
    let Some(caller) = iwa_link_caller(ctx) else {
        return;
    };
    let key = cratonvm_native_api::method_type_table::class_keyed(
        &format!("linker:invokeExact:{MH_IWA_SPREAD_SITE}"),
        &[caller],
    );
    let Some(pair) = ctx.method_handle_linker_get(&key) else {
        return;
    };
    if ctx.array_length(pair) != 2 {
        return;
    }
    let (Value::Object(Some(member)), Value::Object(Some(appendix))) =
        (ctx.get_array_element(pair, 0), ctx.get_array_element(pair, 1))
    else {
        return;
    };
    iwa_link_line_put(ctx, false, u64::from(caller), member, Some(appendix));
}

/// The door's `invokeExact` pair hit for the native steps' spreader
/// (`mh_linked_invoker` at [`MH_IWA_SPREAD_SITE`]), answered from this
/// thread's line: `None` (the door) on a miss.
///
/// What the door asks per call, and why the line answers it the same:
/// * the spreader is not a carrier, has a `form`, and the site's factory
///   exists: the spreader is a JDK `BoundMethodHandle` whose `form` is final,
///   and the line was filed only after the door's pair hit;
/// * the native stack: asked here, raising the door's `StackOverflowError`;
/// * `mh_site_access_refusal`: the site names `MethodHandle`, `Object[]` and
///   `Object`, which no second loader can define, so it never refuses;
/// * the interface null-receiver NPE: only for a null first argument, and the
///   first argument is the `asType` adapter (never null);
/// * the `linker:` row: the line's member and appendix are that row's,
///   rooted, at their addresses for the line's epoch (the row is forgotten
///   only when the caller class unloads, which needs a collection).
///
/// Then as the pair hit: the arguments pinned, the root refresh as
/// `CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH` says, the compiled stack
/// budget rebased, `native_method_handle_link_to(spreader, adapter, array,
/// appendix, member)`.
pub(crate) fn iwa_link_memo_call(
    ctx: &mut dyn NativeContext,
    spreader: ObjectRef,
    extra: &[Value; 2],
) -> Option<MethodCallResult> {
    let caller = iwa_link_caller(&*ctx)?;
    let (member, appendix) = iwa_link_line_get(&*ctx, false, u64::from(caller))?;
    let appendix = appendix?;
    if !matches!(extra[0], Value::Object(Some(_))) {
        return None;
    }
    if crate::lang_reflect::reflective_native_stack_exhausted() {
        return Some(Err(RuntimeError::StackOverflowError.into()));
    }
    let budget = ctx.rebase_compiled_stack_budget(crate::lang_reflect::reflective_stack_bounds());
    let (base, handles) = pin_mh_args(ctx, extra);
    let this_pin = ctx.pin_native_root(spreader);
    let mut values = [Value::Object(None); 5];
    values[0] = Value::Object(Some(ctx.read_native_pin(this_pin, spreader)));
    values[1] = read_pinned_mh_arg(&*ctx, handles[0], extra[0]);
    values[2] = read_pinned_mh_arg(&*ctx, handles[1], extra[1]);
    values[3] = Value::Object(Some(appendix));
    values[4] = Value::Object(Some(member));
    if !crate::nbflags().mh_linker_hit_skips_root_refresh {
        ctx.refresh_root_snapshot();
    }
    let answer = crate::native_method_handle_link_to(ctx, &values);
    ctx.unpin_native_roots(this_pin);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    ctx.restore_compiled_stack_budget(budget);
    Some(answer)
}

/// `MethodHandle.invokeWithArguments(Object...)`'s `Intrinsic` (interpreter round i5 wave
/// 104, lane L3: the JDK frame recorded around the door, `with_invoke_with_arguments_frames`;
/// wave 109, lane L4: `CRATONVM_MH_IWA_JDK_BODY`). A named function since interpreter round
/// i7 wave 138, lane L4 ([`is_iwa_array_native_callback`]); the body is unchanged.
pub(crate) fn mh_iwa_array_intrinsic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 109, lane L4 (`CRATONVM_MH_IWA_JDK_BODY`, default off).
    if let Some(answer) = mh_iwa_jdk_body(ctx, args, MH_IWA_ARRAY_DESC) {
        return answer;
    }
    with_invoke_with_arguments_frames(
        ctx,
        args,
        cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments,
        mh_invoke_with_arguments_array_door,
    )
}

/// Is `callback` the array `invokeWithArguments` `Intrinsic` ([`mh_iwa_array_intrinsic`])?
/// The interpreter's cached `invokevirtual` asks it before
/// [`mh_iwa_interp_plan`] (interpreter round i7 wave 138, lane L4).
pub fn is_iwa_array_native_callback(callback: cratonvm_native_api::NativeCallback) -> bool {
    callback as usize == mh_iwa_array_intrinsic as cratonvm_native_api::NativeCallback as usize
}

/// What an interpreted `invokevirtual MethodHandle.invokeWithArguments(Object[])` pushes
/// instead of calling [`mh_iwa_array_intrinsic`] ([`mh_iwa_interp_plan`]): the JDK body's
/// frame at its `invokeExact` call (its local `invocationType` is `generic`), and on top of it
/// the linker frame `Invokers$Holder.invokeExact_MT(spreader, adapted, arguments, appendix)`,
/// the `MemberName` of which is `member`.
#[derive(Clone, Copy, Debug)]
pub struct IwaInterpPlan {
    /// The `n`-argument spread invoker (the linker's receiver).
    pub spreader: ObjectRef,
    /// `this.asType(genericMethodType(n))`.
    pub adapted: ObjectRef,
    /// `genericMethodType(n)` (the adapted handle's own `type`).
    pub generic: ObjectRef,
    /// The linker's `MemberName` (`Invokers$Holder.invokeExact_MT`).
    pub member: ObjectRef,
    /// The linker's `MethodType` appendix.
    pub appendix: ObjectRef,
}

/// Interpreter round i7 wave 138, lane L4 (`CRATONVM_MH_IWA_INTERP_FRAME_PUSH`,
/// `VmConfig::mh_iwa_interp_frame_push`, default OFF, performance, `--nojit` only; page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 138"): the native
/// steps' answers ([`mh_iwa_native_steps`] under `CRATONVM_MH_IWA_LINK_MEMO`) for an
/// interpreted call whose caller frame is of class `caller`, from this thread's memo lines
/// only, so that the interpreter can push the frames the door's single remaining re-entry
/// ran (`native_method_handle_link_to` into `invokeExact_MT`).
///
/// Pure lookups: no Java runs, nothing is allocated, so the references answered are at
/// their addresses in this address epoch (every line is keyed by it). A miss on any line
/// (the first calls, which run the door and fill them), a census, a variable-arity
/// collector, a carrier, or a handle not served by the JDK body declines (`None`), and the
/// `Intrinsic` runs as before. The one write is the spreader's `customizationCount` reset,
/// which [`mh_iwa_spreader`] makes before every use too, made only when the plan is
/// answered.
pub fn mh_iwa_interp_plan(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    array: Option<ObjectRef>,
    caller: u32,
) -> Option<IwaInterpPlan> {
    let flags = crate::nbflags();
    if !(flags.mh_iwa_jdk_body && flags.mh_iwa_native_steps)
        || caller == 0
        || !iwa_link_memo_enabled(&*ctx)
    {
        return None;
    }
    if !mh_with_arguments_runs_real(&*ctx, this) || mh_is_real_varargs_collector(&*ctx, this) {
        return None;
    }
    let n = array.map_or(0, |a| ctx.array_length(a));
    let adapted = iwa_adapted_memo_get(&*ctx, this, n)?;
    let spreader = iwa_spreader_line_get(&*ctx, n)?;
    let (member, appendix) = iwa_link_line_get(&*ctx, false, u64::from(caller))?;
    let appendix = appendix?;
    let generic = match invokers_mh_type_slot(&*ctx, adapted) {
        Some(slot) => ctx.get_field(adapted, slot),
        None => ctx.get_field_by_name(adapted, "type"),
    };
    let Value::Object(Some(generic)) = generic else {
        return None;
    };
    iwa_spreader_reset_count(&*ctx, spreader);
    Some(IwaInterpPlan {
        spreader,
        adapted,
        generic,
        member,
        appendix,
    })
}

/// The spreader's `customizationCount = 0` before a reuse ([`mh_iwa_spreader`],
/// [`mh_iwa_interp_plan`]). Interpreter round i7 wave 139, lane L4: through the slot
/// [`lane_customize_slots`] resolves once per VM (the by-name write cost a slot-table probe a
/// call), the by-name write when the layout is not resolved or the species is wider than the
/// lanes' bound (the write is then the same as before).
pub(crate) fn iwa_spreader_reset_count(ctx: &dyn NativeContext, spreader: ObjectRef) {
    lane_write_count(ctx, spreader, lane_customize_slots(ctx, spreader), 0);
}

/// [`mh_real_iwa_adapted`]'s memo line for (`this`, `n`) in this VM and address epoch, or
/// `None`. Pure reads (interpreter round i7 wave 138, lane L4).
pub(crate) fn iwa_adapted_memo_get(ctx: &dyn NativeContext, this: ObjectRef, n: usize) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = this.as_ptr() as usize;
    let slot = ((addr >> 3) ^ n) & (IWA_ADAPT_MEMO_SLOTS - 1);
    IWA_ADAPT_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.epoch == epoch && line.addr == addr && line.n == n {
            line.adapted
        } else {
            None
        }
    })
}

/// The key of the `n`-argument spread invoker in this VM's rooted bound-type table
/// (`method_handle_linker_get` / `_put`): keyed by `MethodHandle`'s class id, a boot class that
/// never unloads; the spreader names boot classes only (`(MethodHandle,Object[])Object` over
/// the generic invoker of `(Object*n)Object`), so a strong root retains no application loader.
pub(crate) fn iwa_spreader_key(ctx: &dyn NativeContext, n: usize) -> Option<String> {
    let mh = ctx.class_id_by_name("java/lang/invoke/MethodHandle")?;
    Some(cratonvm_native_api::method_type_table::class_keyed(
        &format!("iwa-spreader:{n}"),
        &[mh.as_u32()],
    ))
}

/// The spread invoker `invokeWithArguments`' array body asks for:
/// `MethodType.genericMethodType(n).invokers().spreadInvoker(0)`, the JDK's own calls
/// (interpreter round i6 wave 116, lane L4).
///
/// JDK 25's `Invokers.spreadInvoker` is uncached: it answers
/// `genericInvoker().asSpreader(Object[].class, n)`, a FRESH `BoundMethodHandle` per call
/// (`asSpreaderChecks`, `rebind`, `editor().spreadArgumentsForm`, `copyWith`). Under
/// `CRATONVM_MH_IWA_SPREADER_MEMO` (default off) the first answer is filed in this VM's rooted
/// bound-type table ([`iwa_spreader_key`]: a GC root, remapped by every collection, so no
/// address epoch is involved) and reused, which is invisible to Java: the spreader is the
/// receiver of the linker's `invokeExact` only (its form, `type` and bound invoker are the ones
/// a fresh call would build, `type` and form from the JDK's interned / cached objects), it never
/// escapes, and its `customizationCount` is set back to `0` before every reuse, so the linker's
/// `checkCustomized` sees what it sees on a fresh spreader and does not customize it (a fresh
/// spreader per call never reaches `CUSTOMIZE_THRESHOLD` on HotSpot either). Shared by the
/// VM's threads: the reset and the linker's increment race as HotSpot's own updates of a shared
/// handle's count do, so at worst a spreader customizes once, which changes no answer.
pub(crate) fn mh_iwa_spreader(
    ctx: &mut dyn NativeContext,
    n: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Wave 130 (lane L4, `CRATONVM_MH_IWA_LINK_MEMO`): the filed row's
    // spreader from this thread's line, without the key string, the class
    // lookup and the table probe.
    let link_memo = iwa_link_memo_enabled(&*ctx);
    if link_memo {
        if let Some(spreader) = iwa_spreader_line_get(&*ctx, n) {
            iwa_spreader_reset_count(&*ctx, spreader);
            return Ok(Some(spreader));
        }
    }
    let key = if crate::nbflags().mh_iwa_spreader_memo && ctx.vm_identity() != 0 {
        iwa_spreader_key(&*ctx, n)
    } else {
        None
    };
    if let Some(spreader) = key.as_deref().and_then(|k| ctx.method_handle_linker_get(k)) {
        if link_memo {
            // A rooted row's object: valid at this address for this epoch.
            iwa_spreader_line_put(&*ctx, n, spreader);
        }
        iwa_spreader_reset_count(&*ctx, spreader);
        return Ok(Some(spreader));
    }
    let Some(Value::Object(Some(generic))) = ctx.invoke_static(
        "java/lang/invoke/MethodType",
        "genericMethodType",
        "(I)Ljava/lang/invoke/MethodType;",
        &[Value::Int(n as i32)], // Cast: an array length fits i32
    )?
    else {
        return Ok(None);
    };
    let Some(Value::Object(Some(invokers))) =
        ctx.invoke_virtual(generic, "invokers", "()Ljava/lang/invoke/Invokers;", &[])?
    else {
        return Ok(None);
    };
    let Some(Value::Object(Some(spreader))) = ctx.invoke_virtual(
        invokers,
        "spreadInvoker",
        "(I)Ljava/lang/invoke/MethodHandle;",
        &[Value::Int(0)],
    )?
    else {
        return Ok(None);
    };
    // A race files one answer; every thread then uses the table's (a full
    // table hands back this one, unfiled).
    let spreader = match key.as_deref() {
        Some(k) => ctx.method_handle_linker_put(k, spreader),
        None => spreader,
    };
    Ok(Some(spreader))
}

/// `MethodHandle.invokeWithArguments(Object...)`'s door (the `Intrinsic`'s
/// body; interpreter round i5 wave 104, lane L3 moved it out of the
/// registration closure so `with_invoke_with_arguments_frames` can wrap it).
pub(crate) fn mh_invoke_with_arguments_array_door(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // SBR5 lane avh: a REAL handle runs the JDK's own `asType(generic)` and
    // its spun entry (`mh_real_invoke_with_arguments_door`); a downcall
    // carrier stays on the door (`mh_with_arguments_runs_real`).
    if mh_with_arguments_runs_real(&*ctx, this) {
        let array = match args.get(1) {
            Some(Value::Object(Some(a))) => Some(*a),
            _ => None,
        };
        return mh_real_invoke_with_arguments_door(ctx, this, array, None);
    }
    // A real JDK `DelegatingMethodHandle` is its target (wave 45).
    let this = mh_door_handle(ctx, this);
    // args[1] is an Object[] — unpack it
    let arr_ref = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            // JIT round 16 wave 11 (lane mhbind): `mh_downcall_door_type`.
            let (desc, kind) = mh_door_desc_and_kind(ctx, this);
            // Round 13 wave 3 hand-back (lane ffm2): see
            // `invoke_with_arguments_boxes_declared_return`. Read
            // before the dispatch, which can move `this`.
            let declared = mh_declared_descriptor(ctx, this);
            let boxes_declared =
                invoke_with_arguments_boxes_declared_return(&desc, declared.as_deref());
            let generic = generic_method_type_descriptor(0);
            // Round 15 wave 4 (lane mh3): a null array is no arguments.
            if let Some(refusal) =
                bound_handle_arity_refusal(ctx, this, &generic, declared.as_deref())
            {
                return Err(refusal);
            }
            // Wave 54 (lane L4): the kinds judged for `invokeExact` only.
            if let Some(refusal) = invoke_exact_typed_kind_arity_refusal(
                ctx,
                this,
                kind,
                &generic,
                declared.as_deref(),
            ) {
                return Err(refusal);
            }
            // Round 15 wave 5 (lane mh4): the return cast the array
            // path below makes (`return_cast_refusal`) is owed here
            // too: `h.asType(()String).invokeWithArguments((Object[])
            // null)` over an `Integer` answer is HotSpot's
            // `ClassCastException`, and this path answered it.
            let marks = mh_marking_bits(&*ctx, this);
            let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
            arm_entry_call_site(&generic);
            // Wave 58 (lane L4): an `asType` copy's reference leaf
            // return at its primitive declared one; wave 59: one plan;
            // wave 60: its tail decision too.
            let plan = DoorReturn::with_arguments(
                kind,
                &desc,
                declared.as_deref(),
                marks,
                explicit_stamp,
                boxes_declared,
            );
            let tail = if plan.with_arguments_works_after() {
                0
            } else {
                b'L'
            };
            let result = mh_dispatch_from_door(ctx, this, &[], tail);
            let result = result.map_err(|failed| {
                mh_deep_refusal_site_text(
                    ctx,
                    failed,
                    declared.as_deref(),
                    Some(&generic),
                    false,
                )
            });
            return door_return_apply(ctx, result, &plan);
        }
    };
    let len = ctx.array_length(arr_ref);
    let unpacked: Vec<Value> = (0..len)
        .map(|i| ctx.get_array_element(arr_ref, i))
        .collect();
    if let Some(refusal) = invoke_with_arguments_count_refusal(ctx, this, unpacked.len()) {
        return Err(refusal);
    }
    // `invokeWithArguments` is `asType(genericMethodType(n))` then an
    // exact invocation, so it casts exactly as `invoke` does -- and it
    // is a SEPARATE native, so the check on `invoke` does not reach it.
    // Measured: `cat.invokeWithArguments("ab", Integer.valueOf(3))`
    // answered `NoSuchMethodError` from inside `String.concat` after the
    // `invoke` door was already fixed.
    //
    // The declared type is read once, here (round 12 wave 7): the cast
    // refusal and the return cast after the dispatch both need it, and
    // nothing between here and the dispatch moves `this`.
    let declared = mh_declared_descriptor(ctx, this);
    // Round 15 wave 4 (lane mh3): `asType(genericMethodType(n))`'s
    // arity refusal for a bound (fixed-arity) handle comes first.
    let generic = generic_method_type_descriptor(unpacked.len());
    if let Some(refusal) =
        bound_handle_arity_refusal(ctx, this, &generic, declared.as_deref())
    {
        return Err(refusal);
    }
    // Wave 54 (lane L4): the kinds judged for `invokeExact` only.
    let kind_now = mh_kind_width_guarded(&*ctx, this);
    if let Some(refusal) = invoke_exact_typed_kind_arity_refusal(
        ctx,
        this,
        kind_now,
        &generic,
        declared.as_deref(),
    ) {
        return Err(refusal);
    }
    if let Some(refusal) =
        invoke_with_arguments_cast_refusal(ctx, this, &unpacked, declared.as_deref())
    {
        return Err(refusal);
    }
    // invokeWithArguments ALWAYS returns Object: a void target must yield
    // null (returning Ok(None) pushes NOTHING — the caller's areturn then
    // underflows the operand stack and killed the VM; Gradle's
    // MethodHandleBasedServiceMethod.invoke was the canonical victim) and
    // primitive returns must be boxed.
    // JIT round 16 wave 11 (lane mhbind): `mh_downcall_door_type`.
    let (desc, kind) = mh_door_desc_and_kind(ctx, this);
    arm_entry_call_site(&generic);
    // Round 12 wave 6 (orchestrator, hunter4 page item 3): the reference
    // return cast `invoke` and `invokeExact` perform (`return_cast_refusal`,
    // `CRATONVM_MH_RETURN_CAST`). The declared descriptor is the one read
    // above (round 15 wave 5, lane mh4: it was rendered a second time
    // here, a mirror-to-name and a `String` per parameter per call; it
    // is a `String`, so nothing since can have changed it).
    let marks = mh_marking_bits(&*ctx, this);
    let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
    // Round 13 wave 3 hand-back (lane ffm2): a primitive declared
    // return the leaf does not share is boxed as the DECLARED type
    // (`invoke_with_arguments_boxes_declared_return`); that is work
    // after the dispatch, so the door is not transparent then.
    let boxes_declared =
        invoke_with_arguments_boxes_declared_return(&desc, declared.as_deref());
    // Wave 58 (lane L4): an `asType` copy's reference leaf return at
    // its primitive declared one; wave 59: one plan (`DoorReturn`).
    let plan = DoorReturn::with_arguments(
        kind,
        &desc,
        declared.as_deref(),
        marks,
        explicit_stamp,
        boxes_declared,
    );
    // The door boxes a primitive result and hands a reference back (`MH_TAIL`
    // token `L`, waves 26 and 28) unless the plan has work to do after
    // the dispatch (wave 60: the plan's own projection).
    let tail = if plan.with_arguments_works_after() {
        0
    } else {
        b'L'
    };
    let result = mh_dispatch_from_door(ctx, this, &unpacked, tail);
    // Wave 54 (lane L4): a leaf's arity refusal names this call site.
    let result = result.map_err(|failed| {
        mh_deep_refusal_site_text(ctx, failed, declared.as_deref(), Some(&generic), false)
    });
    door_return_apply(ctx, result, &plan)
}

/// `MethodHandle.invokeWithArguments(List)`'s door (see
/// [`mh_invoke_with_arguments_array_door`]).
pub(crate) fn mh_invoke_with_arguments_list_door(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // SBR5 lane avh: a REAL handle runs the JDK's own `asType(generic)` and
    // its spun entry (`mh_real_invoke_with_arguments_door`); a downcall
    // carrier stays on the door (`mh_with_arguments_runs_real`).
    if mh_with_arguments_runs_real(&*ctx, this) {
        if let Some(Value::Object(Some(list))) = args.get(1).copied() {
            let this_pin = ctx.pin_native_root(this);
            let list_pin = ctx.pin_native_root(list);
            let toarray = ctx.invoke_virtual(list, "toArray", "()[Ljava/lang/Object;", &[]);
            let this = ctx.read_native_pin(this_pin, this);
            let list = ctx.read_native_pin(list_pin, list);
            let answer = match toarray {
                // Interpreter round i4 wave 84 (lane L4): the JDK body
                // is `invokeWithArguments(arguments.toArray())`, a
                // VIRTUAL call, which a variable-arity handle answers
                // with `AsVarargsCollector`'s override (the jumbo path:
                // more than 127 trailing arguments collected by hand).
                // The door below is `MethodHandle`'s body, whose
                // `genericMethodType(n)` refuses n > 255 with
                // `IllegalArgumentException: bad parameter count n`:
                // `varargs.invokeWithArguments(listOf300)` failed where
                // HotSpot collects. `L4W84InvokeWithArgumentsJumbo`
                // rows `list-*`.
                Ok(Some(Value::Object(Some(arr))))
                    if mh_is_real_varargs_collector(&*ctx, this) =>
                {
                    ctx.invoke_virtual_bytecode_only(
                        this,
                        "invokeWithArguments",
                        "([Ljava/lang/Object;)Ljava/lang/Object;",
                        &[Value::Object(Some(arr))],
                    )
                }
                Ok(Some(Value::Object(Some(arr)))) => mh_real_invoke_with_arguments_door(
                    ctx,
                    this,
                    Some(arr),
                    Some((list_pin, list)),
                ),
                Ok(_) => mh_real_invoke_with_arguments_door(
                    ctx,
                    this,
                    None,
                    Some((list_pin, list)),
                ),
                Err(e) => Err(e),
            };
            // Releases `list_pin` too (pins are released from a handle on).
            ctx.unpin_native_roots(this_pin);
            return answer;
        }
    }
    // A real JDK `DelegatingMethodHandle` is its target (wave 45).
    let this = mh_door_handle(ctx, this);
    // Unpack the List via its own toArray() — layout-agnostic (the old
    // direct slot reads assumed the ArrayList layout and silently saw 0
    // args for any other List implementation).
    // NOTE: the cast refusal for this overload is applied after the
    // List is unpacked, below -- see the `Object[]` overload above for
    // why `invokeWithArguments` needs its own.
    //
    // gc-common w18-b: `List.toArray()` runs Java -- a GC point -- and
    // every later line uses `this` (the cast refusal, `MH_DESC`,
    // `MH_KIND`, the dispatch). Pin it across the call and re-read it,
    // the way `bindTo` below does; the `Object[]` overload above makes
    // no call before its uses and needs none.
    let this_pin = ctx.pin_native_root(this);
    let unpacked: Vec<Value> = match args.get(1) {
        Some(Value::Object(Some(l))) => {
            match ctx.invoke_virtual(*l, "toArray", "()[Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(arr)))) => {
                    let len = ctx.array_length(arr);
                    (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
                }
                Ok(_) => Vec::new(),
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        }
        // The JDK body is `invokeWithArguments(arguments.toArray())`:
        // a null list is that call's helpful NPE (measured, HotSpot
        // 25). It used to invoke the handle with no arguments.
        // Interpreter round i1 wave 43, lane L4.
        Some(Value::Object(None)) => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.List.toArray()\" because \"arguments\" \
                     is null"
                        .to_string(),
                ),
            }
            .into());
        }
        _ => Vec::new(),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(refusal) = invoke_with_arguments_count_refusal(ctx, this, unpacked.len()) {
        return Err(refusal);
    }
    // Read once, after the re-read (round 12 wave 7; see the
    // `Object[]` overload).
    let declared = mh_declared_descriptor(ctx, this);
    // Round 15 wave 4 (lane mh3): as the `Object[]` overload.
    let generic = generic_method_type_descriptor(unpacked.len());
    if let Some(refusal) =
        bound_handle_arity_refusal(ctx, this, &generic, declared.as_deref())
    {
        return Err(refusal);
    }
    // Wave 54 (lane L4): the kinds judged for `invokeExact` only.
    let kind_now = mh_kind_width_guarded(&*ctx, this);
    if let Some(refusal) = invoke_exact_typed_kind_arity_refusal(
        ctx,
        this,
        kind_now,
        &generic,
        declared.as_deref(),
    ) {
        return Err(refusal);
    }
    if let Some(refusal) =
        invoke_with_arguments_cast_refusal(ctx, this, &unpacked, declared.as_deref())
    {
        return Err(refusal);
    }
    // Same Object-return contract as the Object[] overload above: box
    // primitives, void → null.
    // JIT round 16 wave 11 (lane mhbind): `mh_downcall_door_type`.
    let (desc, kind) = mh_door_desc_and_kind(ctx, this);
    arm_entry_call_site(&generic);
    // Round 12 wave 6 (orchestrator, hunter4 page item 3): the reference
    // return cast `invoke` and `invokeExact` perform (`return_cast_refusal`,
    // `CRATONVM_MH_RETURN_CAST`). The declared descriptor is the one read
    // above (round 15 wave 5, lane mh4: it was rendered a second time
    // here, a mirror-to-name and a `String` per parameter per call; it
    // is a `String`, so nothing since can have changed it).
    let marks = mh_marking_bits(&*ctx, this);
    let explicit_stamp = marks & MH_MARK_EXPLICIT_CAST != 0;
    // Round 13 wave 3 hand-back (lane ffm2): a primitive declared
    // return the leaf does not share is boxed as the DECLARED type
    // (`invoke_with_arguments_boxes_declared_return`); that is work
    // after the dispatch, so the door is not transparent then.
    let boxes_declared =
        invoke_with_arguments_boxes_declared_return(&desc, declared.as_deref());
    // Wave 58 (lane L4): an `asType` copy's reference leaf return at
    // its primitive declared one; wave 59: one plan (`DoorReturn`).
    let plan = DoorReturn::with_arguments(
        kind,
        &desc,
        declared.as_deref(),
        marks,
        explicit_stamp,
        boxes_declared,
    );
    // The door boxes a primitive result and hands a reference back (`MH_TAIL`
    // token `L`, waves 26 and 28) unless the plan has work to do after
    // the dispatch (wave 60: the plan's own projection).
    let tail = if plan.with_arguments_works_after() {
        0
    } else {
        b'L'
    };
    let result = mh_dispatch_from_door(ctx, this, &unpacked, tail);
    // Wave 54 (lane L4): a leaf's arity refusal names this call site.
    let result = result.map_err(|failed| {
        mh_deep_refusal_site_text(ctx, failed, declared.as_deref(), Some(&generic), false)
    });
    door_return_apply(ctx, result, &plan)
}

pub fn register_t4_method_handle_invoke(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mh = "java/lang/invoke/MethodHandle";

    // invoke(...) — polymorphic signature with automatic type adaptation.
    // Boxing/unboxing and widening conversions are applied implicitly.
    // Return value is auto-boxed since call-site expects Object.
    r.register_with_kind(
        mh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // TAKEN FIRST. `adapt_invoke_args` (in `mh_invoke_door`) unboxes
            // wrappers, which can re-enter the interpreter and reach the
            // signature-polymorphic dispatch block again — and that block
            // CLEARS the channel for every name it does not arm.
            let call_site = cratonvm_native_api::poly_call_site::take();
            // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`):
            // the caller the arming door named with the site, read with it.
            let armed_caller = cratonvm_native_api::poly_call_site::taken_caller();
            // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4): this call's
            // clock, charged on every return.
            let _census_clock = mh_invoke_census_clock(&*ctx, this, false);
            // Interpreter round i4 wave 94, lane L4 (`CRATONVM_MH_FIELD_LINK_FIRST`,
            // default off): a linked synthetic field accessor at its declared
            // site, ahead of the real-handle lanes (see `mh_field_link_first`).
            if crate::nbflags().mh_field_link_first {
                if let Some(result) = mh_field_link_first(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::Invoke,
                ) {
                    return result;
                }
            }
            // Interpreter round i4 wave 88, lane L4
            // (`CRATONVM_MH_REAL_CTOR_WIDENING_SERVE`, default off): a real
            // nullary constructor handle at an `()Object` site.
            if let Some(result) =
                mh_real_ctor_widening_serve(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i4 wave 95, lane L4
            // (`CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`, default off): a real
            // field accessor at a site of its own type, ahead of the linker.
            if let Some(result) =
                mh_real_field_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            // Interpreter round i5 wave 101, lane L4
            // (`CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default off): a
            // proven `bindTo` of a real instance field accessor.
            if let Some(result) = mh_real_bound_field_before_linker(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                "invoke",
                armed_caller,
            ) {
                return result;
            }
            // Interpreter round i5 wave 102, lane L4
            // (`CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER`, default off): a real
            // static / virtual method handle at a site of its own type.
            if let Some(result) =
                mh_real_direct_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            if let Some(result) =
                mh_linked_invoker(ctx, this, &args[1..], call_site.as_deref(), "invoke")
            {
                return result;
            }
            // JIT round 17 wave 6 (lane mh6): the site's link refuses a class
            // the caller may not access, before any type comparison.
            // Interpreter round i4 wave 95, lane L4: and before the interface
            // null-receiver check below, which is the handle's own (its
            // `Interface.checkReceiver` runs inside the linked form), so
            // HotSpot's link answers first, as `mh_linked_invoker` orders them.
            if let Some(refusal) = mh_site_access_refusal(&*ctx, call_site.as_deref()) {
                return Err(refusal);
            }
            // JIT round 18 wave 11 (lane mhresid): an interface handle's null
            // receiver, with HotSpot's message.
            if let Some(npe) =
                mh_interface_null_receiver_npe(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(npe);
            }
            // SBR3 mhperf: a real `DirectMethodHandle` called at its own type.
            if let Some(result) =
                mh_real_direct_fast(ctx, this, &args[1..], call_site.as_deref(), false, true)
            {
                return result;
            }
            // A real JDK `DelegatingMethodHandle` is its target (wave 45).
            let this = mh_door_handle(ctx, this);
            // JIT round 17 wave 2 (lane mh3): equal descriptor strings naming
            // another loader's class are `asType`'s cast, not an exact call.
            if let Some(refusal) =
                mh_invoke_loader_cast_refusal(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(refusal);
            }
            // JIT round 17 wave 3 (lane mh4): the result's cast across
            // loaders, asked before the call (pure reads), applied after.
            // The array target's lookup can create the array class (no Java,
            // no collection): `this` stays valid.
            let ret_cast = mh_invoke_return_cast_target(ctx, this, call_site.as_deref());
            // Interpreter round i2 wave 67 (lane L4,
            // `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`): a linked field accessor
            // called at EXACTLY its declared type is `invokeExact`.
            if ret_cast.is_none() {
                // Wave 80 (`CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE`, default
                // off): a real getter at a converting site whose adapter the
                // handle's `asTypeCache` already holds.
                if let Some(result) =
                    mh_real_field_converting_serve(ctx, this, &args[1..], call_site.as_deref())
                {
                    return result;
                }
                if let Some(result) =
                    mh_field_link_fast_for_invoke(ctx, this, &args[1..], call_site.as_deref())
                {
                    return result;
                }
                // Wave 68 (`CRATONVM_MH_INVOKE_LINKS_FIELD`): not linked yet
                // (or relinking): link it here at the declared type, as
                // `invokeExact` would; the door serves this call -- or, wave
                // 69 (`CRATONVM_MH_LINKING_CALL_SERVED`), the link just written.
                if let Some(result) = mh_field_link_try_and_serve(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::Invoke,
                ) {
                    return result;
                }
            }
            let result = mh_invoke_registered_body(ctx, this, &args[1..], call_site.as_deref());
            match ret_cast {
                Some(target) => mh_invoke_return_cast(&*ctx, target, result),
                None => result,
            }
        },
        NativeKind::Bridge,
    );

    // invokeExact(...) — strict type checking: argument count must match.
    // Throws WrongMethodTypeException if arity mismatches.
    // Return value is auto-boxed since call-site expects Object.
    r.register_with_kind(
        mh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // TAKEN FIRST and unconditionally, for the reason `invoke` above
            // takes first: anything that re-enters the interpreter can reach
            // the signature-polymorphic dispatch block, which CLEARS the
            // channel for every name it does not arm. Taking it here also
            // guarantees this native never leaves a descriptor armed for a
            // later dispatch through a door that armed none.
            let call_site = cratonvm_native_api::poly_call_site::take();
            // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`):
            // the caller the arming door named with the site, read with it.
            let armed_caller = cratonvm_native_api::poly_call_site::taken_caller();
            // `CRATONVM_DBG=mh-finder-census` (wave 89, lane L4).
            let _census_clock = mh_invoke_census_clock(&*ctx, this, true);
            // Interpreter round i4 wave 94, lane L4 (`CRATONVM_MH_FIELD_LINK_FIRST`,
            // default off): see `mh_field_link_first`.
            if crate::nbflags().mh_field_link_first {
                if let Some(result) = mh_field_link_first(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    LinkingDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // Wave 88, lane L3 (`CRATONVM_MH_REAL_CONSTRUCTOR_LANE`, default
            // off): the reflective accessor's nullary constructor handle at
            // `()Object`, ahead of the JDK invoker linkage below (which runs
            // the handle's whole `LambdaForm` chain on every call).
            if let Some(result) =
                mh_real_constructor_fast(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i9 (`CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE`,
            // default on, `--nojit` only): a typed one-argument constructor's
            // `asType` adapter (restored by t1 lane H; the lang_invoke
            // decomposition had dropped this call with the lane).
            if let Some(result) =
                mh_real_ctor_cast_adapter_fast(ctx, this, &args[1..], call_site.as_deref())
            {
                return result;
            }
            // Interpreter round i4 wave 95, lane L4
            // (`CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`, default off).
            if let Some(result) =
                mh_real_field_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller)
            {
                return result;
            }
            // Interpreter round i5 wave 101, lane L4
            // (`CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default off).
            if let Some(result) = mh_real_bound_field_before_linker(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                "invokeExact",
                armed_caller,
            ) {
                return result;
            }
            // Interpreter round i5 wave 102, lane L4
            // (`CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER`, default off): a real
            // static / virtual method handle at a site of its own type.
            set_mh_tail(call_site.as_deref().map_or(0u8, |cs| door_tail_token(return_type_desc(cs))));
            let before_linker =
                mh_real_direct_before_linker(ctx, this, &args[1..], call_site.as_deref(), armed_caller);
            set_mh_tail(0);
            if let Some(result) = before_linker {
                return result;
            }
            if let Some(result) =
                mh_linked_invoker(ctx, this, &args[1..], call_site.as_deref(), "invokeExact")
            {
                return result;
            }
            // Round 19: a downcall carrier the JIT would bind at this site
            // (none of the refusals below can fire for it).
            if let Some(result) =
                crate::panama::interp_downcall_lane(ctx, args, call_site.as_deref())
            {
                return result;
            }
            // JIT round 17 wave 6 (lane mh6): see `mh_site_access_refusal`.
            // Interpreter round i4 wave 95, lane L4: asked before the interface
            // null-receiver check (HotSpot links the site first), as
            // `mh_linked_invoker` and the `invoke` native order them.
            if let Some(refusal) = mh_site_access_refusal(&*ctx, call_site.as_deref()) {
                return Err(refusal);
            }
            // JIT round 18 wave 11 (lane mhresid): see `mh_interface_null_receiver_npe`.
            if let Some(npe) =
                mh_interface_null_receiver_npe(ctx, this, &args[1..], call_site.as_deref())
            {
                return Err(npe);
            }
            // SBR3 mhperf: a real `DirectMethodHandle` called at its own type.
            set_mh_tail(call_site.as_deref().map_or(0u8, |cs| door_tail_token(return_type_desc(cs))));
            if let Some(result) =
                mh_real_direct_fast(ctx, this, &args[1..], call_site.as_deref(), false, true)
            {
                return result;
            }
            set_mh_tail(0);
            // Wave 73 (`CRATONVM_MH_REAL_FIELD_RECORD`): a real field accessor
            // at its declared type, with the two site refusals below.
            if let Some(result) =
                mh_real_field_record_fast(ctx, this, &args[1..], call_site.as_deref(), false)
            {
                return result;
            }
            // A real JDK `DelegatingMethodHandle` is its target (wave 45).
            let this = mh_door_handle(ctx, this);
            // (The site's access refusal was asked above, wave 95.)
            if let Some(refusal) = mh_real_exact_refusal(ctx, this, call_site.as_deref()) {
                return Err(refusal);
            }
            if let Some(result) = mh_real_direct_dispatch(ctx, this, &args[1..], false) {
                return result;
            }
            if let Some(result) =
                mh_real_lambda_form_dispatch(ctx, this, &args[1..], call_site.as_deref(), false)
            {
                return result;
            }
            // Round 13 wave 10 (lane callcost4): see `mh_direct_lane`.
            if mh_direct_lane_enabled() {
                if let Some(result) = mh_direct_lane(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    DirectLaneDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // Round 16 (lane mh, MH16-2): an `asType` copy of a static handle
            // at its declared type takes the converting lane.
            if mh_converting_lane_enabled() {
                if let Some(result) = mh_converting_lane(
                    ctx,
                    this,
                    &args[1..],
                    call_site.as_deref(),
                    DirectLaneDoor::InvokeExact,
                ) {
                    return result;
                }
            }
            // A field getter/setter linked on an earlier call is served
            // without the door; an unlinked one is linked when this call is
            // one the door serves by a plain field access (SBR wave 5, lane
            // reflacc -- see `mh_field_link_fast`).
            if let Some(result) = mh_field_link_fast(ctx, this, &args[1..], call_site.as_deref()) {
                return result;
            }
            // Wave 69 (`CRATONVM_MH_LINKING_CALL_SERVED`): the linking call
            // itself is served by the link it wrote.
            if let Some(result) = mh_field_link_try_and_serve(
                ctx,
                this,
                &args[1..],
                call_site.as_deref(),
                LinkingDoor::InvokeExact,
            ) {
                return result;
            }
            // Shared channel text (MH15-2, L7): the door borrows it.
            mh_invoke_exact_door(ctx, this, &args[1..], call_site.as_deref(), true)
        },
        NativeKind::Bridge,
    );
    r.register_with_kind(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        // Interpreter round i7 wave 138, lane L4: a named function, so the
        // interpreter can recognise the cached entry
        // ([`is_iwa_array_native_callback`]).
        mh_iwa_array_intrinsic,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        mh,
        "invokeWithArguments",
        "(Ljava/util/List;)Ljava/lang/Object;",
        // Wave 104, lane L3: as the array overload.
        |ctx, args| {
            // Wave 109, lane L4 (`CRATONVM_MH_IWA_JDK_BODY`, default off).
            if let Some(answer) =
                mh_iwa_jdk_body(ctx, args, "(Ljava/util/List;)Ljava/lang/Object;")
            {
                return answer;
            }
            with_invoke_with_arguments_frames(
                ctx,
                args,
                cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList,
                mh_invoke_with_arguments_list_door,
            )
        },
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        mh,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let recv = args.get(1).copied().unwrap_or(Value::Object(None));
            // SBR5 lane avh: a repeat bind of a REAL handle is the JDK's answer for the
            // first bind with the bound argument replaced (`mh_real_bind_to`).
            if let Some(answer) = mh_real_bind_to(ctx, this, recv) {
                return answer;
            }
            // Wave 54 (lane L4): a real JDK handle binds through the JDK.
            if let Some(answer) = mh_foreign_bind_to(ctx, this, recv) {
                return answer;
            }
            // Interpreter round i2 wave 62, lane L4 (`CRATONVM_MH_INSTANCE_OWNER_ID`):
            // an unbound instance handle's recorded owner casts the receiver by
            // id, before anything here allocates. A lambda proxy's handle
            // (`MH_CLASS` `""`, a `type` naming no class) bound any value.
            if let Some(refusal) = bind_to_owner_refusal(ctx, this, recv) {
                return Err(refusal);
            }
            // gen r4w3/rooting: `this` and `recv` are read and stored after
            // `alloc_method_handle` / `build_method_type_from_descriptor`
            // (GC points) below. Pin both now; `this_pin` (lowest) releases
            // everything pinned in this native before it returns a handle.
            let this_pin = ctx.pin_native_root(this);
            let recv_pin = pin_mh_value(ctx, recv);
            // JIT round 16 wave 11 (lane mhbind; MHF-2,
            // `CRATONVM_MH_BINDTO_FAST_MINT`): a repeat bind of the same,
            // unchanged source in this collection epoch, with a bound value
            // the line admits, is the mint alone (`bindto_fast_mint_hit`).
            // The key is read before anything here can collect.
            let mint_key = bindto_fast_mint_probe(&*ctx, this);
            if let Some(key) = &mint_key {
                if let Some(mut answer) = bindto_fast_mint_hit(&*ctx, key, recv) {
                    // Wave 66: a field line's carry, re-read off the source.
                    if bindto_fast_mint_refresh_carry(&*ctx, this, &mut answer) {
                        return bindto_fast_mint_finish(
                            ctx, this_pin, this, recv_pin, recv, &answer,
                        );
                    }
                }
            }
            // What the cast verdict below lets the fast mint remember.
            let mut mint_admits: Option<Option<u32>> = None;
            // W7-19: `bindTo` must REFUSE a target with no leading reference
            // parameter. `MethodHandle.bindTo`'s javadoc: "@throws
            // IllegalArgumentException if the target does not have a leading
            // parameter type that is a reference type", implemented in
            // `MethodType.leadingReferenceParameter()` as
            //   if (ptypes.length == 0 || ptypes[0].isPrimitive())
            //       throw newIllegalArgumentException("no leading reference parameter");
            // — so the test is arity-and-primitiveness, nothing else, and the
            // message is verbatim. Measured on the shipped binary under
            // `--real-jdk`: `findStatic(…(String,int)int).bindTo("abc")
            // .bindTo(2)` was ACCEPTED and answered 6, and `(int)int`
            // .bindTo(3) was accepted too; HotSpot 25 raises
            // `IllegalArgumentException: no leading reference parameter` for
            // both. A missing refusal, not a wrong value.
            //
            // The parameter list is read from the `type` field ONLY, never
            // from `mh_type_descriptor`'s `MH_DESC` fallback. That distinction
            // is the whole safety of this check: `MH_DESC` on a
            // virtual/special handle omits the receiver `alloc_method_handle`
            // prepends to `type`, so `Holder.pub`'s `(I)I` would read as a
            // primitive leading parameter and this would refuse a bind that
            // HotSpot accepts. No `type` MethodType, or one whose mirrors do
            // not render, means the leading parameter is UNKNOWN and the bind
            // is allowed through — a refusal is only ever raised on a positive
            // reading.
            //
            // DONE 2026-08-30, narrowly — the `cast` half of the JDK's one-line
            // body `type.leadingReferenceParameter().cast(x)`, which also raises
            // `ClassCastException` for a wrong REFERENCE type. See the second
            // check below.
            //
            // W7-19 §5.1 deferred it, and its reason still governs the SHAPE of
            // the check: an assignability question asked with `is_subclass`
            // alone answers FALSE for a fabricated stand-in against a real JDK
            // interface (a fabricated class declares no interfaces, so every
            // type test against one fails), and `bindTo` is on the Groovy-indy /
            // SpEL-FunctionReference / log4j-provider path in this tree, all of
            // which bind interfaces and subtypes. A false `ClassCastException`
            // there refuses working code, which is worse than the wrong answer
            // it replaces.
            //
            // What changed is not the risk but the available predicates: the
            // check below never refuses an INTERFACE target (where the false
            // negative lives), consults `synthetic_implements_declared` for the
            // VM-minted stand-ins, and refuses only on a positive reading. The
            // lane §5.1 asked for also exists now —
            // `probes/CodegenFrameworkSmoke.java` boots Groovy, ByteBuddy,
            // Mockito, ASM, Javassist and Objenesis as themselves.
            //
            // The test immediately below stays syntactic — is the first
            // descriptor token `L…;`/`[…` — and cannot be wrong for a reason
            // outside its own two lines.
            //
            // Round 16 (lane mh): the render is kept (`rendered_type`) for the
            // bound type below (`bindto_bound_type_descriptor_with`).
            let mut rendered_type: Option<String> = None;
            if let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") {
                if let Some(tdesc) = methodtype_to_descriptor(ctx, mt) {
                    if let Some((params, _)) = split_descriptor_params(&tdesc) {
                        let leading_is_reference = params
                            .first()
                            .is_some_and(|p| p.starts_with('L') || p.starts_with('['));
                        if !leading_is_reference {
                            return Err(RuntimeError::IllegalArgumentException {
                                message: "no leading reference parameter".to_string(),
                            }
                            .into());
                        }
                        // The `cast` half of `type.leadingReferenceParameter()
                        // .cast(x)`, deferred above until it could be written
                        // WITHOUT the false positive that made it dangerous.
                        //
                        // Measured, `probes/L8InvokeLookupSweep.java`:
                        //   findVirtual(String, "length", ()int)
                        //     .bindTo(Integer.valueOf(1))
                        //   HotSpot   ClassCastException
                        //   CratonVM  accepted, answers a handle of type ()int
                        // A missing check, and the wrong receiver is then live
                        // in a handle that looks perfectly well-typed.
                        //
                        // `reference_arg_admitted` is the shared predicate --
                        // `MethodHandle.invoke` needs the identical judgement on
                        // its own arguments, and writing it twice is how the two
                        // doors come to disagree about one object. It answers
                        // `Some(false)` only on a positive mismatch; every
                        // "cannot tell" case is an allow, and its doc comment
                        // has the reason for each.
                        if let Some(pdesc) = params.first().cloned() {
                            let verdict = reference_arg_admitted(ctx, recv, &pdesc);
                            if verdict == Some(false) {
                                return Err(reference_cast_failure(ctx, recv, &pdesc));
                            }
                            if mint_key.is_some() {
                                mint_admits = bindto_fast_mint_admits(&*ctx, recv, &pdesc, verdict);
                            }
                        }
                    }
                    rendered_type = Some(tdesc);
                }
            }
            // Clone the MH and set BOUND field
            let class = mh_read_class(ctx, this);
            let name = mh_read_name(ctx, this);
            let desc = mh_read_desc(ctx, this);
            // Round 16 (lane mh): whether all three decoded, for
            // `bindto_shared_mint_applies_read` (which used to decode them again).
            let strings_read = class.is_some() && name.is_some() && desc.is_some();
            let class = class.unwrap_or_default();
            let name = name.unwrap_or_default();
            let desc = desc.unwrap_or_default();
            let kind = match ctx.get_field(this, MH_KIND) {
                Value::Int(k) => k,
                _ => MH_KIND_VIRTUAL,
            };
            // A SECOND `bindTo` on an already-bound direct (static/virtual/special)
            // handle binds the NEXT parameter, not the one already captured in the
            // single-slot MH_BOUND. Overwriting MH_BOUND silently dropped the first
            // capture — e.g. `String::formatted`.bindTo(template).bindTo(argsArray)
            // lost the `template` receiver, so dispatch invoked `formatted` on the
            // bound `Object[]` and hard-failed `NoSuchMethodError Object.formatted`
            // (ExpressionLanguageScenarioTests #messageBound / #messageStaticBound).
            // Model the extra capture as `insertArguments(this, 0, {recv})`: the
            // INSERT adapter splices `recv` into `this`'s remaining leading
            // parameter at dispatch while `this` keeps its own prior binding.
            //
            // Round 15 wave 4 (lane mh3): and so does every kind whose dispatch
            // does not read `MH_BOUND` as the captured leading argument (an
            // adapter's wrapper, a constructor's class id, an invoker, a static
            // setter; see `bindto_takes_insert_adapter`). A constructor's
            // `MH_DESC` returns `V`, which the doors box to `null`, so its
            // adapter carries the constructor's `type` descriptor instead
            // (read here, while `this` is current).
            let already_bound = matches!(ctx.get_field(this, MH_BOUND), Value::Object(Some(_)));
            // Round 15 wave 5 (lane mh4): an unbound static handle's recorded
            // owner (`MH_BOUND`'s `Int`), which the bound copy keeps in
            // `MH_STATIC_OWNER`. Read while `this` is current; an `Int`.
            let static_owner = if kind == MH_KIND_STATIC && !already_bound {
                mh_static_owner(ctx.get_field(this, MH_BOUND))
            } else if kind == MH_KIND_SPECIAL && !already_bound {
                // Interpreter round i2 wave 64, lane L4: a `findSpecial`
                // handle's dispatch class by id (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`)
                // rides to the bound copy in the same slot, so the bound
                // handle's `SPECIAL` arm calls by it too.
                mh_special_owner_with(&*ctx, this, mh_special_by_owner_id_enabled())
            } else {
                None
            };
            // Interpreter round i2 wave 65, lane L4 (`CRATONVM_MH_BOUND_FIELD_OWNER`):
            // an unbound instance field handle's recorded owner and minted
            // slot ride to the bound copy, so its arm resolves the slot by id.
            // Read while `this` is current.
            let field_carry = if kind == MH_KIND_GETTER || kind == MH_KIND_SETTER {
                bound_field_carry_with(
                    &*ctx,
                    this,
                    kind,
                    &desc,
                    mh_instance_owner(&*ctx, this),
                    mh_bound_field_owner_enabled(),
                )
            } else {
                None
            };
            // A Panama downcall carrier has no `MH_KIND` (read as virtual
            // above) and dispatches through `mh_dispatch`'s downcall arm, so
            // `downcall.bindTo(allocator)` takes the adapter too; the copy
            // minted a virtual handle with no class, name or descriptor,
            // which is no longer a downcall and cannot dispatch.
            let insert_adapter =
                ((kind == MH_KIND_STATIC || kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL)
                    && already_bound)
                    || bindto_takes_insert_adapter(kind, already_bound, &desc)
                    || bindto_null_takes_insert_adapter(kind, recv, &desc)
                    || (crate::panama::is_downcall_handle(&*ctx, this)
                        && cratonvm_types::flags::runtime_flag_default_on(
                            "CRATONVM_MH_BINDTO_INSERT_ADAPTER",
                        ));
            let constructor_type = if insert_adapter && kind == MH_KIND_CONSTRUCTOR {
                mh_type_descriptor(ctx, this)
            } else {
                None
            };
            if insert_adapter {
                // gc-common w20-f: `recv` and `this` are re-read AFTER the
                // array's allocation (they were re-read before it and stored
                // after it), and the carrier holds `this` and the array at
                // their current addresses.
                let values = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
                let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
                ctx.set_array_element(values, 0, recv);
                let this = ctx.read_native_pin(this_pin, this);
                let wrapper = alloc_mh_carrier_holding(
                    ctx,
                    "__mh_insert_wrapper__",
                    &[
                        Value::Object(Some(this)),
                        Value::Object(Some(values)),
                        Value::Int(0),
                    ],
                );
                // gen r4w3/rooting: pin `wrapper` across the allocation;
                // re-read `this`, and keep `adapter` pinned across the
                // MethodType build.
                let wrapper_pin = ctx.pin_native_root(wrapper);
                let adapter = alloc_method_handle(
                    ctx,
                    "__adapter__",
                    "insert",
                    constructor_type.as_deref().unwrap_or(&desc),
                    MH_KIND_INSERT,
                )?;
                let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
                let this = ctx.read_native_pin(this_pin, this);
                let adapter_pin = ctx.pin_native_root(adapter);
                ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
                // Binding one value REMOVES the leading parameter from `this`'s
                // type. The adapter's `type()` must reflect that (e.g. a fully
                // bound handle reports arity 0); otherwise SpEL's
                // `FunctionReference` reads a stale 1-arg type and re-wraps the
                // call args via `setupArgumentsForVarargsInvocation`, nesting an
                // extra empty `Object[]` into the varargs
                // (#messageBound → "...[Ljava.lang.Object;@..."). Mirror the
                // `MethodHandles.insertArguments` type adjustment.
                if let Some(tdesc) = mh_type_descriptor(ctx, this) {
                    if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                        if !params.is_empty() {
                            params.remove(0); // one value inserted at pos 0
                        }
                        let new_desc = format!("({}){}", params.concat(), ret);
                        let src = Some((ctx.read_native_pin(this_pin, this), kind));
                        if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, &new_desc, src)
                        {
                            let adapter = ctx.read_native_pin(adapter_pin, adapter);
                            ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                        }
                    }
                }
                let adapter = ctx.read_native_pin(adapter_pin, adapter);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(adapter))));
            }
            // Round 12 wave 7: what a stamped `this` (an `asType` /
            // `explicitCastArguments` copy) owes the value it binds and the
            // handle this mints -- the casts of the types beneath its `type`
            // (`bound_cast_chain`; `bindTo`'s own cast above is to the outer
            // type only), its explicit marking, and for a virtual/special
            // handle its own `type` minus the receiver rather than `MH_DESC`.
            // A fresh mint from `MH_CLASS`/`MH_DESC` dropped all three:
            // `findVirtual(Mid, m).asType((Object,int)int).bindTo(grand)`
            // dispatched `Mid.m` on a `Grand`. Read here, before the
            // allocation below, while `this` and `recv` are current.
            let bound_chain = bound_cast_chain(ctx, this, recv, kind);
            let chain_free = bound_chain.is_none();
            let bound_explicit = bound_chain.is_some() && mh_is_explicit_cast(&*ctx, this);
            let bound_virtual_type = if bound_chain.is_some()
                && (kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL)
            {
                mh_declared_descriptor(ctx, this).and_then(|d| descriptor_without_first_param(&d))
            } else {
                None
            };
            // Round 15 wave 4 (lane mh3): the bound handle is FIXED-arity
            // (`mh_bindto_fixed_arity_enabled`), and, for the kinds
            // `bindto_shared_mint_applies` names, minted with `this`'s own
            // strings and its bound `type` built once (MH2-3). `shared` says
            // the two `type` rebuilds below have nothing left to do.
            let fixed_arity = mh_bindto_fixed_arity_enabled();
            let shared = bindto_shared_mint_applies_read(&*ctx, this, kind, strings_read);
            // MHF-2: the shared mint's two answers, for the fast-mint line.
            let mut mint_answer: Option<(std::rc::Rc<str>, bool)> = None;
            let new_mh = if shared {
                let type_desc = bindto_bound_type_descriptor_with(
                    ctx,
                    this,
                    kind,
                    &desc,
                    bound_virtual_type.as_deref(),
                    rendered_type.as_deref(),
                );
                let collects =
                    !fixed_arity && method_is_variable_arity(&*ctx, &class, &name, &desc, kind);
                if mint_key.is_some() && fixed_arity {
                    mint_answer = Some((std::rc::Rc::from(type_desc.as_str()), collects));
                }
                // Wave 66 (`CRATONVM_MH_FIELD_LINK_BOUND`): wide for the bound
                // field link too.
                let link_slots =
                    bound_field_copy_needs_link_slots(field_carry, mh_field_link_bound_enabled());
                alloc_bound_method_handle(
                    ctx, this_pin, this, kind, &type_desc, collects, link_slots,
                )?
            } else {
                let new_mh = alloc_method_handle(ctx, &class, &name, &desc, kind)?;
                if fixed_arity {
                    mh_set_varargs_collector(&*ctx, new_mh, false);
                }
                new_mh
            };
            // gen r4w3/rooting: re-read `this` after the allocation, and keep
            // `new_mh` pinned across the MethodType builds below.
            let this = ctx.read_native_pin(this_pin, this);
            let new_mh_pin = ctx.pin_native_root(new_mh);
            // `bindTo` captures the LEADING argument, so the bound handle's
            // `type()` must have that leading parameter REMOVED (HotSpot:
            // `(Recv,Object)int`.bindTo(r) -> `(Object)int`). For virtual/special
            // handles the leading param is the receiver that `alloc_method_handle`
            // just prepended to `type`; reset `type` to the raw (receiver-less)
            // descriptor so e.g. Groovy's indy `sameClasses` guard builds a
            // classes[] of the right length (was AIOOBE: classes longer than the
            // runtime args). STATIC-handle leading-arg drop is a separate path.
            if shared {
                // Built by `alloc_bound_method_handle` from the same answers
                // (`bindto_bound_type_descriptor`).
            } else if kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL {
                let type_desc = bound_virtual_type.as_deref().unwrap_or(&desc);
                let src = Some((ctx.read_native_pin(this_pin, this), kind));
                if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, type_desc, src) {
                    let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                    ctx.set_field_by_name(new_mh, "type", Value::Object(Some(mt)));
                }
            } else if kind == MH_KIND_STATIC
                || kind == MH_KIND_GETTER
                || kind == MH_KIND_SETTER
                || kind == MH_KIND_ARRAY_GET
                || kind == MH_KIND_ARRAY_SET
                || kind == MH_KIND_IDENTITY
                || kind == MH_KIND_CONSTANT
                || kind == MH_KIND_THROW
                || kind == MH_KIND_ARRAY_LENGTH
                // Round 15 wave 4 (lane mh3): a lambda factory's `type` kept
                // every capture, so a SECOND bind (log4j's `factory.bindTo(
                // serviceType).bindTo(classLoader)`) was cast by the guard above
                // against the FIRST capture's type and refused. Its dispatch
                // counts captures from `MH_DESC`, never from `type`.
                || (kind == MH_KIND_LAMBDA_FACTORY
                    && cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_MH_BINDTO_FACTORY_TYPE",
                    ))
            {
                // Wave 40 (lane L4) added `identity` and `constant`/`empty`:
                // `identity(String).bindTo(x)` is `()String` and
                // `empty((String)int).bindTo(x)` is `()int` on HotSpot; the
                // bound copy kept `(String)String` / `(String)int`, which
                // `invokeExact`'s type check (now judging both kinds) would
                // have refused at the `()String` site, and a second `bindTo`
                // was accepted where HotSpot refuses it.
                // `arrayLength(int[].class).bindTo(a)` likewise is `()int`.
                // STATIC `bindTo` captures the leading PARAMETER (not a receiver),
                // so the bound handle's `type()` must drop that leading parameter
                // — `(String,String[])R`.bindTo(s) -> `(String[])R`. The dispatch
                // path keys off MH_DESC, but downstream `type()` readers do not:
                // SpEL's `FunctionReference` reads the arity to decide varargs
                // repackaging, and a chained `bindTo` (#messageStaticBound) derives
                // its own arity from this one. Leaving the full type here left a
                // stale extra parameter that mis-packed the varargs.
                //
                // W7-19 §5.3 added the four accessor kinds, which take the same
                // drop for the same reason and were the only kinds left out.
                // Measured on the shipped binary against HotSpot 25, same class
                // file: `findGetter(H,"i",int)` bound to a receiver reported
                // `(H)int` where HotSpot reports `()int`, and
                // `arrayElementGetter(int[])` bound to an array reported
                // `([I,int)int` where HotSpot reports `(int)int`. A getter's
                // `type` is its raw descriptor `(LH;)I` — `alloc_method_handle`
                // prepends a receiver only for VIRTUAL/SPECIAL — so slot 0 of the
                // parameter list IS the value `bindTo` just captured, exactly as
                // for STATIC. Static getters/setters (`()I`, `(I)V`) never reach
                // here: the guard at the top of this native already refuses a
                // zero-arity or primitive-leading target, which is what HotSpot
                // does too.
                //
                // This TIGHTENS the guard above on those two shapes, and that is
                // the point rather than a side effect: those were the only two
                // rows in W7-19 §3.3's thirteen-shape census where CratonVM
                // under-refused a second `bindTo` that HotSpot refuses. Before
                // this, a second bind on a bound getter fell through to the
                // allocation below and silently OVERWROTE the first capture;
                // now it raises HotSpot's `IllegalArgumentException`.
                if let Some(tdesc) = mh_type_descriptor(ctx, this) {
                    if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                        if !params.is_empty() {
                            params.remove(0);
                        }
                        let new_desc = format!("({}){}", params.concat(), ret);
                        let src = Some((ctx.read_native_pin(this_pin, this), kind));
                        if let Ok(Some(mt)) = bound_method_type_from_descriptor(ctx, &new_desc, src)
                        {
                            let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                            ctx.set_field_by_name(new_mh, "type", Value::Object(Some(mt)));
                        }
                    }
                }
            }
            // gen r4w3/rooting: current addresses after the MethodType builds.
            let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
            let this = ctx.read_native_pin(this_pin, this);
            let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
            if kind == MH_KIND_LAMBDA_FACTORY {
                // A lambda factory may capture more than one value (e.g. log4j's
                // `factory.bindTo(serviceType).bindTo(classLoader)`). The single-slot
                // MH_BOUND used by the other kinds would drop all but the last bind,
                // so accumulate captures, in bind order, into an Object[].
                //
                // gc-common w15-e: the capture array is allocated AFTER the
                // previous captures, `recv` and `new_mh` were read, and all
                // three are used after it. The pins used to be released
                // before that allocation; now `this_pin` (and with it
                // `recv_pin` / `new_mh_pin`) is held across it, the previous
                // capture array gets its own pin, and each is re-read after.
                let prev = ctx.get_field(this, MH_BOUND);
                let old_pin = match prev {
                    Value::Object(Some(old)) => Some((ctx.pin_native_root(old), old)),
                    _ => None,
                };
                let oldlen = match old_pin {
                    Some((_, old)) => ctx.array_length(old),
                    None => 0,
                };
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, oldlen + 1);
                if let Some((pin, old)) = old_pin {
                    let old = ctx.read_native_pin(pin, old);
                    for i in 0..oldlen {
                        let v = ctx.get_array_element(old, i);
                        ctx.set_array_element(arr, i, v);
                    }
                }
                let recv = read_pinned_mh_arg(ctx, recv_pin, recv);
                ctx.set_array_element(arr, oldlen, recv);
                let new_mh = ctx.read_native_pin(new_mh_pin, new_mh);
                ctx.set_field(new_mh, MH_BOUND, Value::Object(Some(arr)));
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(new_mh))));
            }
            // Round 12 wave 7: the stamped handle's chain, minus the bound
            // parameter, and its explicit marking (see `bound_chain` above).
            // The chain's text is allocated: `new_mh` comes back current, and
            // `recv` is re-read from its pin before the pins are released.
            let (new_mh, recv) = match bound_chain {
                Some(chain) => {
                    if bound_explicit {
                        mh_mark_explicit_cast(ctx, new_mh);
                    }
                    let new_mh = mh_store_cast_chain(ctx, new_mh, &chain);
                    (new_mh, read_pinned_mh_arg(ctx, recv_pin, recv))
                }
                None => (new_mh, recv),
            };
            // A `constant` / `empty` handle keeps answering its constant: the
            // bound argument is dropped, exactly as its dispatch ignores every
            // argument (wave 40, lane L4). Storing `recv` here made
            // `empty((String)int).bindTo("x")` unbox the String as its `int`
            // answer. No allocation since `this` was re-read (`bound_cast_chain`
            // is `None` for this kind), so the read through the pin is current.
            let recv = if kind == MH_KIND_CONSTANT {
                let this = ctx.read_native_pin(this_pin, this);
                ctx.get_field(this, MH_BOUND)
            } else {
                recv
            };
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(new_mh, MH_BOUND, recv);
            if let Some(owner) = static_owner {
                if ctx.object_num_fields(new_mh) > MH_STATIC_OWNER
                    && mh_bindto_static_owner_enabled()
                {
                    ctx.set_field(new_mh, MH_STATIC_OWNER, Value::Int(owner.as_u32() as i32));
                }
            }
            // Wave 65 (lane L4): the field carry (`field_carry` above). No GC
            // point since `new_mh` was last made current.
            let mut prelink = None;
            if let Some(carry) = field_carry {
                mh_write_bound_field_carry(&*ctx, new_mh, carry);
                // Wave 67 (`CRATONVM_MH_BINDTO_PRELINK_FIELD`): link the copy
                // now, so its first `invokeExact` is served by the link. After
                // every write of the mint (`type`, the chain, `MH_BOUND`).
                mh_bindto_prelink_bound_field(ctx, new_mh);
                // Wave 68 (`CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`): what the
                // line below may remember of that link.
                prelink = bindto_prelink_memo_of(&*ctx, new_mh, carry);
            }
            // MHF-2: this ordinary mint's answers, for the next bind of the
            // same source (`bindto_fast_mint_fill` drops them if a collection
            // ran since `mint_key` was read). Only the unbound direct kinds,
            // with nothing chained: their slow path is the tail above.
            if let (Some(key), Some(recv_class), Some((type_desc, collects))) =
                (mint_key, mint_admits, mint_answer)
            {
                if chain_free
                    && !already_bound
                    && bindto_fast_mint_fills_kind(kind, mh_bindto_fast_mint_fields_enabled())
                {
                    let answer = BindToMintAnswer {
                        kind,
                        type_desc,
                        collects,
                        static_owner: static_owner.map(|o| o.as_u32()),
                        recv_class,
                        field_carry,
                        prelink,
                        alt: None,
                    };
                    bindto_fast_mint_fill(&*ctx, key, answer);
                }
            }
            Ok(Some(Value::Object(Some(new_mh))))
        },
        NativeKind::Intrinsic,
    );
    // asTypeUncached — the JDK's own body, except for a downcall carrier
    // (`mh_as_type_uncached_carrier`). A reviewed `Intrinsic`: it must win over
    // the real bytecode in both modes, and for every other receiver it IS that
    // bytecode.
    r.register_with_kind(
        mh,
        "asTypeUncached",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let new_type = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Some(answer) = mh_as_type_uncached_carrier(ctx, this, new_type) {
                return answer;
            }
            // Interpreter round i3 wave 82, lane L4: the converting adapter
            // memo (`CRATONVM_MH_REAL_AS_TYPE_MEMO`, default off; `None`
            // unless it applies, then the body below runs as before).
            if let Some(answer) = mh_real_as_type_memo(ctx, this, new_type) {
                return answer;
            }
            ctx.invoke_virtual_bytecode_only(
                this,
                "asTypeUncached",
                "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
                &[new_type],
            )
        },
        NativeKind::Intrinsic,
    );
    // asType — type adaptation: return self, but propagate the supplied
    // MethodType into the `type` field so subsequent JDK-internal reads of
    // `mh.type()` / `parameterSlotCount` reflect the adapted signature.
    // invoke()/invokeExact handle the actual argument coercions.
    //
    // G31: and REFUSE the conversions HotSpot refuses, which this body did not
    // do at all. See [`mh_can_convert`] for the transcribed rule and the sweep
    // it was checked against. Two things about the shape of the refusal here:
    //
    // **It is gated on both descriptors, and on a REAL `MethodType` receiver.**
    // The check only runs when the handle's `type` field holds a MethodType
    // this file can turn back into a descriptor and the requested MethodType
    // does too. A handle whose type is the `MH_DESC` fallback is left alone:
    // that descriptor is the raw bytecode signature, not the adapted one, and
    // refusing on it would refuse conversions HotSpot allows.
    //
    // **It never refuses where the divergence is OURS.** HotSpot's `asType`
    // returns a NEW handle and leaves the receiver's `type()` untouched
    // (MEASURED: `identity(int).asType((int)long)` answers `(int)long` while
    // the receiver still answers `(int)int`, and the two are different
    // objects). This body has always had ONE object, so a second `asType` on
    // the same reference sees the FIRST one's adapted type where HotSpot would
    // still see the original. Adding a check on top of that aliasing would
    // manufacture refusals HotSpot never issues, so a conversion that the raw
    // `MH_DESC` signature would have allowed is accepted even when the mutated
    // `type` field forbids it. That is a deliberate UNDER-refusal in exactly
    // the cases the aliasing creates, and it cannot turn an accept into a
    // refusal. Collapsing the aliasing (minting a real second handle) is
    // NOMINATED in G31-1, not done here: it would have to reproduce every
    // synthetic dispatch slot of an arbitrary handle kind, and this lane could
    // not build the VM to find out what that breaks.
    // Reviewed intrinsic: it handles this VM's FFM downcall carrier copy,
    // while `mh_foreign_as_type` delegates every real JDK handle to its
    // concrete `MethodHandle.asType` bytecode. Without this resolution entry,
    // JDK-only runs bytecode against the VM-minted abstract MethodHandle
    // carrier and reaches `rebind().editor()` (which only BoundMethodHandle
    // implements).
    r.register_with_kind(
        mh,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // A Panama downcall's dispatch reads its `FunctionDescriptor`, not
            // its `type` field, so `asType` stays a passthrough for one.
            //
            // The refusal this guard used to be written against is gone: it
            // tested for the invented class `java/lang/foreign/DowncallHandle`
            // and existed because assigning `type` would have overwritten the
            // function address that class kept in field 0. The carrier is now a
            // real `MethodHandle` whose slot 0 IS the real `type` field, so
            // that write would be correct rather than destructive — but the
            // passthrough is still right, and cheaper.
            //
            // Round 15 wave 6 (lane mh5): not for `asType(null)`, which is the
            // NPE below on every handle (the passthrough answered the handle).
            //
            // JIT round 16 wave 11 (lane mhbind): the passthrough answered the
            // receiver for ANY new type, so `abs.asType((int)long)
            // .invokeExact(-6)` was refused with `WrongMethodTypeException`
            // where HotSpot answers 6. With `CRATONVM_MH_DOWNCALL_INVOKE_ADAPT`
            // (default on) a downcall answers an adapted copy
            // (`mh_downcall_as_type`); `0` restores the passthrough.
            if let Some(Value::Object(Some(this))) = args.first() {
                if crate::panama::is_downcall_handle(ctx, *this)
                    && !matches!(args.get(1), Some(Value::Object(None)))
                {
                    if mh_downcall_adapt_enabled() {
                        if let Some(Value::Object(Some(mt))) = args.get(1) {
                            return mh_downcall_as_type(ctx, *this, *mt);
                        }
                    }
                    return Ok(Some(args[0]));
                }
            }
            // `asType(null)`. MEASURED on HotSpot 25.0.3+9, transcribed rather
            // than composed — it is the helpful-NPE text for the first field
            // read `asTypeUncached` performs on `newType`:
            //   java.lang.NullPointerException: Cannot invoke
            //   "java.lang.invoke.MethodType.form()" because "newType" is null
            // This body used to hand the receiver straight back for a null
            // argument, which is the silent-lie shape: a caller asking for an
            // adaptation it did not describe got an unadapted handle.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"java.lang.invoke.MethodType.form()\" because \
                         \"newType\" is null"
                            .to_string(),
                    ),
                }
                .into());
            }
            if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) =
                (args.first(), args.get(1))
            {
                let (this, mt) = (*this, *mt);
                // Wave 54 (lane L4): a real JDK handle adapts through the
                // JDK's own `asType`; this model would stamp it in place.
                if let Some(answer) = mh_foreign_as_type(ctx, this, mt) {
                    return answer;
                }
                // Round 15 wave 6 (lane mh5, MH4-1): a variable-arity
                // receiver answers the JDK's FIXED-arity composition.
                if let Some(answer) = mh_astype_varargs_collector(ctx, this, mt) {
                    return answer;
                }
                if let Some(refusal) = mh_astype_refusal(ctx, this, mt) {
                    return Err(refusal);
                }
                return mh_with_stamped_type(ctx, Value::Object(Some(this)), mt, false);
            }
            Ok(Some(args[0]))
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    // C21: rebind() is JDK-abstract (no Code attribute) — synthetic MHs that
    // do not subclass BoundMethodHandle still get rebind() called by JDK
    // internals (e.g. Invokers, LambdaForm specialization). Return self so the
    // chain continues without "no Code attribute" internal errors.
    //
    // DO NOT RETIRE. Confirmed against all nine supported images: `rebind()` is
    // `abstract` on `java.lang.invoke.MethodHandle` on every one of them, so
    // there is no bytecode to fall back to and the comment above describes the
    // exact failure a retirement would restore. `H25-2` N3.
    r.register(
        mh,
        "rebind",
        "()Ljava/lang/invoke/BoundMethodHandle;",
        |_ctx, args| Ok(Some(args[0])),
    );

    // `copyWith(MethodType, LambdaForm)` is JDK-abstract on `MethodHandle`
    // (the LambdaForm-backed subclasses implement it; CratonVM's synthetic
    // handles are none of them), so, as for `rebind` above, there is no
    // bytecode to shadow: the registration answers where the JDK would throw
    // `AbstractMethodError: ... copyWith ... has no Code attribute`.
    //
    // Its one caller that matters is `MethodHandle.viewAsType` (a new view of
    // the same behaviour under another type, no conversions), which the JDK's
    // `StringConcatFactory.makeConcatWithConstants` / `makeConcat` end with.
    // Those two run their own bytecode whenever they are CALLED rather than
    // linked by `invokedynamic` (the VM links javac's concat sites itself),
    // above all from `ObjectMethods.makeToString`, the `toString` of every
    // `ObjectMethods.bootstrap` site the native linkage hands to the JDK and of
    // every direct `ObjectMethods.bootstrap` call. All of those failed here
    // (interpreter round i1 wave 43, lane L4; probe
    // `tools/probes/interp/L4/L4W43CopyWithViews.java`).
    //
    // The view is `asType`'s pure copy (`mh_with_stamped_type`): the same
    // handle slots, the new `type`. The `LambdaForm` argument is ignored — a
    // synthetic handle has none, and its behaviour lives in its slots.
    r.register(
        mh,
        "copyWith",
        "(Ljava/lang/invoke/MethodType;Ljava/lang/invoke/LambdaForm;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| match (args.first(), args.get(1)) {
            (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) => {
                mh_with_stamped_type(ctx, Value::Object(Some(*this)), *mt, false)
            }
            // `copyWith(null, lf)`: the JDK's species constructors store the
            // type unchecked; no JDK caller passes null. Keep the receiver.
            (Some(this), _) => Ok(Some(*this)),
            (None, _) => Ok(Some(Value::Object(None))),
        },
    );

    // type() — return a MethodType representing this MH's signature.
    // C19: prefer the real-JDK `type` field (populated by C15/C19 at
    // slot 0). Fall back to our synthetic descriptor, then finally
    // synthesize `()V` so callers never observe null.
    r.register(
        mh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") {
                return Ok(Some(Value::Object(Some(mt))));
            }
            let desc = mh_read_desc(ctx, this).unwrap_or_default();
            if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &desc) {
                return Ok(Some(Value::Object(Some(mt))));
            }
            let mt = build_method_type_from_descriptor(ctx, "()V")?;
            Ok(Some(Value::Object(mt)))
        },
    );

    // Lookup.find* are already registered in register_p63_method_handles_lookup
    // with full descriptor resolution. No duplicate registration needed here.

    // MethodType.makeImpl(Class, Class[], boolean): a reviewed `Intrinsic` over
    // concrete JDK bytecode (AGENTS.md), registered ONLY under
    // `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO` (interpreter round i3 wave 74, lane
    // L4; default off), and since wave 75 only in a VM without a JIT
    // (`NativeMethodRegistry::jit_enabled`). A memo hit answers the interned instance the body
    // would; every other call IS the body (`mt_make_impl_intrinsic`). Removal:
    // when the interpreter's own invocation cost makes the body cheap enough
    // (the bench rows `mt-*` of `L4W74FinderMintSplitBench` decide), or never
    // if it is flipped on.
    if mh_method_type_intern_memo_registers(r) {
        r.register_with_kind(
            MT_INTERN_CLASS,
            "makeImpl",
            MT_INTERN_DESC,
            mt_make_impl_intrinsic,
            NativeKind::Intrinsic,
        );
        // Wave 80: a no-op unless `CRATONVM_INTRINSIC_BODY_COMPILES`.
        r.mark_body_compilable_intrinsic_row(
            MT_INTERN_BODY_ROW,
            MT_INTERN_CLASS,
            "makeImpl",
            MT_INTERN_DESC,
        );
    }
    // Interpreter round i6 wave 132, lane L4 (`CRATONVM_MH_INVOKER_CHECKS_INTRINSIC`,
    // default off, no-JIT VMs only): `Invokers.checkCustomized` and
    // `Invokers.checkExactType` as reviewed `Intrinsic`s over JDK bytecode
    // (AGENTS.md); every call they do not answer IS the body
    // (`invokers_check_jdk_body`). See the section comment above
    // `INVOKERS_CLASS`. Removal: as `makeImpl`'s.
    if mh_invoker_checks_registers(r) {
        r.register_with_kind(
            INVOKERS_CLASS,
            "checkCustomized",
            INVOKERS_CHECK_CUSTOMIZED_DESC,
            invokers_check_customized_intrinsic,
            NativeKind::Intrinsic,
        );
        r.register_with_kind(
            INVOKERS_CLASS,
            "checkExactType",
            INVOKERS_CHECK_EXACT_TYPE_DESC,
            invokers_check_exact_type_intrinsic,
            NativeKind::Intrinsic,
        );
        r.mark_body_compilable_intrinsic_row(
            INVOKERS_CHECKS_BODY_ROW,
            INVOKERS_CLASS,
            "checkCustomized",
            INVOKERS_CHECK_CUSTOMIZED_DESC,
        );
        r.mark_body_compilable_intrinsic_row(
            INVOKERS_CHECKS_BODY_ROW,
            INVOKERS_CLASS,
            "checkExactType",
            INVOKERS_CHECK_EXACT_TYPE_DESC,
        );
    }
    // Interpreter round i7 wave 137, lane L4 (`CRATONVM_MH_LF_LEAF_INTRINSICS`,
    // default off, no-JIT VMs only): `MethodHandleImpl.checkSpreadArgument` and
    // `DirectMethodHandle.internalMemberName` as reviewed `Intrinsic`s over JDK
    // bytecode (AGENTS.md); every call they do not answer IS the body
    // (`invoke_special_bytecode_only`). See the section comment above `LF_LEAF_MHI_CLASS`.
    if mh_lf_leaf_registers(r) {
        r.register_with_kind(
            LF_LEAF_MHI_CLASS,
            "checkSpreadArgument",
            LF_LEAF_CHECK_SPREAD_DESC,
            mhi_check_spread_argument_intrinsic,
            NativeKind::Intrinsic,
        );
        r.register_with_kind(
            LF_LEAF_DMH_CLASS,
            "internalMemberName",
            LF_LEAF_INTERNAL_MEMBER_DESC,
            dmh_internal_member_name_intrinsic,
            NativeKind::Intrinsic,
        );
        r.mark_body_compilable_intrinsic_row(
            LF_LEAF_BODY_ROW,
            LF_LEAF_MHI_CLASS,
            "checkSpreadArgument",
            LF_LEAF_CHECK_SPREAD_DESC,
        );
        r.mark_body_compilable_intrinsic_row(
            LF_LEAF_BODY_ROW,
            LF_LEAF_DMH_CLASS,
            "internalMemberName",
            LF_LEAF_INTERNAL_MEMBER_DESC,
        );
    }
    // Interpreter round i6 wave 121, lane L2 (page
    // `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
    // the `Intrinsic`s above whose natives re-enter the JDK's own bytecode
    // (`mh_foreign_as_type` / `mh_real_as_type_bytecode`, `mh_iwa_jdk_body`,
    // `mh_foreign_bind_to`) name those bodies for re-entry-only publication.
    // Moved here from `vm_init.rs`, where wave 117 filed the list. Changes no
    // row's kind; the VM publishes nothing unless
    // `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`.
    r.mark_reentry_only_body(
        mh,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
    );
    r.mark_reentry_only_body(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    );
    r.mark_reentry_only_body(mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;");
    r.mark_reentry_only_body(
        mh,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
    );
    r.set_category(__prev_cat);
}

/// The `Class` mirror for one component of a method descriptor, named as
/// [`descriptor_to_class_name`] spells it (`int`, `java/lang/String`, `[I`, …).
///
/// A `MethodType`'s parameter and return mirrors are the objects
/// `java.lang.invoke` reasons about — `MethodTypeForm.canonicalize` erases a
/// parameter to `Object` only when `!t.isPrimitive()` — so a mirror that
/// misreports its own primitiveness corrupts every `MethodType` derived from
/// this one. Resolve the real class first, and only then fall back to the
/// VM's stand-in mirror factory.
///
/// The fallback is unavoidable: `class_id_by_name` answers `None` both for a
/// name no loader has and for a name SEVERAL loaders define (a Groovy script
/// class under `GroovyClassLoader$InnerLoader` is routinely both), and there
/// is still a descriptor to spell. `class_id_by_name_delegated` is tried in
/// between because it resolves the parent-delegation answer for an
/// ordinary-application lookup where the plain search declines to pick.
pub(crate) fn mirror_for_descriptor_name(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    mirror_for_descriptor_name_resolving(ctx, name, None).0
}

/// What [`mirror_for_descriptor_name_resolving`] answered a component with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DescriptorComponent {
    /// A primitive (or `void`) mirror.
    Primitive,
    /// The mirror of this loaded class (its raw `ClassId`).
    Class(u32),
    /// A stand-in: no lookup found the class.
    StandIn,
}

/// [`mirror_for_descriptor_name`], also telling what the mirror is (JIT round
/// 16 wave 15, lane mhapp): the bound-type table keys on the `ClassId`s, and
/// the builder interns only a type with no stand-in.
pub(crate) fn mirror_for_descriptor_name_resolving(
    ctx: &mut dyn NativeContext,
    name: &str,
    near: Option<ClassId>,
) -> (ObjectRef, DescriptorComponent) {
    // The nine primitive names are not classes; `primitive_class_mirror` is
    // their canonical factory and no lookup can improve on it.
    if is_primitive_descriptor_name(name) {
        return (
            ctx.primitive_class_mirror(name),
            DescriptorComponent::Primitive,
        );
    }
    // A lambda proxy's own name, as `unnameable_mirror_descriptor` renders
    // it (interpreter round i4 wave 92, lane L4): the proxy's mirror, not a
    // stand-in of that name. First, so no loader is asked for a name no
    // loader defines (a `$$Lambda/0x` name is never a loadable binary name).
    if let Some(cid) = lambda_proxy_descriptor_class(&*ctx, name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    // Round 16 wave 17 (mhapp): the class `near`'s defining loader sees under
    // `name`, before the loader-blind lookups (JVMS 5.3: a descriptor's names
    // are resolved by the referencing class's loader).
    if let Some(cid) = near.and_then(|n| mh_caller_sees(&*ctx, name, n)) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    if let Some(cid) = ctx.class_id_by_name(name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    if let Some(cid) = ctx.class_id_by_name_delegated(name) {
        return (
            ctx.get_class_mirror(cid),
            DescriptorComponent::Class(cid.as_u32()),
        );
    }
    // An array descriptor is the one miss worth acting on: `[Ljava/lang/
    // Object;` is very often not yet indexed even though every ingredient
    // for it is, and the stand-in it would otherwise get has no
    // `componentType`, so `Class.getSimpleName()` renders `Object;` and
    // `mt.parameterType(0) == Object[].class` is false — the identity
    // `MethodHandle.asType` and Spring's converter registry both compare on.
    // Load it on demand, exactly as `native_class_array_type` does for
    // `Class.arrayType()`, and only then mint a stand-in.
    if name.starts_with('[') {
        let _ = ctx.load_class(name);
        if let Some(cid) = near
            .and_then(|n| mh_caller_sees(&*ctx, name, n))
            .or_else(|| ctx.class_id_by_name(name))
        {
            return (
                ctx.get_class_mirror(cid),
                DescriptorComponent::Class(cid.as_u32()),
            );
        }
    }
    (
        ctx.primitive_class_mirror(name),
        DescriptorComponent::StandIn,
    )
}

/// `CRATONVM_MH_INTERN_DESCRIPTOR_TYPES` -- default ON (JIT round 16 wave 15,
/// lane mhapp; a correctness fix, both modes): a `MethodType` the VM builds
/// from a descriptor ([`build_method_type_from_descriptor`]: `findVirtual`'s
/// type, `bindTo`'s bound type, ...) is the JDK's INTERNED instance, through
/// `MethodType.methodType(Class, Class[], boolean)` and so `makeImpl`'s
/// `internTable`, as on HotSpot, where two equal types are one object
/// (`R16WmteBindApp`: `same=true`). It was a fresh native carrier per build,
/// so `mh.bindTo(a).type() == mh.bindTo(b).type()` was false for every type
/// the bound-type table does not share. `0`/`false`/`off`/`no` restores the
/// fresh carrier. Read once (a test's thread override is honoured).
pub(crate) fn mh_intern_descriptor_types_enabled() -> bool {
    const KEY: &str = "CRATONVM_MH_INTERN_DESCRIPTOR_TYPES";
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if cratonvm_types::flags::overrides_active() {
        return cratonvm_types::flags::runtime_flag_default_on(KEY);
    }
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(KEY))
}

/// `MethodType.methodType(Class, Class[], boolean)`: the package-private
/// "trusted" factory every public `methodType` overload ends in. It answers
/// the `objectOnlyTypes` cache for an all-`Object` type and `makeImpl`'s
/// interned instance otherwise. Not registered as a native anywhere, so the
/// call reaches the JDK's bytecode.
pub(crate) const MT_TRUSTED_FACTORY: &str =
    "(Ljava/lang/Class;[Ljava/lang/Class;Z)Ljava/lang/invoke/MethodType;";

/// The JDK's interned `MethodType` for the pinned `rtype` / `ptypes` (a fresh
/// `Class[]` the caller built and never writes again, so it is passed as
/// TRUSTED: on an intern miss the JDK keeps it as the new type's `ptypes`).
/// `None` -- the caller keeps its native carrier -- when the image has no
/// such factory (a synthetic JDK), `MethodType` is not initialized yet (a
/// build during its own `<clinit>`, or before it: this never runs a class
/// initializer), or the call does not answer a `MethodType`. Every failure is
/// dropped: the descriptor came from a loaded method, and the carrier is what
/// the build has always answered.
pub(crate) fn intern_descriptor_type_via_jdk(
    ctx: &mut dyn NativeContext,
    rtype_pin: usize,
    rtype: ObjectRef,
    ptypes_pin: usize,
    ptypes: ObjectRef,
) -> Option<ObjectRef> {
    let mt_class = ctx.class_id_by_name(MT_CLASS)?;
    if !ctx.is_class_initialized(mt_class)
        || ctx.is_class_synthetic_stub(MT_CLASS)
        || !ctx.method_exists(MT_CLASS, "methodType", MT_TRUSTED_FACTORY)
    {
        return None;
    }
    let rtype = ctx.read_native_pin(rtype_pin, rtype);
    let ptypes = ctx.read_native_pin(ptypes_pin, ptypes);
    match ctx.invoke(
        MT_CLASS,
        "methodType",
        MT_TRUSTED_FACTORY,
        &[
            Value::Object(Some(rtype)),
            Value::Object(Some(ptypes)),
            Value::Int(1),
        ],
    ) {
        Ok(Some(Value::Object(Some(mt)))) => Some(mt),
        _ => None,
    }
}

/// Build a MethodType object from a JVM method descriptor string.
pub fn build_method_type_from_descriptor(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    Ok(build_method_type_from_descriptor_resolving(ctx, desc, None)?.map(|(mt, _)| mt))
}

/// [`build_method_type_from_descriptor`] resolving every class name through
/// `near`'s defining loader first (round 16 wave 17, lane mhapp); `None` is
/// the loader-blind builder.
pub(crate) fn build_method_type_from_descriptor_near(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    Ok(build_method_type_from_descriptor_resolving(ctx, desc, near)?.map(|(mt, _)| mt))
}

/// [`build_method_type_from_descriptor`], also answering the raw `ClassId`
/// it resolved for every reference component (parameters then the return,
/// primitives skipped), or `None` there when one is a stand-in.
pub(crate) fn build_method_type_from_descriptor_resolving(
    ctx: &mut dyn NativeContext,
    desc: &str,
    near: Option<ClassId>,
) -> Result<Option<(ObjectRef, Option<Vec<u32>>)>, MethodCallFailed> {
    if desc.is_empty() || !desc.starts_with('(') {
        return Ok(None);
    }

    let Some(close) = desc_close_paren(desc) else {
        return Ok(None);
    };
    let params_str = &desc[1..close];
    let ret_str = &desc[close + 1..];

    let param_names = parse_descriptor_types(params_str);
    let ret_name = descriptor_to_class_name(ret_str);

    // Create return type Class mirror
    let (ret_mirror, ret_resolved) = mirror_for_descriptor_name_resolving(ctx, &ret_name, near);
    let mut resolved: Option<Vec<u32>> = Some(Vec::with_capacity(param_names.len() + 1));
    let note = |r: DescriptorComponent, resolved: &mut Option<Vec<u32>>| match r {
        DescriptorComponent::Primitive => {}
        DescriptorComponent::Class(id) => {
            if let Some(ids) = resolved.as_mut() {
                ids.push(id);
            }
        }
        DescriptorComponent::StandIn => *resolved = None,
    };

    // GC-safety: `new_array`/`primitive_class_mirror` (lazily allocates a
    // synthetic mirror on first use, same as `synthetic_class_mirror`)/
    // `alloc_concurrent_synthetic` below can all trigger a collection that
    // relocates `ret_mirror`; pin it and re-read the forwarded reference
    // before each subsequent use.
    let ret_mirror_pin = ctx.pin_native_root(ret_mirror);

    // Create params array: a `Class[]`, `MethodType.ptypes`' declared type.
    // It was an untyped `Object[]` until wave 45 (lane L4), and every JDK
    // `(Class<?>[]) Arrays.copyOf(ptypes, ..)` over such a type failed its
    // `checkcast` (`MethodType.asCollectorType` / `insertParameterTypes`;
    // `L4W44RecordDeserialization`, `L4W44CombinatorHunt` `asCollector-pos`).
    let arr = new_class_array(ctx, param_names.len());
    let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
    // Same GC-safety concern applies to `arr`, written into on every loop
    // iteration after a per-iteration `primitive_class_mirror`/allocating
    // call; pin it too and re-read before each write.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, pname) in param_names.iter().enumerate() {
        let (mirror, r) = mirror_for_descriptor_name_resolving(ctx, pname, near);
        note(r, &mut resolved);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    note(ret_resolved, &mut resolved);

    // JIT round 16 wave 15 (mhapp): the JDK's interned instance, as HotSpot
    // answers (`mh_intern_descriptor_types_enabled`). Never over a stand-in:
    // interning one would freeze it into the JDK's table.
    if resolved.is_some() && mh_intern_descriptor_types_enabled() {
        if let Some(mt) =
            intern_descriptor_type_via_jdk(ctx, ret_mirror_pin, ret_mirror, arr_pin, arr)
        {
            ctx.unpin_native_roots(ret_mirror_pin);
            return Ok(Some((mt, resolved)));
        }
    }

    // JDK MethodType field layout: rtype(0), ptypes(1), form(2), wrapAlt(3),
    // invokers(4), methodDescriptor(5). Allocate 6 slots so the JDK-resolved
    // `form` slot (2) lives within the synthetic object.
    let mt: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6)?;
    let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(ret_mirror_pin);
    ctx.set_field(mt, 0, Value::Object(Some(ret_mirror)));
    ctx.set_field(mt, 1, Value::Object(Some(arr)));

    // gen r4w3/rooting: `populate_method_type_form` refreshes `mt` in place.
    let mut mt = mt;
    populate_method_type_form(ctx, &mut mt)?;
    Ok(Some((mt, resolved)))
}

/// C21: Build a synthetic MethodTypeForm and link it into `mt.form` so
/// `MethodType.parameterSlotCount()` (= `form.parameterSlotCount`) and
/// `MethodType.erasedType()` / `.erase()` (= `form.erasedType()`) do not NPE
/// on the null `form` field. JDK MethodTypeForm field layout:
///   parameterSlotCount: short (slot 0)
///   primitiveCount:     short (slot 1)
///   erasedType:         MethodType (slot 2)
///   basicType:          MethodType (slot 3)
///   methodHandles:      SoftReference[] (slot 4)
///   lambdaForms:        SoftReference[] (slot 5)
///   interpretEntry:     SoftReference (slot 6)
///
/// Reads the `ptypes` array (slot 1) to compute slot/primitive counts and
/// stores `mt` itself as both erasedType and basicType — a minimal stand-in
/// so downstream `form.erasedType()` returns a non-null MethodType.
///
/// Slots 4/5 (`methodHandles`, `lambdaForms`) are the JDK's two lazy caches
/// and MUST be non-null arrays here — see `MTF_LF_CACHE_LEN` below.
///
/// gen r4w3/rooting: `mt` is `&mut` because the form allocation below is a GC
/// point (`try_alloc_concurrent_synthetic` can run `<clinit>`); the caller's
/// copy of `mt` is refreshed to its post-collection address on success.
pub(crate) fn populate_method_type_form(
    ctx: &mut dyn NativeContext,
    mt_ref: &mut cratonvm_types::ObjectRef,
) -> Result<(), MethodCallFailed> {
    let mt = *mt_ref;
    let mut slot_count: i32 = 0;
    let mut primitive_count: i32 = 0;
    if let Value::Object(Some(arr)) = ctx.get_field(mt, 1) {
        let n = ctx.array_length(arr);
        for i in 0..n {
            slot_count += 1;
            if let Value::Object(Some(mirror)) = ctx.get_array_element(arr, i) {
                let nm = crate::lang_class::mirror_class_name(ctx, mirror).unwrap_or_default();
                match nm.as_str() {
                    "long" | "double" => slot_count += 1,
                    "boolean" | "byte" | "char" | "short" | "int" | "float" => {
                        primitive_count += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    // GC-safety: `alloc_concurrent_synthetic` and the two cache-array
    // allocations below can each trigger a collection that relocates `mt` and `form`.
    // Pin both and re-read the forwarded reference before every use.
    let mt_pin = ctx.pin_native_root(mt);
    let form = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodTypeForm", 7)?;
    let form_pin = ctx.pin_native_root(form);
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(form, 0, Value::Int(slot_count));
    ctx.set_field(form, 1, Value::Int(primitive_count));
    ctx.set_field(form, 2, Value::Object(Some(mt)));
    ctx.set_field(form, 3, Value::Object(Some(mt)));
    // Slots 4/5 are the JDK's two lazy caches, `methodHandles` and
    // `lambdaForms`. Leaving them null is a state the real constructor NEVER
    // produces for a form that claims `basicType == erasedType` — which is
    // exactly what the two writes above claim for this form. The real
    // `MethodTypeForm(MethodType)` allocates BOTH arrays on that branch and
    // only leaves them null on the non-basic branch, whose accessors the JDK
    // then never calls (every caller routes through `basicType().form()`).
    //
    // Every JDK reader is a bare indexed load with no null check:
    //   MethodTypeForm.cachedLambdaForm   -> `lambdaForms[which]`
    //   MethodTypeForm.cachedMethodHandle -> `methodHandles[which]`
    // so a null slot is not a cache miss, it is a NullPointerException in
    // real-JDK bytecode we cannot patch. Observed twice:
    //   * `CallSite.makeUninitializedCallSite` NPE'd on the null
    //     `methodHandles` — worked around by shimming that method instead
    //     (see the `makeUninitializedCallSite` registration below).
    //   * `MethodHandle.asVarargsCollector` -> `DelegatingMethodHandle
    //     .makeReinvokerForm` -> `mtype.form().cachedLambdaForm(LF_DELEGATE)`
    //     NPE'd with `Cannot load from object array because "this.lambdaForms"
    //     is null` (regression-suite RJdkHandles, BOTH --real-jdk and
    //     --jdk-only; HotSpot 25 passes).
    // Allocating empty arrays turns those reads back into ordinary cache
    // MISSES (null entry), which is what the JDK expects on a fresh form.
    // Both fields are declared `SoftReference<...>[]` (i6-L2: typed, not
    // `Object[]`).
    let soft_ref = crate::lang_class::reflection_component_id(ctx, "java/lang/ref/SoftReference");
    let method_handles = ctx.new_ref_array(soft_ref, MTF_MH_CACHE_LEN);
    let form = ctx.read_native_pin(form_pin, form);
    ctx.set_field(form, 4, Value::Object(Some(method_handles)));
    let lambda_forms = ctx.new_ref_array(soft_ref, MTF_LF_CACHE_LEN);
    let form = ctx.read_native_pin(form_pin, form);
    ctx.set_field(form, 5, Value::Object(Some(lambda_forms)));
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(mt, 2, Value::Object(Some(form)));
    ctx.unpin_native_roots(mt_pin);
    *mt_ref = mt;
    Ok(())
}

/// Length of the fabricated `MethodTypeForm.methodHandles` cache (slot 4).
/// JDK 25's `MethodTypeForm.MH_LIMIT` is 3.
///
/// Deliberately sized PAST the JDK constant. Both caches are `private` fields
/// touched only by `MethodTypeForm`'s own accessors, and only ever by an
/// indexed load/store — nothing reads `.length`, nothing iterates them — so an
/// over-long array is indistinguishable from an exact one to every reader,
/// while an array sized from a stale constant would be an
/// `ArrayIndexOutOfBoundsException` the day a JDK release adds a cache index.
pub(crate) const MTF_MH_CACHE_LEN: usize = 16;

/// Length of the fabricated `MethodTypeForm.lambdaForms` cache (slot 5).
/// JDK 25's `MethodTypeForm.LF_LIMIT` is 26. Over-sized for the reason given
/// on `MTF_MH_CACHE_LEN`.
pub(crate) const MTF_LF_CACHE_LEN: usize = 64;

/// Parse a sequence of JVM type descriptors from a parameter string.
/// e.g. "ILjava/lang/String;D" → ["int", "java/lang/String", "double"]
///
/// Returns `Cow<'static, str>` so the primitive descriptor branches
/// (which dominate this hot path on the invoke/invokeExact dispatch) avoid
/// allocating a `String` per parameter; only reference / array types
/// produce an owned string.
pub(crate) fn parse_descriptor_types(desc: &str) -> Vec<Cow<'static, str>> {
    let mut result: Vec<Cow<'static, str>> = Vec::new();
    let bytes = desc.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'I' => {
                result.push(Cow::Borrowed(NAME_INT));
                i += 1;
            }
            b'J' => {
                result.push(Cow::Borrowed(NAME_LONG));
                i += 1;
            }
            b'F' => {
                result.push(Cow::Borrowed(NAME_FLOAT));
                i += 1;
            }
            b'D' => {
                result.push(Cow::Borrowed(NAME_DOUBLE));
                i += 1;
            }
            b'Z' => {
                result.push(Cow::Borrowed(NAME_BOOLEAN));
                i += 1;
            }
            b'B' => {
                result.push(Cow::Borrowed(NAME_BYTE));
                i += 1;
            }
            b'C' => {
                result.push(Cow::Borrowed(NAME_CHAR));
                i += 1;
            }
            b'S' => {
                result.push(Cow::Borrowed(NAME_SHORT));
                i += 1;
            }
            b'V' => {
                result.push(Cow::Borrowed(NAME_VOID));
                i += 1;
            }
            b'L' => {
                if let Some(semi) = desc[i..].find(';') {
                    result.push(Cow::Owned(desc[i + 1..i + semi].to_string()));
                    i += semi + 1;
                } else {
                    break;
                }
            }
            b'[' => {
                let start = i;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        if let Some(semi) = desc[i..].find(';') {
                            result.push(Cow::Owned(desc[start..i + semi + 1].to_string()));
                            i += semi + 1;
                        } else {
                            break;
                        }
                    } else {
                        result.push(Cow::Owned(desc[start..=i].to_string()));
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    result
}


/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a filter species' form
/// key decodes to the filtered position ([`mh_filter_transform_position`]).
#[cfg(test)]
mod jr21_mh_filter_transform_key_tests {
    use super::mh_filter_transform_position;

    #[test]
    fn jr21_filter_keys_decode_to_their_position() {
        // TransformKey.of(FILTER_ARG, 1 + 0, L_TYPE): 0x15.
        assert_eq!(mh_filter_transform_position(0x15), Some(Some(0)));
        // FILTER_ARG at LF position 3 (site argument 2), I_TYPE: 0x135.
        assert_eq!(mh_filter_transform_position(0x135), Some(Some(2)));
        // FILTER_RETURN, not constant-zero, I_TYPE: 0x106.
        assert_eq!(mh_filter_transform_position(0x106), Some(None));
        // FILTER_RETURN with constantZero: no filter call.
        assert_eq!(mh_filter_transform_position(0x116), None);
        // FILTER_ARG at LF position 0 (the handle itself), BIND_ARG(1), a
        // fourth byte: none.
        assert_eq!(mh_filter_transform_position(0x05), None);
        assert_eq!(mh_filter_transform_position(0x11), None);
        assert_eq!(mh_filter_transform_position(0x1115), None);
    }
}

/// j27 wave 2 (lane INVOKE; `CRATONVM_MH_BINDTO_REAL_MINT_BATCH`): the batched
/// memo-hit mint stores what the per-field mint stores -- the prototype's `type`
/// and leading bound arguments (a primitive one included), the line's `form`,
/// and the new value last -- and leaves the prototype alone.
#[cfg(test)]
mod j27_invoke_bind_mint_batch_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

    #[test]
    fn j27_the_batched_mint_copies_the_prototype_and_binds_the_new_value_last() {
        let mut ctx = MockNativeContext::new();
        // A `Species_LIL` layout: type 0, form 1, argL0 2, argI1 3, argL2 4.
        let species = ClassId::new(0x7271);
        let other = ClassId::new(0x7272);
        let proto = ctx.alloc_object(species, 5);
        let fresh = ctx.alloc_object(species, 5);
        let [ty, old_form, new_form, target, old_x, new_x] =
            [0; 6].map(|_| ctx.alloc_object(other, 0));
        ctx.set_field(proto, 0, Value::Object(Some(ty)));
        ctx.set_field(proto, 1, Value::Object(Some(old_form)));
        ctx.set_field(proto, 2, Value::Object(Some(target)));
        ctx.set_field(proto, 3, Value::Int(42));
        ctx.set_field(proto, 4, Value::Object(Some(old_x)));
        let mut args = [0usize; REAL_BIND_MAX_ARGS];
        args[..3].copy_from_slice(&[2, 3, 4]);
        let mut desc = [b'L'; REAL_BIND_MAX_ARGS];
        desc[1] = species_letter_descriptor('I').expect("an int letter");
        let slots = RealBindSlots {
            type_slot: 0,
            form_slot: 1,
            args,
            desc,
            count: 3,
        };
        real_bind_mint_batched(&ctx, proto, new_form, fresh, Some(new_x), &slots);
        assert_eq!(ctx.get_field(fresh, 0), Value::Object(Some(ty)));
        assert_eq!(ctx.get_field(fresh, 1), Value::Object(Some(new_form)));
        assert_eq!(ctx.get_field(fresh, 2), Value::Object(Some(target)));
        assert_eq!(ctx.get_field(fresh, 3), Value::Int(42));
        assert_eq!(ctx.get_field(fresh, 4), Value::Object(Some(new_x)));
        // The prototype is read, never written.
        assert_eq!(ctx.get_field(proto, 1), Value::Object(Some(old_form)));
        assert_eq!(ctx.get_field(proto, 4), Value::Object(Some(old_x)));
        // A `null` bound value is stored as `null`.
        let fresh2 = ctx.alloc_object(species, 5);
        real_bind_mint_batched(&ctx, proto, new_form, fresh2, None, &slots);
        assert_eq!(ctx.get_field(fresh2, 4), Value::Object(None));
        assert_eq!(ctx.get_field(fresh2, 2), Value::Object(Some(target)));
    }

    /// `CRATONVM_JIT_MH_BIND_COUNT_RAW`: the compiled bind's raw count answers only
    /// once this thread settled the slots AND a default threshold for this VM, and
    /// its step is `lane_customization`'s `Count` arm (due at the threshold).
    #[test]
    fn j27_the_raw_count_needs_both_settled_facts_and_stops_at_the_threshold() {
        use cratonvm_types::flags::with_thread_overrides;
        let vm = 0x7273_0001;
        let resolved = LaneCustomizeSlots { form: 1, count: 4, customized: 2 };
        let on: [(&str, Option<&str>); 2] = [
            ("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", None),
            ("CRATONVM_JIT_MH_BIND_COUNT_RAW", None),
        ];
        with_thread_overrides(&on, || {
            LANE_CUSTOMIZE_SLOTS.with(|c| c.set((0, None)));
            CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.set((0, 0)));
            assert_eq!(lane_raw_count_slots(vm), None, "nothing settled");
            LANE_CUSTOMIZE_SLOTS.with(|c| c.set((vm, Some(resolved))));
            assert_eq!(lane_raw_count_slots(vm), None, "the threshold is not settled");
            CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.set((vm, 64)));
            assert_eq!(lane_raw_count_slots(vm), None, "a non-default threshold");
            CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.set((vm, MH_CUSTOMIZE_THRESHOLD)));
            assert_eq!(
                lane_raw_count_slots(vm),
                Some(LaneRawCountSlots { form: 1, count: 4, customized: 2, widest: MH_BOUND })
            );
            assert_eq!(lane_raw_count_slots(vm + 8), None, "another VM's facts");
        });
        with_thread_overrides(&[("CRATONVM_JIT_MH_BIND_COUNT_RAW", Some("0"))], || {
            assert_eq!(lane_raw_count_slots(vm), None, "the kill switch");
        });
        with_thread_overrides(&[("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", Some("0"))], || {
            assert_eq!(lane_raw_count_slots(vm), None, "the uncounted lanes");
        });
        LANE_CUSTOMIZE_SLOTS.with(|c| c.set((0, None)));
        CUSTOMIZE_THRESHOLD_SETTLED.with(|c| c.set((0, 0)));
        assert_eq!(lane_raw_count_step(Value::Int(0)), Some(1));
        assert_eq!(lane_raw_count_step(Value::Int(MH_CUSTOMIZE_THRESHOLD - 1)), Some(MH_CUSTOMIZE_THRESHOLD));
        assert_eq!(lane_raw_count_step(Value::Int(MH_CUSTOMIZE_THRESHOLD)), None, "due");
        assert_eq!(lane_raw_count_step(Value::Object(None)), None, "unreadable");
    }

    #[test]
    fn j27_species_letters_map_to_their_field_descriptors() {
        let letters: Vec<Option<u8>> = "LIJFDX".chars().map(species_letter_descriptor).collect();
        assert_eq!(
            letters,
            vec![Some(b'L'), Some(b'I'), Some(b'J'), Some(b'F'), Some(b'D'), None]
        );
    }
}

#[cfg(test)]
mod j27_mheditor17_real_jdk_handle_tests {
    //! JIT round j27 wave 4, lane MHROWS2 (MHEDITOR17-1).
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    #[test]
    fn a_jdk_shaped_handle_is_real_and_a_kinded_carrier_is_not() {
        let mut ctx = MockNativeContext::new();
        let class = ClassId::new(0x7281);
        // A JDK `DirectMethodHandle`'s width: far below the carrier window.
        let jdk = ctx.alloc_object(class, 6);
        assert!(mh_is_real_handle(&ctx, jdk));
        assert!(mh_is_real_jdk_handle(&ctx, jdk));
        let carrier = ctx.alloc_object(class, MH_BOUND + 1);
        ctx.set_field(carrier, MH_KIND, Value::Int(MH_KIND_STATIC));
        assert!(!mh_is_real_handle(&ctx, carrier));
        assert!(!mh_is_real_jdk_handle(&ctx, carrier));
        // A wide object whose `MH_KIND` slot holds no kind is no carrier. The
        // mock fills fresh slots with `Int(0)` -- which IS a kind
        // (`MH_KIND_STATIC`) -- so the slot is set to the VM's reference
        // default, `null`, explicitly.
        let wide = ctx.alloc_object(class, MH_BOUND + 1);
        ctx.set_field(wide, MH_KIND, Value::Object(None));
        assert!(mh_is_real_jdk_handle(&ctx, wide));
    }
}
