#[allow(unused_imports)]
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
pub fn mh_direct_lane_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// `CRATONVM_MH_DIRECT_LANE_RAW_RETURN` -- default ON (round 13 wave 10, lane
/// callcost4); `0`/`false`/`off`/`no` makes the direct lane box a primitive
/// result as the ordinary doors do. With it on, a primitive of the call
/// site's own return type is handed back raw: every by-name route that reaches
/// these natives (the interpreter's stackless door, `invoke_or_native`'s
/// signature-polymorphic block, the JIT's native site cache) passes the result
/// through `unbox_poly_return_checked`, which keeps a raw primitive as is and
/// unboxes the box the door would have made to the same `Value`. Read once.
pub fn mh_direct_lane_raw_return_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_MH_DIRECT_LANE_RECORD` -- default ON (round 15 wave 1, lane mh;
/// proposal C14-1 of `jit-r14-calls-proposals-RETIRED-20260930.md`); `0`/`false`/`off`/`no`
/// makes [`mh_direct_lane_shape`] decode the handle on every call again
/// (three Java `String` decodes, the descriptor split and the declared `type`
/// render), and the `invoke` door's refusals re-read `MH_DESC` / `MH_CLASS`.
/// With it on, a per-thread memo ([`DIRECT_LANE_MEMO`]) serves the decoded
/// [`DirectLaneRecord`]. Performance only, both modes. Read once.
pub fn mh_direct_lane_record_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
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
pub fn mh_direct_lane_lean_virtual_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
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
pub fn mh_direct_lane_invoke_refusal_skip_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        invoke_declared_cast_enabled()
    })
}

/// What the direct lane derives from a handle's `MH_CLASS`, `MH_NAME`,
/// `MH_DESC` and `type` objects (round 15 wave 1, lane mh; proposal C14-1).
/// A pure function of those four objects (and of whether the handle is an
/// UNBOUND virtual one): Java `String`s are immutable, and a `MethodType`'s
/// `rtype` / `ptypes` are never rewritten after it is built.
pub struct DirectLaneRecord {
    pub class: String,
    pub name: String,
    pub desc: String,
    pub params: Vec<String>,
    /// The handle's own member type: `desc`, with `L<class>;` prepended for an
    /// UNBOUND virtual handle (the caller supplies its receiver).
    pub own_type: String,
    /// The `type` field, rendered (`methodtype_to_descriptor`), when it reads
    /// (round 16, lane mh: the converting lane, [`mh_converting_lane`], hands
    /// it to the door's own refusals and conversions).
    pub declared: Option<String>,
    /// `mh_declared_descriptor` (the `type` field, rendered) is `own_type`.
    pub declared_is_own_type: bool,
    /// `own_type` splits into exactly `L<class>;` (an unbound virtual handle
    /// only) followed by `params`, so a call site that IS `own_type` names
    /// every receiver and parameter position with the very token the
    /// `invoke` door's refusal judges it against (round 15 wave 4, lane mh2;
    /// MH15-1, [`mh_direct_lane_invoke_refusal_skip_enabled`]). A class name
    /// no JVM mints (one holding `;`) misaligns the split and reads `false`.
    pub own_type_aligned: bool,
}

/// Decode a [`DirectLaneRecord`] from the four field values (as read by
/// `get_field(MH_CLASS / MH_NAME / MH_DESC)` and `get_field_by_name("type")`),
/// or `None` for a handle the lane does not model. With `site`, a site that is
/// not the own type declines before the declared type is rendered (the
/// round-13 order, kept for the switched-off path). Pure reads.
pub fn mh_direct_lane_decode(
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
pub struct DirectLaneMemoLine {
    pub vm: usize,
    pub epoch: u64,
    pub refs: [usize; 4],
    pub unbound_virtual: bool,
    pub record: Option<std::rc::Rc<DirectLaneRecord>>,
}

impl DirectLaneMemoLine {
    /// A line no lookup matches (`vm` 0 is never memoised).
    pub fn empty() -> Self {
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
pub const DIRECT_LANE_MEMO_SLOTS: usize = 16;

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
    pub static DIRECT_LANE_MEMO: std::cell::RefCell<Vec<DirectLaneMemoLine>> =
        std::cell::RefCell::new(
            (0..DIRECT_LANE_MEMO_SLOTS).map(|_| DirectLaneMemoLine::empty()).collect(),
        );
}

/// The [`DirectLaneRecord`] of `this`, from [`DIRECT_LANE_MEMO`] or decoded
/// (and memoised). Pure reads; no GC point.
pub fn mh_direct_lane_record_memoised(
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
pub enum DirectLaneDoor {
    InvokeExact,
    Invoke,
}

/// What [`mh_direct_lane_shape`] read off an admitted handle.
pub struct DirectLaneShape {
    pub kind: i32,
    pub bound: Value,
    /// `class`, `name`, `desc` and `params` (round 15 wave 1: memoised,
    /// [`mh_direct_lane_record_enabled`]).
    pub record: std::rc::Rc<DirectLaneRecord>,
}

/// [`MH_EXPLICIT_SCOPE`] cleared for the extent of a direct-lane dispatch and
/// restored on every exit, as [`mh_dispatch_from_door`] does for the ordinary
/// one.
pub struct ExplicitScopeCleared(pub bool);

impl ExplicitScopeCleared {
    pub fn new() -> Self {
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
pub fn mh_direct_lane_shape(
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
pub fn mh_direct_lane(
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
pub fn mh_direct_lane_return(
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
pub fn mh_converting_lane_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The admission of [`mh_converting_lane`]: pure reads, no allocation, no GC
/// point. `Some((record, declared, bound))` when the call is one it models.
pub fn mh_converting_lane_shape(
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
pub fn mh_converting_static_dispatch(
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
pub fn mh_converting_lane(
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
pub struct ConvertingCall<'a> {
    pub this_pin: usize,
    pub this: ObjectRef,
    pub record: &'a DirectLaneRecord,
    pub declared: &'a str,
    pub bound: Value,
    pub extra: &'a [Value],
    pub site: &'a str,
}

/// `mh_invoke_exact_door`'s steps, in order, for an admitted call.
pub fn mh_converting_invoke_exact(
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
pub fn mh_converting_invoke(ctx: &mut dyn NativeContext, call: &ConvertingCall) -> MethodCallResult {
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
        true
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
        true
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
pub fn mh_compiled_bind_target_with(
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
pub fn mh_real_compiled_bind(
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
pub fn mh_real_direct_kind(target: &RealDirectTarget) -> Option<(i32, String)> {
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
pub fn mh_real_lane_record(target: &RealDirectTarget, own_type: String) -> DirectLaneRecord {
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
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// [`mh_compiled_bind_target`] also admits a REAL `bindTo` result
/// ([`mh_real_bound_compiled_bind`]). Read on the dispatch helper's cold fill
/// only.
pub fn mh_constant_bind_real_bound_enabled() -> bool {
    true
}

/// The species `bindTo` of a plain direct handle answers.

/// `MethodTypeForm.LF_REBIND`: the `lambdaForms` index of the
/// `BoundMethodHandle` reinvoker form (`BoundMethodHandle.makeReinvoker`).
pub const MTF_LF_REBIND: usize = 7;

/// `LambdaFormEditor.TransformKey.of(BIND_ARG, 1)`'s packed bytes: `BIND_ARG`
/// (`1`) in the low nibble, the position (`1`) in the next one.
pub const LFE_BIND_ARG_1_PACKED: i64 = 0x11;

/// The most `Transform[]` entries [`mh_real_is_bind_to_form`] scans (the JDK's
/// array cache holds at most 16 before it becomes a `ConcurrentHashMap`).
pub const LFE_TRANSFORM_SCAN_MAX: usize = 64;

/// Is `form` (uncustomized) the form `bindTo` gives a rebind of the direct
/// handle `target`: the `BIND_ARG` position-1 transform cached on the
/// reinvoker form of `target`'s basic type
/// (`target.type.form.basicType.form.lambdaForms[LF_REBIND]`)? Pure reads. Any
/// shape this does not recognise (a cleared soft reference, a
/// `ConcurrentHashMap` cache, a `fullBytes` key) answers `false`: the site keeps
/// its link chain.
pub fn mh_real_is_bind_to_form(ctx: &dyn NativeContext, target: ObjectRef, form: ObjectRef) -> bool {
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
pub fn mh_real_obj(v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The `BoundMethodHandle` reinvoker form cached for `target`'s basic type
/// (`target.type.form.basicType.form.lambdaForms[LF_REBIND]`'s referent), or
/// `None`. Pure reads.
pub fn mh_real_rebind_form(ctx: &dyn NativeContext, target: ObjectRef) -> Option<ObjectRef> {
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
pub fn mh_real_bound_parts(
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
    mh_real_is_bind_to_form(ctx, target, form).then_some((target, receiver))}

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
pub fn mh_real_bound_compiled_bind(
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
    true
}

/// What a REAL `bindTo` result's direct handle binds as (j27 wave 4, lane
/// MHROWS; [`mh_real_bound_member_kind`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MhRealBoundMember {
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
pub fn mh_real_bound_member_kind(
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
pub fn mh_real_bound_leading_value_ok(ctx: &dyn NativeContext, value: ObjectRef, leading: &str) -> bool {
    match ctx.classify_class_name(leading) {
        cratonvm_native_api::NameLookup::Unique(cid) => {
            !ctx.object_is_array(value) && ctx.is_subclass(ctx.class_id_of_object(value), cid)
        }
        _ => false,
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
pub const LFE_FILTER_ARG: i64 = 5;

/// `LambdaFormEditor.FILTER_RETURN`.
pub const LFE_FILTER_RETURN: i64 = 6;

/// The packed `TransformKey` bytes of the transform cached on `target`'s
/// reinvoker form ([`mh_real_rebind_form`]) whose result is `form`
/// (uncustomized), or `None`: [`mh_real_is_bind_to_form`]'s scan, answering
/// the key instead of testing one. A `fullBytes` key (`packedBytes == 0`), a
/// cleared soft reference or a `ConcurrentHashMap` cache answers `None`. Pure
/// reads.
pub fn mh_real_transform_key(
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
pub fn mh_filter_transform_position(key: i64) -> Option<Option<u16>> {
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
pub fn mh_real_member_compiled_bind(
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

/// mh21 (the census step of MHBIND21-3, MHFILTER21-3 and MHFILTER21-4, which
/// every admission above skipped): the diagnostic line for a REAL
/// `BoundMethodHandle` species that no compiled-bind admission took, read only
/// under `CRATONVM_DBG_JIT_MH_BIND=1` (the dispatch helper's cold fill). It names
/// the species class (`Species_LI`, `Species_LL`, ...), the class of its `argL0`
/// and, when `argL0` is a real handle and the (uncustomized) form is a transform
/// cached on its reinvoker form, the transform key's raw packed bytes and its
/// kind nibble (`1` `BIND_ARG`, `5` `FILTER_ARG`, `6` `FILTER_RETURN`, ...
/// `LambdaFormEditor`'s numbering), e.g.
/// `species=Species_LI argL0=java/lang/invoke/DirectMethodHandle key=0x211 kind=1`.
/// `None` for a handle that is no species. Pure reads; no allocation but the text.
pub fn mh_real_species_census_line(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
) -> Option<String> {
    if !mh_is_real_handle(&*ctx, handle) {
        return None;
    }
    let class = ctx.class_name_arc_of_id(ctx.class_id_of_object(handle))?;
    let species = class.strip_prefix(BMH_SPECIES_PREFIX)?.to_string();
    let target = mh_real_obj(ctx.get_field_by_name(handle, "argL0"));
    let target_class = target
        .and_then(|t| ctx.class_name_arc_of_id(ctx.class_id_of_object(t)))
        .map_or_else(|| "null".to_string(), |n| n.to_string());
    let form = mh_real_obj(ctx.get_field_by_name(handle, "form"));
    let key = match (target, form) {
        (Some(t), Some(f)) if mh_is_real_handle(&*ctx, t) => mh_real_transform_key(&*ctx, t, f),
        _ => None,
    };
    Some(match key {
        Some(k) => format!(
            "species=Species_{species} argL0={target_class} key={k:#x} kind={}",
            k & 0xF
        ),
        None => format!("species=Species_{species} argL0={target_class} key=none"),
    })
}

/// The class a method descriptor's return type names (through any array
/// dimensions), or `None` for a primitive / `void` return or a malformed
/// descriptor. JIT round 20, lane dispatch: [`mh_compiled_bind_member`]'s
/// return-type screen.
pub fn mh_bind_return_class_name(desc: &str) -> Option<&str> {
    let ret = &desc[desc.rfind(')')? + 1..];
    ret.trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .filter(|s| !s.is_empty())
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
    true
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
pub fn mh_compiled_bind_member(
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
    true
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
pub fn mh_compiled_bind_interface_member(
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
pub const MH_BIND_CONVERSION_MAX_WORDS: usize = 16;

/// Is `token` a reference type an `Integer` value converts from into `int`,
/// and an `int` boxes into, as one plain unbox / `Integer.valueOf`?
pub fn mh_bind_int_wrapper_token(token: &str) -> bool {
    matches!(
        token,
        "Ljava/lang/Integer;" | "Ljava/lang/Number;" | "Ljava/lang/Object;"
    )
}

/// The conversions from the call's type `from` into the member's own type
/// `to` (the two [`MhBindConversion`] fields without the class id); `None`
/// when some position needs anything outside the closed set, or when nothing
/// converts (the exact bind's case).
pub fn mh_bind_conversions(from: &str, to: &str) -> Option<(u32, bool)> {
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
        false    } else if to_ret == "I" && mh_bind_int_wrapper_token(&from_ret) {
        true    } else {
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
// `Field.getInt` native it had to replace (`SbrW5ReflAccBench.java`,
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
// `--real-jdk`, and `--compatible` under `CRATONVM_MH_FIELD_LINK_COMPATIBLE`
// (interpreter round i2 wave 61, lane L4; [`mh_field_link_mode_admits`]):
// `--compatible` answers `Field`'s accessors with natives, but every
// framework accessor over a field `MethodHandle` reaches this door there too.
// Kill switch `CRATONVM_MH_FIELD_LINK=0` (default on).

/// Packed link state (an `Int`): bits 0-15 the field slot, 16-21 the index
/// of `MethodHandle.type`, then the `FL_*` flags. `0` = not linked.
/// Width-guarded like [`MH_VARARGS`]: only [`alloc_method_handle`] mints the
/// link slots, and a narrower handle is never linked.
pub const MH_FIELD_LINK: usize = MH_BASE + 10;
/// The `MethodType` the link was made under. A linked call requires the
/// handle's `type` to still be this object, so an in-place restamp of `type`
/// (whatever writes it) unlinks the handle rather than inheriting a link made
/// for another signature.
pub const MH_FIELD_LINK_TYPE: usize = MH_BASE + 11;
/// Instance: the receiver class id the door admitted when the link was made
/// (the receiver must be exactly that class and not an array). Static: the
/// initialized class whose static slot is read or written.
pub const MH_FIELD_LINK_CLASS: usize = MH_BASE + 12;
/// A reference setter whose leaf value type is not `Object`: the value class
/// id the door admitted (a value must be null or exactly that non-array class).
pub const MH_FIELD_LINK_VALUE: usize = MH_BASE + 13;
/// The declared descriptor, as a [`field_link_site_id`] id.
pub const MH_FIELD_LINK_SITE: usize = MH_BASE + 14;
/// Slots [`alloc_method_handle`] allocates. Still below
/// `panama::DOWNCALL_BASE`, so [`crate::panama::is_downcall_handle`]'s width
/// screen keeps rejecting every handle minted here.
pub const MH_SLOT_COUNT: usize = MH_FIELD_LINK_SITE + 1;

pub const FL_LINKED: i32 = 1 << 24;
pub const FL_STATIC: i32 = 1 << 25;
pub const FL_SETTER: i32 = 1 << 26;
pub const FL_VALUE_CHECK: i32 = 1 << 27;
/// Bits 28-30: how many times this handle has been (re)linked. A handle whose
/// receiver class keeps changing stops relinking after [`FL_MAX_LINKS`] and is
/// served by the door, so a megamorphic accessor costs the door, not the door
/// plus a relink per call.
pub const FL_LINKS_SHIFT: u32 = 28;
pub const FL_MAX_LINKS: i32 = 7;
/// Bit 31: bits 0-15 are the instance slot the handle carries FROM MINT
/// (interpreter round i2 wave 64, lane L4; `CRATONVM_MH_FIELD_SLOT_FROM_MINT`,
/// [`mh_record_minted_field_slot`]), with or without `FL_LINKED`.
pub const FL_MINT_SLOT: i32 = i32::MIN;
/// Bit 23 (interpreter round i2 wave 65, lane L4;
/// `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`): an INSTANCE link whose
/// [`MH_FIELD_LINK_CLASS`] is the handle's recorded OWNER, admitting a
/// receiver of the owner or of any subclass of it ([`field_link_receiver_admitted`]).
/// Taken from the type index, which was therefore 7 bits (6 since wave 66, [`FL_BOUND`]) ([`FL_TYPE_INDEX_MASK`]):
/// `MethodHandle.type`'s index is a small constant (one of the few instance
/// fields `MethodHandle` itself declares, below the synthetic window at
/// [`MH_BASE`]), so no link ever had a larger one.
pub const FL_OWNER_ADMITS: i32 = 1 << 23;
/// Bit 22 (interpreter round i2 wave 66, lane L4; `CRATONVM_MH_FIELD_LINK_BOUND`):
/// a link of a BOUND instance getter / setter ([`mh_field_link_try_bound`]).
/// The receiver is `MH_BOUND`'s object, never an argument: a getter's call
/// passes nothing and a setter's the value alone. [`MH_FIELD_LINK_CLASS`] is
/// the bound receiver's exact class, re-checked per call. Taken from the type
/// index, as [`FL_OWNER_ADMITS`] was (now 6 bits; a larger index never links).
pub const FL_BOUND: i32 = 1 << 22;
/// Bit 21 (interpreter round i3 wave 79, lane L4; page
/// `i71-L4-field-setter-handle-on-a-volatile-field-stores-without-the-volatile-fence`):
/// the linked field is `volatile` ([`field_link_field_is_volatile`], judged once
/// by the derivation). The fast path then stores as `VarHandle.setVolatile`
/// does ([`field_handle_volatile_store`]: a SeqCst fence on each side) and
/// follows a load with an acquire fence, as HotSpot's accessor form for a
/// volatile field (`putXxxVolatile` / `getXxxVolatile`) does. Taken from the
/// type index, as [`FL_BOUND`] was (now 5 bits; [`MH_BASE`] is 16, and
/// `MethodHandle.type` is declared below it).
pub const FL_VOLATILE: i32 = 1 << 21;
/// Bits 16-20: the index of `MethodHandle.type` (after `>> 16`).
pub const FL_TYPE_INDEX_MASK: i32 = 0x1F;

/// `ACC_VOLATILE` (JVMS 4.5), as `MemberName.flags` and a field's access flags
/// carry it.
pub const FIELD_ACC_VOLATILE: i32 = 0x0040;

/// A field handle's store of a `volatile` field (interpreter round i3 wave 79,
/// lane L4): `store` between two SeqCst fences, [`varhandle_set_volatile`]'s
/// rule (the trailing fence is the StoreLoad barrier a Dekker handshake through
/// two setter handles relies on); a plain store otherwise. Every field handle
/// path that stores -- the door's `SETTER` arm, the field-accessor link
/// ([`mh_field_link_fast`], [`mh_field_link_serve_written`]) and the real
/// accessor ([`mh_real_field_access_resolved`]) -- stores through this.
#[inline]
pub fn field_handle_volatile_store<R>(volatile: bool, store: impl FnOnce() -> R) -> R {
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
pub fn field_handle_volatile_load_fence(volatile: bool) {
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
pub fn field_link_field_is_volatile(
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
pub fn mh_field_link_owner_admits_enabled() -> bool {
    crate::nbflags().mh_field_link_owner_admits
}

/// `CRATONVM_MH_FIELD_LINK_BOUND` (default on since the wave-66 triage, performance; wave 66, lane L4;
/// proposal `i65-L4-proposal-bound-field-handles-link-too`): link a bound
/// instance field getter / setter too ([`mh_field_link_try_bound`]), and mint
/// `bindTo`'s copy of an owned field handle wide enough to hold the link.
pub fn mh_field_link_bound_enabled() -> bool {
    crate::nbflags().mh_field_link_bound
}

/// Does `bindTo`'s copy carrying `carry` need [`alloc_method_handle`]'s width?
/// For a minted slot (wave 65), and under `CRATONVM_MH_FIELD_LINK_BOUND` for
/// any owned field copy, so the bound link has its slots (`bound_link`).
pub fn bound_field_copy_needs_link_slots(carry: Option<BoundFieldCarry>, bound_link: bool) -> bool {
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
pub fn bound_field_link_value_type<'a>(
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
pub fn field_link_receiver_admitted(
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
pub fn mh_field_link_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// Whether this VM's mode links field handles: `--real-jdk` always;
/// `--compatible` with `CRATONVM_MH_FIELD_LINK_COMPATIBLE` (interpreter round
/// i2 wave 61, lane L4; proposal
/// `interpreter-L4-proposal-link-field-handles-under-compatible-too-FIXED-20261025`). The link never
/// changes an answer (see the section comment), and since wave 60's
/// `CRATONVM_MH_COMPATIBLE_RECORD_OWNER` a `--compatible` static handle
/// records its owner, which the static arms require; a handle minted without
/// one is still left to the door.
pub fn mh_field_link_mode_admits(ctx: &dyn NativeContext) -> bool {
    // `JdkInvoke` (nio21 wave 12, L3): the row takes `--real-jdk`'s arm.
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
        || crate::nbflags().mh_field_link_compatible
}

/// Declared descriptors a link compares a call site against, interned so a
/// linked call compares one string without rendering the `MethodType`. A
/// content-addressed cache of descriptor TEXT, not compatibility state: the
/// same text has the same id in every VM of the process, and an id is only
/// ever compared with the text it was issued for.
#[derive(Default)]
pub struct FieldLinkSites {
    pub ids: rustc_hash::FxHashMap<Arc<str>, u32>,
    pub names: Vec<Arc<str>>,
}

/// Bound on [`FieldLinkSites`]: past it nothing new is linked (the door
/// serves those handles), so a program minting descriptors without end cannot
/// grow the table without end.
pub const FIELD_LINK_SITES_MAX: usize = 1 << 16;

pub fn field_link_sites() -> &'static parking_lot::RwLock<FieldLinkSites> {
    static SITES: std::sync::OnceLock<parking_lot::RwLock<FieldLinkSites>> =
        std::sync::OnceLock::new();
    SITES.get_or_init(Default::default)
}

/// The id of `desc` in [`FieldLinkSites`], interning it; `None` when full.
pub fn field_link_site_id(desc: &str) -> Option<u32> {
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
pub const FIELD_LINK_SITE_TEXT_LINES: usize = 64;

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
    pub static FIELD_LINK_SITE_TEXTS: std::cell::RefCell<Vec<Option<(u32, Arc<str>)>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`field_link_site_matches`] under `CRATONVM_MH_FIELD_LINK_SITE_BY_ARC`:
/// answered from this thread's copy of the interned text when it holds `id`,
/// else from the table (under its read lock, once) and copied.
pub fn field_link_site_matches_local(id: usize, site: &str) -> bool {
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


/// Is `site` exactly the descriptor interned as `id`?
pub fn field_link_site_matches(id: i32, site: &str) -> bool {
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
pub fn mh_field_link_fast(
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
pub struct FieldLinkCandidate {
    /// A `SETTER` (else a `GETTER`).
    pub setter: bool,
    /// The packed [`MH_FIELD_LINK`] state as it stands (`0` when none).
    pub old: i32,
    /// `old`'s relink count, below [`FL_MAX_LINKS`].
    pub links: i32,
}

/// The checks every field-accessor link runs before it looks at a call (the
/// switches and this VM's mode, the width, the kind, the relink bound, no
/// marking): shared by [`mh_field_link_try`] and `bindTo`'s pre-link
/// ([`mh_bindto_prelink_bound_field`], wave 67), so the two cannot drift.
/// Reads only.
pub fn field_link_candidate(
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
pub enum BoundLinkCall<'a> {
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
pub fn mh_field_link_serves_invoke_enabled() -> bool {
    crate::nbflags().mh_field_link_serves_invoke
}

/// Is `site` exactly the declared type linked handle `this` interned
/// ([`MH_FIELD_LINK_SITE`])? `false` for an unlinked or narrow handle, or no
/// site. Reads only.
pub fn field_link_site_is(ctx: &dyn NativeContext, this: ObjectRef, site: Option<&str>) -> bool {
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
pub fn mh_field_link_fast_for_invoke(
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
    // `CRATONVM_MH_FIELD_LINK_FIRST`: `(vm, class, Some(()))` for the
    // synthetic `java/lang/invoke/MethodHandle` carrier class, `None` for any
    // other handle class this thread asked about (a few real species), so
    // neither answer takes the class-manager lock twice. The class is the
    // VM's own `MethodHandle` (never unloaded while the VM lives); the key
    // carries the VM.
    // addr-keyed: NOT an object address -- the `usize` is the VM identity, beside a class id; bounded to 8 ways.
    pub static FIELD_LINK_FIRST_CARRIER_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<()>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Is `this` an instance of the synthetic carrier class itself (the VM's
/// `java/lang/invoke/MethodHandle`, which is abstract in the JDK, so no real
/// handle is a direct instance of it)? Memoised per thread by class id
/// ([`FIELD_LINK_FIRST_CARRIER_MEMO`]). Reads only.
pub fn mh_is_synthetic_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
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
pub fn mh_field_link_first(
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
pub fn mh_invoke_links_field_enabled() -> bool {
    crate::nbflags().mh_invoke_links_field
}

/// Is the `invoke` call site `site` one at which `invoke` IS `invokeExact`
/// for a handle declared `declared`: a non-empty descriptor equal to the
/// declared type? An unknown declared type, or no site, is not. Pure.
pub fn invoke_site_is_declared(site: Option<&str>, declared: Option<&str>) -> bool {
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
pub fn mh_declared_type_is_site(ctx: &mut dyn NativeContext, this: ObjectRef, site: &str) -> bool {
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
pub fn mh_field_link_try_for_invoke(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
) {
    mh_field_link_try_for_invoke_noting(ctx, this, extra, call_site, &mut None);
}

/// [`mh_field_link_try_for_invoke`], noting a written link in `note` (wave 71,
/// [`FieldLinkWritten`]).
pub fn mh_field_link_try_for_invoke_noting(
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
pub fn mh_invoke_link_declared_once_enabled() -> bool {
    crate::nbflags().mh_invoke_link_declared_once
}

/// [`mh_field_link_try_for_invoke`] with `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`
/// as `once` (the unit tests pass it). Off, the link after the declared-type
/// comparison is wave 68's: [`field_link_candidate`] and the declared type
/// looked up again inside the attempt.
pub fn mh_field_link_try_for_invoke_with(
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
pub fn mh_linking_call_served_enabled() -> bool {
    crate::nbflags().mh_linking_call_served
}

/// The packed [`MH_FIELD_LINK`] word of `this` (`0` for a narrow handle or
/// none). Reads only.
pub fn field_link_state_word(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
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
pub fn field_link_was_written(before: i32, after: i32) -> bool {
    after & FL_LINKED != 0 && after != before
}

/// Which door a linking call came through (wave 69,
/// [`mh_field_link_try_and_serve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkingDoor {
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
pub fn mh_field_link_serve_linking_call_with(
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
pub fn mh_field_link_try_and_serve(
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
pub fn mh_linking_call_direct_serve_enabled() -> bool {
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
pub struct FieldLinkWritten {
    /// The packed [`MH_FIELD_LINK`] word written (slot, kind bits).
    pub state: i32,
    /// [`MH_FIELD_LINK_CLASS`]: the static owner, the receiver class, or
    /// (with [`FL_OWNER_ADMITS`]) the field's owner.
    pub class_raw: i32,
    /// The receiver the link admitted; `None` for a static link.
    pub receiver: Option<ObjectRef>,
    /// The receiver's exact class (`0` for a static link).
    pub recv_raw: i32,
    /// The call site was compared EQUAL to the declared type the link interned.
    pub site_is_declared: bool,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type, asked when the
    /// site was not judged by the writer.
    pub site_id: i32,
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
pub fn mh_field_link_serve_written(
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
pub fn mh_field_link_derivation_memo_enabled() -> bool {
    crate::nbflags().mh_field_link_derivation_memo
}

/// Lines in [`FIELD_LINK_MEMO`]; a power of two (the index is a mask).
pub const FIELD_LINK_MEMO_SLOTS: usize = 16;

/// The handle slots a memo key reads as words (then `MH_STATIC_OWNER` and
/// `MH_BOUND`'s shape).
pub const FIELD_LINK_MEMO_GUARD_SLOTS: [usize; 6] = [
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
pub struct FieldLinkMemoKey {
    pub vm: usize,
    pub epoch: u64,
    /// [`FIELD_LINK_MEMO_GUARD_SLOTS`], `MH_STATIC_OWNER`, then
    /// [`field_link_memo_bound_word`] of `MH_BOUND`.
    pub guard: [u64; 8],
    /// The link word the candidate screen found (`FieldLinkCandidate::old`).
    pub old: i32,
}

/// The words a derivation wrote for a [`FieldLinkMemoKey`], and what of the
/// call it depended on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldLinkMemoAnswer {
    /// The packed [`MH_FIELD_LINK`] word written.
    pub state: i32,
    /// The `type` object linked ([`MH_FIELD_LINK_TYPE`]), as a word.
    pub type_word: u64,
    /// [`MH_FIELD_LINK_CLASS`].
    pub class_raw: i32,
    /// [`MH_FIELD_LINK_VALUE`]: the checked value's class, else `0`.
    pub value_raw: i32,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type.
    pub site_id: i32,
    /// The call's argument count after the handle.
    pub arity: usize,
    /// The exact class of the receiver the derivation admitted.
    pub recv_class: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct FieldLinkMemoLine {
    pub key: FieldLinkMemoKey,
    pub answer: FieldLinkMemoAnswer,
}

thread_local! {
    /// Per-thread link-derivation lines, direct-mapped by `MH_DESC`'s word. A
    /// context reporting VM identity `0` (the mocks) is never memoised outside
    /// the unit tests' own override.
    pub static FIELD_LINK_MEMO: std::cell::RefCell<[Option<FieldLinkMemoLine>; FIELD_LINK_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([None; FIELD_LINK_MEMO_SLOTS]) };
}

/// The VM identity the memo keys on, or `None` when it is off (the switch,
/// `CRATONVM_DBG_MH_FIELD_LINK`, or a context with no identity).
pub fn field_link_memo_vm(ctx: &dyn NativeContext) -> Option<usize> {
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
pub fn field_link_memo_bound_word(v: Value) -> u64 {
    match v {
        Value::Object(Some(_)) => u64::MAX - 1,
        other => bindto_mint_word(other),
    }
}

/// The memo key of `this` (width-checked by [`field_link_candidate`], which
/// found `candidate`) under `vm`. Reads only.
pub fn field_link_memo_key(
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

pub fn field_link_memo_line_index(key: &FieldLinkMemoKey) -> usize {
    ((key.guard[2] >> 3) as usize) & (FIELD_LINK_MEMO_SLOTS - 1)
}

/// Does a derivation that left the link word `state` leave words the memo
/// may remember? A written INSTANCE link whose slot is the recorded owner's
/// (bound, minted, or owner-admitting). Pure.
pub fn field_link_memo_keeps(state: i32) -> bool {
    state & FL_LINKED != 0
        && state & FL_STATIC == 0
        && state & (FL_BOUND | FL_OWNER_ADMITS | FL_MINT_SLOT) != 0
}

/// The receiver a link of `state` judges: the bound one, else the call's
/// first argument; `None` for a null or missing one. Reads only.
pub fn field_link_memo_receiver(
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
pub enum FieldLinkMemoReplay {
    /// The remembered link is written; the derivation is skipped.
    Written,
    /// Derive as before; then [`field_link_memo_fill_after`] with this key
    /// (`None`: the memo is off).
    Miss(Option<FieldLinkMemoKey>),
}

/// [`field_link_memo_replay`], noting in `note` (wave 71,
/// `CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`) what a written link admitted.
pub fn field_link_memo_replay_noting(
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
pub fn field_link_memo_write(
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
pub fn field_link_memo_fill_after(
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
pub fn field_link_memo_answer_of(
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
pub fn bound_link_call_extra<'a>(
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
pub fn mh_field_link_try(
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
pub fn mh_field_link_derive(
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
pub fn mh_field_link_try_bound(
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
pub fn mh_bindto_prelink_field_enabled() -> bool {
    crate::nbflags().mh_bindto_prelink_field
}

