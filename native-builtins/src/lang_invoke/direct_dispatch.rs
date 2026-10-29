// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =============================================================================
// T4: MethodHandle.invoke / invokeExact / invokeWithArguments
//
// MethodHandle layout (5 fields):
//   MH_CLASS  = 0  String  — class name (JVM-style, slash-separated)
//   MH_NAME   = 1  String  — method name
//   MH_DESC   = 2  String  — JVM descriptor
//   MH_KIND   = 3  Int     — 0=static 1=virtual 2=special 3=constructor
//                            4=getter 5=setter
//   MH_BOUND  = 4  Object  — bound receiver (for bound MH) or null
//
// Lookup.find* now populates these fields so invoke/invokeExact can dispatch.
// Existing zero-field stubs created by other subsystems remain harmless:
// invoke on a 0-field MH returns null/void.
// =============================================================================

// C15: Synthetic-MH field slots live AFTER the real JDK's instance fields.
// The real JDK `java/lang/invoke/MethodHandle` has 6 instance fields
// (type, form, asTypeCache, asTypeSoftCache, customizationCount,
// updateInProgress) at slots 0-5, where slot 0 (`type`) is the MethodType the
// JDK expects.  If we put MH_CLASS at slot 0 it collides with `type` and
// either overwrites JDK code's view of the handle or gets overwritten when
// the JDK class field is populated.  By anchoring our synthetic fields at
// slot 16 we reserve plenty of headroom past any future JDK field additions.
pub(crate) const MH_BASE: usize = 16;
pub(crate) const MH_CLASS: usize = MH_BASE + 0;
pub(crate) const MH_NAME: usize = MH_BASE + 1;
pub(crate) const MH_DESC: usize = MH_BASE + 2;
pub(crate) const MH_KIND: usize = MH_BASE + 3;
pub(crate) const MH_BOUND: usize = MH_BASE + 4;

/// The identity of the class a STATIC member handle was minted
/// against, taken from the caller's own `Class` object BEFORE the handle is
/// allocated (allocation can move the object the mirror was read from).
///
/// A handle records its owner as a binary-name string, and dispatch resolves
/// that string to "the" class of that name. Two loaders that define one binary
/// name are two classes (JVMS 5.3.4), so `findStaticGetter(forkCopy, "INSTANCE",
/// forkCopy).invoke()` read the APPLICATION copy's static -- and initialised it,
/// running a `<clinit>` the caller never asked for. `findConstructor` has kept
/// the id in `MH_BOUND` for the same reason since the Groovy work; the static
/// kinds ignore an `Int` there (only an `Object` there is a `bindTo` capture).
///
/// Every class under `--jdk-only`, where class bytes are authoritative.
///
/// `--compatible` keeps the by-name resolution for a class a built-in loader
/// defined, and records the owner of a class a USER-DEFINED loader defined
/// (interpreter round i1 wave 38, lane L4). For such a class the by-name
/// route answers the same class while its name is unique, and otherwise
/// either another loader's class or, once two user-defined loaders have each
/// defined the name, no class at all: `ClassManager::get_loaded_class_id`
/// refuses to guess, and the static arm of [`mh_dispatch`] returned the
/// load's `ClassNotFound` as an internal error, which Java cannot catch and
/// which ended the VM (`main-vm run() returned Err: ... class not found: PvI`,
/// probe `tools/probes/interp/L4/L4W38CompatibleStaticHandleOwner.java`, and
/// the second row of `L2W37InterfaceSelectionHot`, which mints a fresh loader
/// per row).
pub(crate) fn mh_static_owner_of_mirror(
    ctx: &dyn NativeContext,
    owner_mirror: Option<ObjectRef>,
) -> Option<ClassId> {
    let cid = ctx.class_id_from_mirror(owner_mirror?)?;
    // `CRATONVM_MH_COMPATIBLE_RECORD_OWNER` (interpreter round i2 wave 60,
    // lane L4): `--compatible` records every owner too, as `--jdk-only` does;
    // the by-name arms stay the fallback for a handle minted without one.
    // `JdkInvoke` (nio21 wave 12, L3): the row takes `--jdk-only`'s arm.
    if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
        || crate::nbflags().mh_compatible_record_owner
    {
        return Some(cid);
    }
    let user_defined = ctx.loader_id_of_class(cid)
        >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED as i32;
    user_defined.then_some(cid)
}

/// The `Class` object in a reflected `Method`/`Field`'s `clazz` field.
pub(crate) fn member_clazz_mirror(ctx: &mut dyn NativeContext, member: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(member, "clazz") {
        Value::Object(Some(mirror)) => Some(mirror),
        _ => None,
    }
}

/// Record [`mh_static_owner_of_mirror`]'s answer on a freshly allocated handle.
pub(crate) fn mh_record_static_owner(ctx: &mut dyn NativeContext, mh: ObjectRef, owner: Option<ClassId>) {
    if let Some(cid) = owner {
        ctx.set_field(mh, MH_BOUND, Value::Int(cid.as_u32() as i32));
    }
}

/// The owner recorded by [`mh_record_static_owner`], read back from `MH_BOUND`.
pub(crate) fn mh_static_owner(bound: Value) -> Option<ClassId> {
    match bound {
        Value::Int(raw) if raw > 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

/// The class that declares the static field `field` as JVMS §5.4.3.2 looks
/// it up from `named`: `named`, then its superinterfaces, then its superclass
/// chain. `None` when nothing on the way declares a static of that name.
pub(crate) fn static_field_holder(ctx: &dyn NativeContext, named: ClassId, field: &str) -> Option<ClassId> {
    fn walk(
        ctx: &dyn NativeContext,
        cid: ClassId,
        field: &str,
        budget: &mut u32,
    ) -> Option<ClassId> {
        // Bounded: a cyclic or absurdly deep hierarchy answers `None`.
        *budget = budget.checked_sub(1)?;
        if ctx.static_field_index_by_name(cid, field).is_some() {
            return Some(cid);
        }
        for iface in ctx.class_interfaces(cid) {
            if let Some(holder) = walk(ctx, iface, field, budget) {
                return Some(holder);
            }
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => walk(ctx, parent, field, budget),
            _ => None,
        }
    }
    let mut budget = 256;
    walk(ctx, named, field, &mut budget)
}

/// The DECLARING class of an INHERITED static field named through `named` —
/// `findStaticGetter(C.class, "f", ..)` / `findStaticSetter` /
/// `findStaticVarHandle` for an `f` that `P` (or an interface) declares — or
/// `None` when `named` declares `f` itself or nothing does.
///
/// Such a handle reads, writes and initializes the declaring class (JVMS
/// §5.5; `DirectMethodHandle.make` names `member.getDeclaringClass()`).
/// `static_field_index_by_name` answers only a class's own fields, so the
/// static field arms read slot 0 of `C` (or nothing) and ran `C.<clinit>`.
/// Takes no reference and cannot collect. Interpreter round i1 wave 16, L3.
pub(crate) fn inherited_static_field_holder(
    ctx: &dyn NativeContext,
    named: ClassId,
    field: &str,
) -> Option<ClassId> {
    if ctx.static_field_index_by_name(named, field).is_some() {
        return None;
    }
    static_field_holder(ctx, named, field).filter(|&holder| holder != named)
}

/// The DECLARING class of an INHERITED instance field named through `named`
/// (interpreter round i3 wave 79, lane L4): `None` when `named` declares an
/// instance field `name` itself or no superclass does, else the nearest
/// superclass that declares one (JVMS 5.4.3.2; an interface declares no
/// instance field). [`mhn_instance_field_holder_from`]'s walk. Reads only;
/// cannot collect.
pub(crate) fn inherited_instance_field_holder(
    ctx: &dyn NativeContext,
    named: ClassId,
    name: &str,
) -> Option<ClassId> {
    mhn_instance_field_holder_from(ctx, named, name)
        .filter(|&(holder, _)| holder != named)
        .map(|(holder, _)| holder)
}

/// The class declaring the instance field `name` as field resolution finds it
/// from `class_id` -- its own fields (a lambda proxy's captures included,
/// [`crate::lang_class::declared_fields_with_aliases`]), then up the
/// superclass chain -- with the field's slot. Bounded. Reads only (wave 79).
pub(crate) fn mhn_instance_field_holder_from(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<(ClassId, usize)> {
    let mut cur = Some(class_id);
    for _ in 0..64 {
        let cid = cur?;
        if let Some(slot) = crate::lang_class::declared_fields_with_aliases(ctx, cid)
            .into_iter()
            .find(|f| !f.is_static && f.name == name)
            .map(|f| f.slot_index)
        {
            return Some((cid, slot));
        }
        cur = ctx.superclass_of(cid).filter(|&parent| parent != cid);
    }
    None
}

/// [`inherited_static_field_holder`] for a static field method handle:
/// the class is the recorded owner ([`mh_static_owner`]) or, by name, the
/// loaded class of that name (a name no loader holds uniquely keeps the
/// by-name path of the caller).
pub(crate) fn mh_inherited_static_field_holder(
    ctx: &dyn NativeContext,
    bound: Value,
    class: &str,
    field: &str,
) -> Option<ClassId> {
    let named = match mh_static_owner(bound) {
        Some(owner) => owner,
        None => ctx.class_id_by_name(class)?,
    };
    inherited_static_field_holder(ctx, named, field)
}

/// The `asVarargsCollector` marking — `Int(1)` when this handle is a
/// variable-arity collector, `Int(0)`/absent otherwise (W7-19 §5.2).
///
/// Sixth synthetic slot; `alloc_string_concat_method_handle` allocates
/// `MH_VARARGS + 1`, and `alloc_method_handle` one more for
/// [`MH_STATIC_SETTLED`] (wave 20).
///
/// **Never read it raw.** Not every `java/lang/invoke/MethodHandle` in this VM
/// carries this layout: `MethodHandles.empty`/`zero` allocate 17 slots and
/// `panama.rs` uses a compact layout whose field 0 is a native address, so a
/// bare `get_field(mh, MH_VARARGS)` is an out-of-bounds read on a real
/// receiver — the same shape as the inert `arrayElementGetter` handle recorded
/// on [`MH_KIND_ARRAY_GET`], where slots 18 and 19 were read off the end of a
/// 17-slot object. [`mh_is_varargs_collector`] and
/// [`mh_set_varargs_collector`] width-guard with `object_num_fields` for that
/// reason.
pub(crate) const MH_VARARGS: usize = MH_BASE + 5;

/// Is this handle marked a variable-arity collector? Width-guarded — see
/// [`MH_VARARGS`]. A handle that is too narrow to carry the marking simply is
/// not one, which is the pre-W7-19 answer for every handle.
pub(crate) fn mh_is_varargs_collector(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    mh_marking_bits(ctx, mh) & MH_MARK_VARARGS != 0
}

/// Set or clear the marking, width-guarded — see [`MH_VARARGS`]. A handle
/// narrower than the standard synthetic layout is left exactly as it was; the
/// write is never allowed off the end of the object. The other bit of the
/// slot ([`MH_MARK_EXPLICIT_CAST`]) is kept.
pub(crate) fn mh_set_varargs_collector(ctx: &dyn NativeContext, mh: ObjectRef, on: bool) {
    let rest = mh_marking_bits(ctx, mh) & !MH_MARK_VARARGS;
    mh_write_marking_bits(ctx, mh, if on { rest | MH_MARK_VARARGS } else { rest });
}

/// [`MH_VARARGS`]'s bits. Bit 0 is the variable-arity marking (the slot's
/// original, and only, meaning until wave 26: `Int(1)` / `Int(0)`).
pub(crate) const MH_MARK_VARARGS: i32 = 1;

/// Bit 1 of [`MH_VARARGS`] (interpreter round i1 wave 26, lane L4): the handle's
/// `type` was stamped by `MethodHandles.explicitCastArguments`, whose argument
/// conversions are JLS 5.5 casts (a primitive narrowing, `null` to zero), not
/// `asType`'s. The leaf arms of [`mh_dispatch`] refuse an argument `asType`
/// cannot convert ([`adapt_leaf_args`]); beneath a handle carrying this bit
/// they cast instead ([`MH_EXPLICIT_SCOPE`], [`explicit_cast_primitive`]), so
/// no conversion an explicit cast performs is refused. A copy
/// ([`mh_clone_handle`]: `asType`, `asVarargsCollector`, ...) inherits it,
/// because it dispatches the same leaf through the same explicit cast.
pub(crate) const MH_MARK_EXPLICIT_CAST: i32 = 2;

/// Bit 2 of [`MH_VARARGS`]: [`MH_CAST_CHAIN`] holds work for [`mh_dispatch`]
/// -- at least one MIDDLE type (two or more entries), or a bound value's
/// refused cast. A single-stamp copy carries its one entry without the bit:
/// the doors and the leaf already judge outer-vs-leaf, so it costs nothing.
pub(crate) const MH_MARK_CAST_CHAIN: i32 = 4;

/// Every bit of [`MH_VARARGS`], one row each: `(name, bit, inherited by a
/// copy)`. Interpreter round i1 wave 51, lane L4
/// (`interpreter-L4-proposal-one-table-of-method-handle-marking-bits-FIXED`): the bits were
/// declared where their features live, hundreds of lines apart, and two of
/// them collided ([`MH_MARK_EXACT_INVOKER`] and [`MH_MARK_CAST_CHAIN`] were
/// both `4` until wave 50). A new bit is a new `MH_MARK_*` constant declared
/// in this group AND a row here; `the_marking_bits_are_one_table` (distinct
/// powers of two, every bit a row) and `the_marking_bits_have_one_writer`
/// (no `MH_VARARGS` store outside [`mh_write_marking_bits`]) hold the file
/// to that.
///
/// Per bit, which lanes it diverts and why a copy ([`mh_clone_handle`]:
/// `asFixedArity`, `asVarargsCollector`, `withVarargs`, an `asType` copy)
/// keeps it:
///
/// * `varargs` -- the collector's `asType` and the generic door collect the
///   trailing arguments; the direct lane and the plain static leaf decline.
///   Copied, then set or cleared by the copy's own factory.
/// * `explicit-cast` -- the leaf casts (JLS 5.5) instead of refusing an
///   `asType` conversion ([`MH_EXPLICIT_SCOPE`]); the direct lane declines.
///   Copied: the copy dispatches the same leaf through the same cast.
/// * `cast-chain` -- [`mh_dispatch`] judges [`MH_CAST_CHAIN`]; the direct
///   lane declines. Copied together with the chain slot it describes (a copy
///   that restamps calls [`mh_store_cast_chain`] / [`mh_clear_cast_chain`]).
/// * `exact-invoker` -- the invoker arm checks its target's type exactly
///   (`checkExactType`); the direct lane declines. Copied: the copy still
///   invokes its target exactly.
/// * `vh-invoker` -- a null `VarHandle` is HotSpot's helpful
///   `NullPointerException` ([`vh_invoker_null_handle`]); the direct lane
///   declines. Copied: the copy runs the same invoker form.
pub(crate) const MH_MARKS: [(&str, i32, bool); 5] = [
    ("varargs", MH_MARK_VARARGS, true),
    ("explicit-cast", MH_MARK_EXPLICIT_CAST, true),
    ("cast-chain", MH_MARK_CAST_CHAIN, true),
    ("exact-invoker", MH_MARK_EXACT_INVOKER, true),
    ("vh-invoker", MH_MARK_VH_INVOKER, true),
];

/// The bits a copy keeps ([`MH_MARKS`]' third column).
pub(crate) const MH_MARKS_INHERITED: i32 = {
    let mut bits = 0;
    let mut i = 0;
    while i < MH_MARKS.len() {
        if MH_MARKS[i].2 {
            bits |= MH_MARKS[i].1;
        }
        i += 1;
    }
    bits
};

/// The bits that send a call away from the direct lane
/// ([`mh_direct_lane_shape`]) and the plain static leaf
/// ([`mh_plain_static_leaf`]): every bit today, each for the reason its
/// [`MH_MARKS`] row gives. A bit that changes nothing a direct call does may
/// be left out of this mask, and only then.
pub(crate) const MH_MARKS_DIVERT_DIRECT_LANE: i32 = MH_MARK_VARARGS
    | MH_MARK_EXPLICIT_CAST
    | MH_MARK_CAST_CHAIN
    | MH_MARK_EXACT_INVOKER
    | MH_MARK_VH_INVOKER;

/// The ONE writer of [`MH_VARARGS`]: store `bits`, width-guarded (a handle
/// too narrow to carry the slot is left exactly as it was).
pub(crate) fn mh_write_marking_bits(ctx: &dyn NativeContext, mh: ObjectRef, bits: i32) {
    if ctx.object_num_fields(mh) > MH_VARARGS {
        ctx.set_field(mh, MH_VARARGS, Value::Int(bits));
    }
}

/// The [`MH_VARARGS`] slot as bits, `0` for a handle too narrow to carry it.
pub(crate) fn mh_marking_bits(ctx: &dyn NativeContext, mh: ObjectRef) -> i32 {
    if ctx.object_num_fields(mh) <= MH_VARARGS {
        return 0;
    }
    match ctx.get_field(mh, MH_VARARGS) {
        Value::Int(bits) => bits,
        _ => 0,
    }
}

/// Is this handle's `type` an `explicitCastArguments` stamp? See
/// [`MH_MARK_EXPLICIT_CAST`].
pub(crate) fn mh_is_explicit_cast(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    mh_marking_bits(ctx, mh) & MH_MARK_EXPLICIT_CAST != 0
}

/// Mark a handle `explicitCastArguments` just stamped. Width-guarded.
pub(crate) fn mh_mark_explicit_cast(ctx: &dyn NativeContext, mh: ObjectRef) {
    mh_write_marking_bits(ctx, mh, mh_marking_bits(ctx, mh) | MH_MARK_EXPLICIT_CAST);
}

/// Bit 3 of [`MH_VARARGS`] (interpreter round i1 wave 28, lane L4): an
/// `MH_KIND_INVOKER` handle `MethodHandles.exactInvoker` minted, whose target
/// must have exactly the type recorded in `MH_DESC` (HotSpot's
/// `Invokers.checkExactType`); without it an invoker is `invoker` /
/// `spreadInvoker`'s, which converts its target to that type instead
/// (`checkGenericType`). A copy (`asType`, `asVarargsCollector`, ...)
/// inherits it: it still invokes its target exactly. Also the
/// `varHandleExactInvoker` mark of an `MH_KIND_VIRTUAL` handle on
/// `VarHandle` (wave 50, `vh_invoker_call_refusal`).
///
/// It was bit 2 until wave 50, the bit [`MH_MARK_CAST_CHAIN`] also uses: an
/// exact invoker read as holding cast-chain work, and an invoker copy whose
/// chain was active read as exact.
pub(crate) const MH_MARK_EXACT_INVOKER: i32 = 8;

/// Bit 4 of [`MH_VARARGS`] (wave 50, lane L4): a `varHandleInvoker` /
/// `varHandleExactInvoker` handle ([`mhs_var_handle_invoker`]), whose null
/// `VarHandle` is HotSpot's helpful `NullPointerException` naming
/// `VarHandle.asDirect()` ([`vh_invoker_null_handle`]), where a direct
/// handle's null receiver has no message.
pub(crate) const MH_MARK_VH_INVOKER: i32 = 16;

/// The `NullPointerException` a `varHandleInvoker` / `varHandleExactInvoker`
/// handle raises for a null `VarHandle` (measured on 25.0.3, both factories,
/// `invoke` and `invokeExact`: the invoker's `LambdaForm` calls
/// `handle.asDirect()` first), or `None` for any other handle.
pub(crate) fn vh_invoker_null_handle(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<MethodCallFailed> {
    if mh_marking_bits(ctx, mh) & MH_MARK_VH_INVOKER == 0 {
        return None;
    }
    Some(
        RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.lang.invoke.VarHandle.asDirect()\" because \"handle\" is null"
                    .to_string(),
            ),
        }
        .into(),
    )
}

/// Is this an `exactInvoker` handle? See [`MH_MARK_EXACT_INVOKER`].
pub(crate) fn mh_is_exact_invoker(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    mh_marking_bits(ctx, mh) & MH_MARK_EXACT_INVOKER != 0
}

/// Mark a handle `exactInvoker` just minted. Width-guarded.
pub(crate) fn mh_mark_exact_invoker(ctx: &dyn NativeContext, mh: ObjectRef) {
    mh_write_marking_bits(ctx, mh, mh_marking_bits(ctx, mh) | MH_MARK_EXACT_INVOKER);
}

/// A `MH_KIND_STATIC` handle's settled owner: `Int(class id)` once a call
/// through the handle has left nothing to initialize for the class it
/// dispatches on (`NativeInvokeAccess::invoke_static_settling`), `Int(0)`
/// before. The static arm of [`mh_dispatch`] hands it back to the VM, which
/// then skips the initialization check — for an inherited static, a
/// class-manager read and a member resolution per call. HotSpot's
/// `DirectMethodHandle` drops its `ensureInitialized` form the same way once
/// the declaring class is initialized. Interpreter round i1 wave 20, lane L3.
///
/// Width-guarded like [`MH_VARARGS`]: only [`alloc_method_handle`] mints the
/// slot, and a narrower handle simply never settles. The VM re-checks the
/// remembered id against the owner each call resolves, so a stale or copied
/// value can only cost the fast path, never skip an initialization.
pub(crate) const MH_STATIC_SETTLED: usize = MH_BASE + 6;

/// Whether `mh` is wide enough to carry [`MH_STATIC_SETTLED`].
pub(crate) fn mh_has_static_settled_slot(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    ctx.object_num_fields(mh) > MH_STATIC_SETTLED
}

/// The owner [`MH_STATIC_SETTLED`] remembers, if any. Width-guarded.
pub(crate) fn mh_static_settled(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<ClassId> {
    if !mh_has_static_settled_slot(ctx, mh) {
        return None;
    }
    match ctx.get_field(mh, MH_STATIC_SETTLED) {
        Value::Int(raw) if raw > 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

/// Remember `owner` in [`MH_STATIC_SETTLED`]. Width-guarded.
pub(crate) fn mh_set_static_settled(ctx: &dyn NativeContext, mh: ObjectRef, owner: ClassId) {
    if mh_has_static_settled_slot(ctx, mh) {
        ctx.set_field(mh, MH_STATIC_SETTLED, Value::Int(owner.as_u32() as i32));
    }
}

/// One direct-mapped line of [`CARRIER_DOOR_KEY_MEMO`]: the VM, the
/// collection epoch, the carrier handle's address and the addresses of its
/// `MH_CLASS`, `MH_NAME` and `MH_DESC` objects, and the door-key slot kept
/// for that carrier. Interpreter round i2 wave 66, lane L3.
pub(crate) struct CarrierDoorKeyLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) carrier: usize,
    pub(crate) refs: [usize; 3],
    pub(crate) key: Option<std::rc::Rc<cratonvm_native_api::SettledDoorKey>>,
}

impl CarrierDoorKeyLine {
    /// A line no lookup matches (`vm` 0 is never memoised).
    const fn empty() -> Self {
        CarrierDoorKeyLine {
            vm: 0,
            epoch: 0,
            carrier: 0,
            refs: [0; 3],
            key: None,
        }
    }
}

/// Lines in [`CARRIER_DOOR_KEY_MEMO`]; a power of two (the index is a mask).
pub(crate) const CARRIER_DOOR_KEY_MEMO_SLOTS: usize = 16;

thread_local! {
    /// Per-thread door-key slots for the `--compatible` carrier handle
    /// (interpreter round i2 wave 66, lane L3,
    /// `CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY`; proposal
    /// `i65-L3-proposal-the-carrier-handle-keeps-its-door-key`). The carrier
    /// keeps its settled owner in the heap slot [`MH_STATIC_SETTLED`] and has
    /// no Rust record to hold a [`cratonvm_native_api::SettledDoorKey`], so
    /// the slot lives here, indexed by the carrier's address.
    ///
    /// The soundness argument is [`DIRECT_LANE_MEMO`]'s: within one VM and
    /// one collection epoch nothing moves or is freed, so an equal address
    /// is the same object. The carrier's identity alone is not trusted (a
    /// few paths rewrite `MH_NAME` / `MH_DESC` in place), so a hit also
    /// needs the same class, name and descriptor objects. The slot itself
    /// is tied by the VM to the settled owner and the class-redefinition
    /// count ([`cratonvm_native_api::SettledDoorKey::lookup`]), so a line
    /// that outlives either only misses. Per thread, like the Rust records'
    /// memos: the `Cell` is never shared between threads. A VM identity of
    /// `0` (the mocks) is never memoised.
    pub(crate) static CARRIER_DOOR_KEY_MEMO: std::cell::RefCell<[CarrierDoorKeyLine; CARRIER_DOOR_KEY_MEMO_SLOTS]> =
        std::cell::RefCell::new(std::array::from_fn(|_| CarrierDoorKeyLine::empty()));
}

/// The door-key slot [`CARRIER_DOOR_KEY_MEMO`] holds for the carrier at
/// `carrier` with member objects at `refs` in VM `vm`, epoch `epoch`: the
/// line's slot on a hit, else a fresh empty slot that replaces the line.
/// Pure bookkeeping; no `NativeContext` call, so no reentrancy under the
/// borrow.
pub(crate) fn carrier_door_key_line(
    vm: usize,
    epoch: u64,
    carrier: usize,
    refs: [usize; 3],
) -> std::rc::Rc<cratonvm_native_api::SettledDoorKey> {
    let index = (carrier >> 3) & (CARRIER_DOOR_KEY_MEMO_SLOTS - 1);
    CARRIER_DOOR_KEY_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let line = &mut memo[index];
        if line.vm == vm && line.epoch == epoch && line.carrier == carrier && line.refs == refs {
            if let Some(key) = &line.key {
                return key.clone();
            }
        }
        let key = std::rc::Rc::new(cratonvm_native_api::SettledDoorKey::new());
        *line = CarrierDoorKeyLine {
            vm,
            epoch,
            carrier,
            refs,
            key: Some(key.clone()),
        };
        key
    })
}

/// The door-key slot of a settled carrier handle `mh`, when the VM keeps
/// one (`NativeInvokeAccess::carrier_settled_door_key`, interpreter round
/// i2 wave 66, lane L3); `None` with the switch off, for an unsettled
/// handle, or in a context with no VM identity. Pure reads; no GC point --
/// call it where `mh` is current, beside [`mh_static_settled`].
pub(crate) fn mh_carrier_door_key(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    settled: Option<ClassId>,
) -> Option<std::rc::Rc<cratonvm_native_api::SettledDoorKey>> {
    settled?;
    if !ctx.carrier_settled_door_key() {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let addr = |v: Value| match v {
        Value::Object(Some(o)) => o.as_ptr() as usize,
        _ => 0,
    };
    let refs = [
        addr(ctx.get_field(mh, MH_CLASS)),
        addr(ctx.get_field(mh, MH_NAME)),
        addr(ctx.get_field(mh, MH_DESC)),
    ];
    Some(carrier_door_key_line(
        vm,
        ctx.gc_address_epoch(),
        mh.as_ptr() as usize,
        refs,
    ))
}

/// The carrier's settling call: `NativeInvokeAccess::invoke_static_settling_keyed`
/// with the slot [`mh_carrier_door_key`] gave, else the unkeyed
/// `invoke_static_settling`. Interpreter round i2 wave 66, lane L3.
#[allow(clippy::too_many_arguments)]
pub(crate) fn carrier_invoke_static_settling(
    ctx: &mut dyn NativeContext,
    door_key: Option<&cratonvm_native_api::SettledDoorKey>,
    owner: Option<ClassId>,
    settled: Option<ClassId>,
    class: &str,
    name: &str,
    desc: &str,
    args: &[Value],
) -> (MethodCallResult, Option<ClassId>) {
    match door_key {
        Some(key) => ctx.invoke_static_settling_keyed(owner, settled, key, class, name, desc, args),
        None => ctx.invoke_static_settling(owner, settled, class, name, desc, args),
    }
}

/// A `findSpecial` / `unreflectSpecial` handle's `specialCaller` (internal
/// name, a `String`), or null. The JDK narrows such a handle's receiver to the
/// caller (`Lookup.restrictReceiver`), which is its declared `type` until an
/// `asType` copy restamps `type`; `MH_CLASS` is the class dispatch resolves in
/// and says nothing about it. The leaf casts of `invoke` and `invokeExact`
/// read it from here, so the narrowing survives the copy (round 12 wave 5,
/// lane hunter3). Width-guarded like [`MH_VARARGS`]: only
/// [`alloc_method_handle_with_receiver`] mints the slot.
pub(crate) const MH_SPECIAL_CALLER: usize = MH_BASE + 7;

/// The receiver class the leaf of a virtual/special handle requires:
/// [`MH_SPECIAL_CALLER`] when recorded, else `class` (`MH_CLASS`).
pub(crate) fn mh_leaf_receiver_class(ctx: &dyn NativeContext, mh: ObjectRef, class: String) -> String {
    if ctx.object_num_fields(mh) <= MH_SPECIAL_CALLER {
        return class;
    }
    match ctx.get_field(mh, MH_SPECIAL_CALLER) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or(class),
        _ => class,
    }
}

/// `CRATONVM_MH_SPECIAL_CALLER_CAST` -- default ON (round 12 wave 5, lane
/// hunter3); `0`/`false`/`off`/`no` mints `findSpecial` handles without
/// [`MH_SPECIAL_CALLER`], so their leaf receiver cast is `MH_CLASS` again.
/// Read per special-handle mint (a cold path).
pub(crate) fn special_caller_cast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_SPECIAL_CALLER_CAST")
}

// ---------------------------------------------------------------------------
// The adapter chain of an `asType` / `explicitCastArguments` copy
// (round 12 wave 7, lane mh5)
// ---------------------------------------------------------------------------
//
// HotSpot's `h.asType(A).asType(B)` is an adapter (B -> A) around an adapter
// (A -> h.type()) around `h`. CratonVM stamps each result as a COPY of `h`
// with `type` restamped (`mh_with_stamped_type`), so the doors saw only the
// outermost type and the leaf, and every cast the MIDDLE types owe was lost:
// `takesObj(Object).asType((Sub)int).asType((Object)int).invokeExact(mid)`
// answered where HotSpot throws `ClassCastException` (Object -> Sub), and a
// middle `Integer` RETURN let a `String` through
// (`r12w6-hunter4-methodhandle-adapter-casts-lost-by-chains-bindto-and-iwa`).
// `bindTo` after `asType` lost the adapter's cast of the bound value the same
// way (the bound handle is minted fresh from `MH_CLASS` / `MH_DESC`).
//
// [`MH_CAST_CHAIN`] now records every type beneath the copy's `type`, and
// [`mh_dispatch`] judges the arguments and the return through each of them
// (`mh_cast_chain_judge_args` / `_return`). The judge only REFUSES: it tracks
// each value through the links (so a later link sees what the earlier one
// produced) but hands the leaf the values it was given, which the leaf and the
// doors convert exactly as before. See
// `docs/feature-designs/profile-guided-inlining.md`
// for what that leaves.

/// Ninth synthetic slot: a `String`, the types beneath this handle's `type`,
/// outermost first, each prefixed with the operation that converts INTO it
/// from the type above (`S` = `asType`, `E` = `explicitCastArguments`); the
/// last entry is the unstamped handle's own type. An optional leading
/// `!<field descriptor>` records a bound value that fails that cast (every
/// invocation raises it, as HotSpot's adapter under a `BoundMethodHandle`
/// does). Width-guarded like [`MH_VARARGS`]: [`alloc_method_handle`] mints it
/// (round 12 wave 7), still below `panama::DOWNCALL_BASE`.
///
/// Recorded only for the kinds [`kind_has_authoritative_adapted_type`] names
/// (their `type` is measured correct); a handle of any other kind keeps the
/// outer-vs-leaf model.
pub(crate) const MH_CAST_CHAIN: usize = MH_BASE + 8;

/// [`MH_MARK_CAST_CHAIN`] (declared with the other marking bits, see
/// [`MH_MARKS`]) says whether this chain holds work.
///
/// A chain longer than this is not recorded (the copy keeps the outer-vs-leaf
/// model): a program that restamps one handle in a loop would otherwise grow
/// the chain without bound.
pub(crate) const MH_CAST_CHAIN_MAX_LINKS: usize = 16;

/// Tenth synthetic slot (round 15 wave 5, lane mh4;
/// `r15w4-mh3-bindto-static-drops-recorded-owner-FIXED-20260930.md`): a BOUND
/// `STATIC` handle's recorded owner, `Int(class id)`, or `Int(0)`. An unbound
/// one keeps it in `MH_BOUND` ([`mh_record_static_owner`]), where `bindTo`
/// stores the captured argument, so the bound copy dispatched by NAME again
/// (another loader's class of that name, or the internal `ClassNotFound`
/// interpreter wave 38 fixed for unbound handles). Written by `bindTo`'s
/// ordinary mint ([`mh_bindto_static_owner_enabled`]), read through
/// [`mh_static_owner_in`]. Width-guarded like [`MH_VARARGS`]: minted by
/// [`alloc_method_handle`] and [`alloc_bound_method_handle`], copied by
/// [`mh_clone_handle`], still below `panama::DOWNCALL_BASE`.
pub(crate) const MH_STATIC_OWNER: usize = MH_BASE + 9;

/// [`mh_static_owner`] of `bound` (`mh`'s `MH_BOUND`), else the owner a bound
/// static handle carries in [`MH_STATIC_OWNER`]. `mh` must be current; reads
/// only.
pub(crate) fn mh_static_owner_in(ctx: &dyn NativeContext, mh: ObjectRef, bound: Value) -> Option<ClassId> {
    mh_static_owner(bound).or_else(|| {
        if ctx.object_num_fields(mh) <= MH_STATIC_OWNER {
            return None;
        }
        mh_static_owner(ctx.get_field(mh, MH_STATIC_OWNER))
    })
}

// ---------------------------------------------------------------------------
// The owner of an unbound INSTANCE handle, by class id (interpreter round i2
// wave 62, lane L4; `CRATONVM_MH_INSTANCE_OWNER_ID`, proposal
// `i61-L4-proposal-method-handles-name-their-owner-by-class-id-20261025.md`,
// first stage)
// ---------------------------------------------------------------------------
//
// An instance handle names its member's class only as the string `MH_CLASS`,
// and the leaf's receiver cast (`leaf_receiver_refusal`) re-resolved that
// name on every call. A lambda proxy's handle is minted over the class `""`
// (its mirror names no class), so its cast was skipped outright: the door ran
// `run()` on a `String` (`NoSuchMethodError`) and `bindTo("x")` answered a
// handle, where HotSpot raises `ClassCastException: Cannot cast
// java.lang.String to P` (`L4W61LambdaProxyHandleGaps`).
//
// With the switch on, a `VIRTUAL` handle minted by `findVirtual` /
// `unreflect`, and a lambda proxy's method, getter and setter handles, keep
// the owner's `ClassId` in `MH_STATIC_OWNER` -- the slot a bound `STATIC`
// handle uses, which no instance arm reads ([`mh_static_owner_in`] is asked by
// the static arms only). An `asType` copy keeps it ([`mh_clone_handle`] copies
// every slot); a `bindTo` mint writes `0` (its receiver was cast by `bindTo`),
// except that a bound instance FIELD handle keeps it for its slot lookup
// (wave 65, [`BoundFieldCarry`]).
// The receiver cast admits an exact or subclass receiver by id, and refuses a
// nameless owner's other receivers itself; a named owner's other receivers
// keep the by-name judgement (interfaces, stand-ins), so the switch moves no
// answer for a named class.

/// The owner [`mh_record_instance_owner`] left on unbound instance handle
/// `mh`, or `None` (switch off, a narrow handle, nothing recorded). Callers ask
/// only for an instance kind. Reads only.
pub(crate) fn mh_instance_owner(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<ClassId> {
    if !crate::nbflags().mh_instance_owner_id || ctx.object_num_fields(mh) <= MH_STATIC_OWNER {
        return None;
    }
    // A lambda proxy's id is >= 0x8000_0000, a negative `Int`: any non-zero
    // value is an owner (the static reader's `> 0` would drop it).
    match ctx.get_field(mh, MH_STATIC_OWNER) {
        Value::Int(raw) if raw != 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

/// The `VIRTUAL` leaf call of a handle whose declaring class is `owner`
/// (interpreter round i6 wave 118, lane L4, `CRATONVM_MH_VIRTUAL_BY_OWNER_ID`,
/// default ON since wave 134 by user decision, `=0` off; the `VIRTUAL` half of
/// `i63-L4-proposal-virtual-door-dispatches-by-owner-id`): by the owner's id
/// when it is known and the switch is on, else by `class`'s name as before.
pub(crate) fn mh_invoke_virtual_by_owner(
    ctx: &mut dyn NativeContext,
    owner: Option<ClassId>,
    class: &str,
    receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    match owner {
        // WARNING(i6-user-flip): CRATONVM_MH_VIRTUAL_BY_OWNER_ID default ON by user decision 2026-10-19; =0 restores the old path
        Some(owner) if crate::nbflags().mh_virtual_by_owner_id => {
            ctx.invoke_virtual_declared_by_class_id(owner, class, receiver, name, desc, args)
        }
        _ => ctx.invoke_virtual_declared(class, receiver, name, desc, args),
    }
}

/// Record `owner` on the fresh unbound instance handle `mh` (current), under
/// the switch. Width-guarded.
pub(crate) fn mh_record_instance_owner(ctx: &dyn NativeContext, mh: ObjectRef, owner: Option<ClassId>) {
    let Some(owner) = owner else {
        return;
    };
    if crate::nbflags().mh_instance_owner_id && ctx.object_num_fields(mh) > MH_STATIC_OWNER {
        ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(owner.as_u32() as i32));
    }
}

/// `Class.getName()` of `owner`: a lambda proxy's own name (its `/0x..`
/// suffix kept), else the binary name.
pub(crate) fn mh_owner_display_name(ctx: &dyn NativeContext, owner: ClassId) -> String {
    if let Some(name) = crate::lang_class::lambda_proxy_class_name(ctx, owner) {
        return name;
    }
    ctx.class_name_of_id(owner)
        .unwrap_or_default()
        .class_text(&*ctx)
}

/// The `ClassCastException` HotSpot's `checkReceiver` / `bindTo` cast raises
/// for `value` against `owner` (named by [`mh_owner_display_name`]).
pub(crate) fn owner_cast_failure(
    ctx: &mut dyn NativeContext,
    value: ObjectRef,
    owner: ClassId,
) -> MethodCallFailed {
    let from = if ctx.object_is_array(value) {
        entry_value_type_display(&*ctx, value)
    } else {
        let cid = ctx.class_id_of_object(value);
        mh_owner_display_name(&*ctx, cid)
    };
    RuntimeError::ClassCastException {
        message: format!(
            "Cannot cast {} to {}",
            from.class_text(&*ctx),
            mh_owner_display_name(&*ctx, owner)
        ),
    }
    .into()
}

/// The receiver cast of `owner`'s unbound instance handle: `Some(None)` when
/// the id admits `receiver` (the same class or a subclass), `Some(Some(e))`
/// when the owner has no name (`class` is `""`) and the receiver is not one,
/// `None` when the by-name judgement must answer.
pub(crate) fn owner_receiver_verdict(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    owner: ClassId,
    class: &str,
) -> Option<Option<MethodCallFailed>> {
    owner_receiver_verdict_with(ctx, receiver, owner, class, mh_owner_id_authority_enabled())
}

/// [`owner_receiver_verdict`] with `CRATONVM_MH_OWNER_ID_AUTHORITY` as a
/// parameter (the unit tests ask both answers): with `authority`, a receiver
/// of another loader's class named `class` (or a subclass of one,
/// [`owner_foreign_namesake`]) is refused by the id too, where the by-name
/// judgement admitted it.
pub(crate) fn owner_receiver_verdict_with(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    owner: ClassId,
    class: &str,
    authority: bool,
) -> Option<Option<MethodCallFailed>> {
    if !ctx.object_is_array(receiver) {
        let rc = ctx.class_id_of_object(receiver);
        if rc == owner || ctx.is_subclass(rc, owner) {
            return Some(None);
        }
        if authority && owner_foreign_namesake(&*ctx, rc, owner, class) {
            return Some(Some(owner_cast_failure(ctx, receiver, owner)));
        }
        if authority && owner_refusal_the_name_cannot_judge(&*ctx, rc, owner, class) {
            return Some(Some(owner_cast_failure(ctx, receiver, owner)));
        }
    }
    if class.is_empty() {
        return Some(Some(owner_cast_failure(ctx, receiver, owner)));
    }
    None
}

/// Must the id refuse a receiver of class `rc` that is neither `owner` nor a
/// subclass of it, because the by-name judgement would admit it blind
/// (interpreter round i3 wave 79, lane L4; page
/// `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver`)?
/// `true` when `class` (the owner's name) names no UNIQUE class --
/// `class_id_by_name` answers `None` for a name several
/// loaders define, and [`leaf_reference_refusal`] then fails open -- and
/// nothing about the pair is one the id cannot settle: the owner is a class
/// (an interface's implementors are judged by name), and the receiver is an
/// ordinary loaded class (not a lambda proxy, a `java.lang.reflect.Proxy` or
/// a VM stand-in, which acquire their supertypes outside the loaded
/// hierarchy). `L4W65FieldLinkSubclasses` defines its `Base` twice: a `String`
/// handed to the `(Object)int` copy of a `Base` getter read a slot of the
/// `String` (`0`) where HotSpot's `checkBase` cast throws
/// `ClassCastException: Cannot cast java.lang.String to ..$Base`. Read only
/// after the id refused (the refusal path). Reads only.
pub(crate) fn owner_refusal_the_name_cannot_judge(
    ctx: &dyn NativeContext,
    rc: ClassId,
    owner: ClassId,
    class: &str,
) -> bool {
    if class.is_empty() || class.starts_with('[') || class == "java/lang/Object" {
        return false;
    }
    if rc.as_u32() >= 0x8000_0000 || owner.as_u32() >= 0x8000_0000 {
        return false;
    }
    if ctx.class_id_by_name(class).is_some() || ctx.is_interface_class(owner) {
        return false;
    }
    let Some(name) = ctx.class_name_arc_of_id(rc) else {
        return false;
    };
    !ctx.is_class_synthetic_stub(&name)
        && !crate::lang_class::reflective_name_is_runtime_proxy(&name)
}

/// [`leaf_receiver_refusal`] for unbound instance handle `mh` (current): the
/// recorded owner answers first ([`owner_receiver_verdict`]); one predictable
/// branch when the switch is off or nothing is recorded.
pub(crate) fn leaf_receiver_refusal_owned(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    receiver: ObjectRef,
    class: &str,
    explicit: bool,
) -> Option<MethodCallFailed> {
    if let Some(owner) = mh_instance_owner(&*ctx, mh) {
        if let Some(verdict) = owner_receiver_verdict(ctx, receiver, owner, class) {
            return verdict;
        }
    }
    leaf_receiver_refusal(ctx, receiver, class, explicit)
}

/// `bindTo(recv)`'s cast of a non-null `recv` on unbound instance handle `mh`
/// (current) by its recorded owner: the refusal only when the owner has no
/// name (a lambda proxy's), whose `type` the by-name check cannot read. A
/// named owner keeps `bindTo`'s own check. Reads before any allocation.
pub(crate) fn bind_to_owner_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    recv: Value,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(r)) = recv else {
        return None;
    };
    let owner = mh_instance_owner(&*ctx, mh)?;
    let instance_kind = matches!(
        ctx.get_field(mh, MH_KIND),
        Value::Int(k) if k == MH_KIND_VIRTUAL || k == MH_KIND_GETTER || k == MH_KIND_SETTER
    );
    if !instance_kind || matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_))) {
        return None;
    }
    // The id admits an exact or subclass receiver before the class name is
    // decoded (the common bind).
    let rc = (!ctx.object_is_array(r)).then(|| ctx.class_id_of_object(r));
    if let Some(rc) = rc {
        if rc == owner || ctx.is_subclass(rc, owner) {
            return None;
        }
    }
    let class = mh_read_class(&*ctx, mh).unwrap_or_default();
    // Stage 2 (wave 63, `CRATONVM_MH_OWNER_ID_AUTHORITY`): another loader's
    // class of the owner's name is refused by the id, as `bindTo`'s
    // `Class.cast` refuses it on HotSpot; the by-name check below admitted it.
    if let Some(rc) = rc {
        if mh_owner_id_authority_enabled() && owner_foreign_namesake(&*ctx, rc, owner, &class) {
            return Some(owner_cast_failure(ctx, r, owner));
        }
    }
    // A named owner keeps `bindTo`'s own by-name check.
    class.is_empty().then(|| owner_cast_failure(ctx, r, owner))
}

// ---------------------------------------------------------------------------
// Stage 2: the recorded owner is the authority (interpreter round i2 wave 63,
// lane L4; `CRATONVM_MH_OWNER_ID_AUTHORITY`, opt-in; proposal
// `i61-L4-proposal-method-handles-name-their-owner-by-class-id-20261025.md`)
// ---------------------------------------------------------------------------
//
// Stage 1 recorded the owner of a `VIRTUAL` handle and of a lambda proxy's
// handles, and let it ADMIT a receiver by id; a named owner's mismatch still
// asked the name, and field handles of named classes recorded nothing, so the
// door's instance field arms re-resolved `MH_CLASS` by name for the slot on
// every `invoke` (a class-manager name lookup), and the field-accessor link
// did it again at link time.
//
// With the switch on (and stage 1's `CRATONVM_MH_INSTANCE_OWNER_ID`, default
// on):
//
// * `findGetter` / `findSetter` / `unreflectGetter` / `unreflectSetter` of an
//   instance field record the owner (the `refc` mirror, or `Field.clazz`) in
//   `MH_STATIC_OWNER`, as stage 1's `VIRTUAL` handles do
//   ([`mh_field_owner_of_mirror`]);
// * the door's instance `GETTER` / `SETTER` arms and [`mh_field_link_try`]
//   resolve the slot by the owner's id ([`mh_instance_field_slot`]), the
//   by-name lookup kept as the fallback (a lambda proxy's capture, a handle
//   minted without an owner);
// * the owner refuses a receiver whose class -- or a superclass of it --
//   carries the owner's NAME but is another loader's class
//   ([`owner_foreign_namesake`]): `findVirtual(BaseA, ..).invoke(baseB)` ran
//   `BaseB`'s method, and a field handle read `BaseB`'s slot, where HotSpot's
//   cast raises `ClassCastException: Cannot cast Base to Base`. Only a
//   mismatch both loaders of which the VM KNOWS is refused
//   (`loader_id_of_class_definite`), so a stand-in class of this VM whose
//   loader is unrecorded keeps the by-name answer.

/// `CRATONVM_MH_OWNER_ID_AUTHORITY` (opt-in; see the section comment).
pub(crate) fn mh_owner_id_authority_enabled() -> bool {
    crate::nbflags().mh_owner_id_authority
}

/// Is `rc` -- a receiver's class that is neither `owner` nor a subclass of
/// it -- another loader's class named `class` (the owner's `MH_CLASS`), or a
/// subclass of one? Walks `rc`'s superclass chain for the first class carrying
/// the name and answers whether its defining loader is known and differs from
/// the owner's. Flag-free (the callers ask the switch). Reads only.
pub(crate) fn owner_foreign_namesake(
    ctx: &dyn NativeContext,
    rc: ClassId,
    owner: ClassId,
    class: &str,
) -> bool {
    if class.is_empty() || class.starts_with('[') {
        return false;
    }
    let Some(owner_loader) = ctx.loader_id_of_class_definite(owner) else {
        return false;
    };
    let mut cid = rc;
    for _ in 0..64 {
        if cid == owner {
            return false;
        }
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(class) {
            return ctx
                .loader_id_of_class_definite(cid)
                .is_some_and(|loader| loader != owner_loader);
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return false,
        }
    }
    false
}

/// The owner a fresh unbound instance FIELD handle records (stage 2): the
/// class `mirror` (the finder's `refc`, or a `Field`'s `clazz`) names, under
/// `CRATONVM_MH_OWNER_ID_AUTHORITY`; `None` otherwise, or for a mirror that
/// names no class of the store (a primitive or array type has no fields).
/// Reads only.
pub(crate) fn mh_field_owner_of_mirror(ctx: &dyn NativeContext, mirror: Option<ObjectRef>) -> Option<ClassId> {
    if !mh_owner_id_authority_enabled() {
        return None;
    }
    let cid = ctx.class_id_from_mirror(mirror?)?;
    let name = ctx.class_name_arc_of_id(cid)?;
    (!name.is_empty() && !name.starts_with('[')).then_some(cid)
}

/// The owner the door's instance field arms resolve the slot in (stage 2):
/// [`mh_instance_owner`] under `CRATONVM_MH_OWNER_ID_AUTHORITY`, else `None`
/// (the by-name lookup). `mh` must be current. Reads only.
pub(crate) fn mh_field_slot_owner(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<ClassId> {
    if !mh_owner_id_authority_enabled() {
        return None;
    }
    mh_instance_owner(ctx, mh)
}

/// The instance slot of field `name`: in `owner`'s hierarchy by id when one is
/// given and declares it there, else by `class`'s name (the pre-stage-2
/// lookup, and the fallback for an owner outside the class store -- a lambda
/// proxy, whose captures the caller finds itself).
pub(crate) fn mh_instance_field_slot(
    ctx: &dyn NativeContext,
    owner: Option<ClassId>,
    class: &str,
    name: &str,
) -> Option<usize> {
    if let Some(slot) = owner.and_then(|o| ctx.resolve_field_index_by_class_id(o, name)) {
        return Some(slot);
    }
    ctx.resolve_field_index(class, name)
}

// ---------------------------------------------------------------------------
// An instance field handle carries its slot from mint (interpreter round i2
// wave 64, lane L4; `CRATONVM_MH_FIELD_SLOT_FROM_MINT`, proposal
// `i63-L4-proposal-field-handles-carry-their-slot-from-mint-20261027.md`)
// ---------------------------------------------------------------------------
//
// A field handle's member never changes, so neither does its instance slot in
// the recorded owner's layout. Stage 2 (`CRATONVM_MH_OWNER_ID_AUTHORITY`)
// still resolved it on EVERY `invoke` through the door's `GETTER` / `SETTER`
// arm ([`mh_instance_field_slot`]: a class-manager read and a walk of the
// owner's hierarchy). With this switch on (and stage 2's), the four finders
// that record an instance field owner (`findGetter`, `findSetter`,
// `unreflectGetter`, `unreflectSetter`) resolve the slot once, by the owner's
// id, and keep it in the field-accessor link's packed state
// ([`MH_FIELD_LINK`]): bits 0-15 the slot, [`FL_MINT_SLOT`] (bit 31) saying
// those bits are the MINTED slot. `FL_LINKED` stays clear, so nothing reads
// the handle as linked; a link made later keeps the bit (its bits 0-15 are the
// same slot), and a copy ([`mh_clone_handle`] -> [`mh_clear_field_link`])
// keeps the minted slot while it drops the link. No new handle slot: the
// window below `panama::DOWNCALL_BASE` is full.
//
// The minted slot is exactly what the per-call lookup answered for that
// owner (the owner's layout is fixed once the class is defined, and JVMTI
// redefinition may not change fields), and it is read only when the arm also
// finds the owner ([`mh_field_slot_owner`]): the same receiver casts admit the
// same receivers, and only the lookup is skipped. A handle minted without one
// (switch off at mint, an owner that does not declare the name, a lambda
// proxy's capture, a slot past 16 bits) keeps the lookup.

/// `CRATONVM_MH_FIELD_SLOT_FROM_MINT` (opt-in; see the section comment).
pub(crate) fn mh_field_slot_from_mint_enabled() -> bool {
    crate::nbflags().mh_field_slot_from_mint
}

/// The packed state a fresh handle's [`MH_FIELD_LINK`] holds to carry
/// instance `slot` from mint, or `None` when it does not fit.
pub(crate) fn field_link_minted_state(slot: usize) -> Option<i32> {
    (slot <= 0xFFFF).then_some(FL_MINT_SLOT | slot as i32)
}

/// The slot a packed [`MH_FIELD_LINK`] state carries from mint, if it does.
pub(crate) fn field_link_state_minted_slot(state: i32) -> Option<usize> {
    (state & FL_MINT_SLOT != 0).then_some((state & 0xFFFF) as usize)
}

/// What a COPY keeps of its source's packed link state: the minted slot only
/// (the link, its flags and its relink count describe the source).
pub(crate) fn field_link_state_kept_by_copy(state: i32) -> i32 {
    if state & FL_MINT_SLOT != 0 {
        state & (FL_MINT_SLOT | 0xFFFF)
    } else {
        0
    }
}

/// A new link `state` of `slot`, carrying `old`'s minted bit when `old`
/// minted that same slot (an instance link of a minted handle).
pub(crate) fn field_link_state_keeping_mint(state: i32, old: i32, slot: usize) -> i32 {
    match field_link_state_minted_slot(old) {
        Some(minted) if minted == slot => state | FL_MINT_SLOT,
        _ => state,
    }
}

/// Record `owner`'s slot of field `name` on the fresh instance field handle
/// `mh` (current, just minted, unlinked), when `on` and `owner` declares it.
/// Width-guarded. Reads the class store; allocates nothing.
pub(crate) fn mh_record_minted_field_slot_with(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    owner: Option<ClassId>,
    name: &str,
    on: bool,
) {
    if !on || ctx.object_num_fields(mh) < MH_SLOT_COUNT {
        return;
    }
    let Some(owner) = owner else {
        return;
    };
    let Some(state) = ctx
        .resolve_field_index_by_class_id(owner, name)
        .and_then(field_link_minted_state)
    else {
        return;
    };
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
}

/// [`mh_record_minted_field_slot_with`] under the switch.
pub(crate) fn mh_record_minted_field_slot(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    owner: Option<ClassId>,
    name: &str,
) {
    mh_record_minted_field_slot_with(ctx, mh, owner, name, mh_field_slot_from_mint_enabled());
}

/// The slot `mh` carries from mint, when `on`. Width-guarded; reads only.
pub(crate) fn mh_minted_field_slot_with(ctx: &dyn NativeContext, mh: ObjectRef, on: bool) -> Option<usize> {
    if !on || ctx.object_num_fields(mh) < MH_SLOT_COUNT {
        return None;
    }
    match ctx.get_field(mh, MH_FIELD_LINK) {
        Value::Int(state) => field_link_state_minted_slot(state),
        _ => None,
    }
}

/// The slot the door's instance field arms use without a lookup: the minted
/// one, read only for a handle whose stage-2 owner the arm found
/// (`field_owner`). `mh` must be current. Reads only.
pub(crate) fn mh_field_arm_minted_slot(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    field_owner: Option<ClassId>,
) -> Option<usize> {
    field_owner?;
    mh_minted_field_slot_with(ctx, mh, mh_field_slot_from_mint_enabled())
}

// ---------------------------------------------------------------------------
// A bound field handle keeps its owner and its minted slot (interpreter round
// i2 wave 65, lane L4; `CRATONVM_MH_BOUND_FIELD_OWNER`, default on, kill switch
// `=0`; proposal
// `i64-L4-proposal-bound-field-handles-keep-owner-and-slot-20261028.md`)
// ---------------------------------------------------------------------------
//
// Stage 2 gives an unbound instance field handle its owner's `ClassId`
// (`MH_STATIC_OWNER`) and wave 64 its slot (`MH_FIELD_LINK`'s minted bits).
// `bindTo`'s ordinary mint wrote `0` to both, so the bound copy's `GETTER` /
// `SETTER` arm (the `Value::Object(Some(r)) = bound` branch) had no owner and
// resolved the slot BY NAME (`resolve_field_index(MH_CLASS, name)`) on every
// call: loader-blind -- `bindTo` cast the receiver against the owner by id,
// but the slot came from whichever class the NAME found, so two loaders'
// classes of one name with different layouts read the wrong slot of the bound
// receiver -- and a class lookup plus a hierarchy walk per call.
//
// With the switch on, the bound copy of an unbound INSTANCE `GETTER` /
// `SETTER` keeps the source's recorded owner in `MH_STATIC_OWNER` and, when
// the source carries a minted slot, that slot in `MH_FIELD_LINK`
// ([`field_link_state_kept_by_copy`]; the shared mint is then allocated as
// wide as [`alloc_method_handle`]'s, so the link slots exist). Every reader of
// an instance owner either serves the bound arm (the door's `GETTER` /
// `SETTER` arms, through [`mh_field_slot_owner`]) or already excludes a bound
// handle (`bind_to_owner_refusal`, the leaf receiver cast of the unbound
// branch, `revealDirect`'s lambda-proxy branch, the field-accessor link). The
// MHF-2 fast mint fills a line for a field kind only under wave 66's
// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`, and then carries the same
// ([`BindToMintAnswer::field_carry`]). Under wave 66's
// `CRATONVM_MH_FIELD_LINK_BOUND` the copy is wide whenever it carries an
// owner, and the field-accessor link serves it ([`mh_field_link_try_bound`]).

/// `CRATONVM_MH_BOUND_FIELD_OWNER` (default on; see the section comment).
pub(crate) fn mh_bound_field_owner_enabled() -> bool {
    crate::nbflags().mh_bound_field_owner
}

/// What `bindTo`'s ordinary mint carries from an unbound instance field handle
/// to its bound copy (wave 65).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundFieldCarry {
    /// The source's recorded owner ([`mh_instance_owner`]).
    pub(crate) owner: ClassId,
    /// The source's packed link state as a copy keeps it (the minted slot
    /// only), `0` when it carries none.
    pub(crate) link_state: i32,
}

impl BoundFieldCarry {
    /// Does the bound copy need [`alloc_method_handle`]'s width (the link
    /// slots) to hold this?
    pub(crate) fn needs_link_slots(&self) -> bool {
        self.link_state != 0
    }
}

/// The carry for `bindTo(recv)` of `src` (current; `kind` / `desc` its
/// `MH_KIND` / `MH_DESC`, `owner` its [`mh_instance_owner`]), when `on`:
/// only an UNBOUND instance getter (a receiver parameter) or setter (two
/// parameters) with a recorded owner. Width-guarded; reads only.
pub(crate) fn bound_field_carry_with(
    ctx: &dyn NativeContext,
    src: ObjectRef,
    kind: i32,
    desc: &str,
    owner: Option<ClassId>,
    on: bool,
) -> Option<BoundFieldCarry> {
    if !on {
        return None;
    }
    let instance = match kind {
        MH_KIND_GETTER => !desc.starts_with("()"),
        MH_KIND_SETTER => desc_has_two_params(desc),
        _ => false,
    };
    if !instance || matches!(ctx.get_field(src, MH_BOUND), Value::Object(Some(_))) {
        return None;
    }
    let owner = owner?;
    let link_state = if ctx.object_num_fields(src) >= MH_SLOT_COUNT {
        match ctx.get_field(src, MH_FIELD_LINK) {
            Value::Int(state) => field_link_state_kept_by_copy(state),
            _ => 0,
        }
    } else {
        0
    };
    Some(BoundFieldCarry { owner, link_state })
}

/// Write `carry` onto the fresh bound copy `mh` (current). Width-guarded: a
/// copy too narrow for the link slots keeps the owner alone.
pub(crate) fn mh_write_bound_field_carry(ctx: &dyn NativeContext, mh: ObjectRef, carry: BoundFieldCarry) {
    if ctx.object_num_fields(mh) <= MH_STATIC_OWNER {
        return;
    }
    ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(carry.owner.as_u32() as i32));
    if carry.link_state != 0 && ctx.object_num_fields(mh) >= MH_SLOT_COUNT {
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(carry.link_state));
    }
}

// ---------------------------------------------------------------------------
// A `findSpecial` handle dispatches on its class by id (interpreter round i2
// wave 64, lane L4; `CRATONVM_MH_SPECIAL_BY_OWNER_ID`, first stage of
// `interpreter-i63-L4-proposal-virtual-door-dispatches-by-owner-id-KEPT-OFF-20261224.md`)
// ---------------------------------------------------------------------------
//
// The `SPECIAL` arm handed `MH_CLASS` (the dispatch class's NAME) to
// `invoke_special`, which re-resolved it through the loader-blind global map
// on every call: two loaders' classes of one name ran whichever copy the name
// found (a plugin's `super.m()` handle could run the other copy's body), and
// each call paid a by-name class lookup. `findSpecial` / `unreflectSpecial`
// already hold the classes by id (the `refc` / `Method.clazz` mirror and the
// `specialCaller` mirror); with this switch they record the DISPATCH class's
// id -- `refc`, or the caller's direct superclass when
// [`special_dispatch_class`] restarted the search there -- in
// `MH_STATIC_OWNER`, which no `SPECIAL` path read before (the instance readers
// all exclude the kind: the direct lane, the compiled bind admissions,
// `bindTo`'s owner cast; the leaf receiver cast of this arm is by
// `specialCaller`, unchanged), and the arm calls `invoke_special_by_class_id`,
// the same VM body with the class pre-resolved. A handle without one (switch
// off at mint, a `bindTo` copy, a name the id does not carry) keeps the
// by-name call.

/// `CRATONVM_MH_SPECIAL_BY_OWNER_ID` (opt-in; see the section comment).
pub(crate) fn mh_special_by_owner_id_enabled() -> bool {
    crate::nbflags().mh_special_by_owner_id
}

/// The id of the class a `findSpecial` / `unreflectSpecial` handle dispatches
/// on: `refc_id` when `dispatch_class` is `refc`, else the direct superclass
/// of `caller_id` ([`special_dispatch_class`]'s restart); `None` when that id
/// does not carry the name `dispatch_class` (nothing is guessed). Reads only.
pub(crate) fn special_dispatch_owner(
    ctx: &dyn NativeContext,
    dispatch_class: &str,
    refc: &str,
    refc_id: Option<ClassId>,
    caller_id: Option<ClassId>,
) -> Option<ClassId> {
    if dispatch_class.is_empty() || dispatch_class.starts_with('[') {
        return None;
    }
    let candidate = if dispatch_class == refc {
        refc_id?
    } else {
        ctx.superclass_of(caller_id?)?
    };
    (ctx.class_name_arc_of_id(candidate).as_deref() == Some(dispatch_class)).then_some(candidate)
}

/// Record `owner` on the fresh `SPECIAL` handle `mh` (current), when `on`.
/// Width-guarded.
pub(crate) fn mh_record_special_owner_with(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    owner: Option<ClassId>,
    on: bool,
) {
    let Some(owner) = owner else {
        return;
    };
    if on && ctx.object_num_fields(mh) > MH_STATIC_OWNER {
        ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(owner.as_u32() as i32));
    }
}

/// [`mh_record_special_owner_with`] under the switch.
pub(crate) fn mh_record_special_owner(ctx: &dyn NativeContext, mh: ObjectRef, owner: Option<ClassId>) {
    mh_record_special_owner_with(ctx, mh, owner, mh_special_by_owner_id_enabled());
}

/// The `specialCaller`'s id a `findSpecial` / `unreflectSpecial` handle casts
/// its receiver to (wave 64 orchestrator fix, `L4W64SpecialByOwnerId` row
/// `special-mid-on-mid`): the caller mirror's id, only when the handle is
/// typed by its caller at all (`CRATONVM_MH_FIND_SPECIAL_JDK` and
/// `CRATONVM_MH_SPECIAL_CALLER_CAST`, both default on) and the id carries the
/// caller's name. Reads only.
pub(crate) fn special_caller_owner(
    ctx: &dyn NativeContext,
    caller_name: Option<&str>,
    caller_id: Option<ClassId>,
) -> Option<ClassId> {
    let name = special_receiver(caller_name)?;
    if name.is_empty() || name.starts_with('[') || name == "java/lang/Object" {
        return None;
    }
    if !special_caller_cast_enabled() {
        return None;
    }
    let cid = caller_id?;
    (ctx.class_name_arc_of_id(cid).as_deref() == Some(name)).then_some(cid)
}

/// Record the `specialCaller`'s id on the fresh `SPECIAL` handle `mh`
/// (current), when `on`, in `MH_STATIC_SETTLED`: the slot only a `STATIC`
/// handle's arms settle (every reader is a static path: the `STATIC` arm, the
/// direct lane's and the converting lane's static branches, which admit no
/// `SPECIAL` handle). Width-guarded.
pub(crate) fn mh_record_special_caller_with(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    caller: Option<ClassId>,
    on: bool,
) {
    let Some(caller) = caller else {
        return;
    };
    if on && mh_has_static_settled_slot(ctx, mh) {
        ctx.set_field(mh, MH_STATIC_SETTLED, Value::Int(caller.as_u32() as i32));
    }
}

/// [`mh_record_special_caller_with`] under the switch.
pub(crate) fn mh_record_special_caller(ctx: &dyn NativeContext, mh: ObjectRef, caller: Option<ClassId>) {
    mh_record_special_caller_with(ctx, mh, caller, mh_special_by_owner_id_enabled());
}

/// The `specialCaller` id a `SPECIAL` handle `mh` (current) recorded, when
/// `on`. Callers ask only for `MH_KIND_SPECIAL`. Width-guarded; reads only.
pub(crate) fn mh_special_caller_with(ctx: &dyn NativeContext, mh: ObjectRef, on: bool) -> Option<ClassId> {
    if !on || !mh_has_static_settled_slot(ctx, mh) {
        return None;
    }
    match ctx.get_field(mh, MH_STATIC_SETTLED) {
        Value::Int(raw) if raw != 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

/// The receiver cast of an unbound `SPECIAL` handle by its recorded
/// `specialCaller` id: `Some(None)` admits (the caller or a subclass of it, by
/// id), `Some(Some(e))` refuses with HotSpot's `ClassCastException: Cannot
/// cast X to Caller`, `None` leaves it to the by-name cast (an interface
/// caller, which the id walk does not answer). The by-name cast admitted a
/// receiver whose class name the global map could not resolve uniquely (two
/// loaders' `Sub`): `findSpecial(Mid, who, Sub).invoke(mid)` ran `Mid.who`
/// where HotSpot refuses the `Mid`.
pub(crate) fn special_caller_receiver_verdict(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    caller: ClassId,
) -> Option<Option<MethodCallFailed>> {
    if !ctx.object_is_array(receiver) {
        let rc = ctx.class_id_of_object(receiver);
        if rc == caller || ctx.is_subclass(rc, caller) {
            return Some(None);
        }
    }
    if ctx.is_interface_class(caller) {
        return None;
    }
    Some(Some(owner_cast_failure(ctx, receiver, caller)))
}

/// `CRATONVM_FRAME_TRACE`'s route line for a `SPECIAL` member call (wave 64,
/// host round 2 of `L4W64SpecialByOwnerId`): which door served it and whether
/// it went by the declaring class's id. The variable is read once per process
/// (a debug switch, not compatibility state).
#[inline]
pub(crate) fn note_special_route(route: &str, class: &str, name: &str, desc: &str, by_id: bool) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_FRAME_TRACE").is_some()) {
        note_special_route_line(route, class, name, desc, by_id);
    }
}

#[cold]
#[inline(never)]
pub(crate) fn note_special_route_line(route: &str, class: &str, name: &str, desc: &str, by_id: bool) {
    let how = if by_id { "by-id" } else { "by-name" };
    eprintln!("[mh-special-route] {route} {how} {class}.{name}{desc}");
}

/// The dispatch class a `SPECIAL` handle `mh` (current) recorded, when `on`.
/// Callers ask only for `MH_KIND_SPECIAL`. Width-guarded; reads only.
pub(crate) fn mh_special_owner_with(ctx: &dyn NativeContext, mh: ObjectRef, on: bool) -> Option<ClassId> {
    if !on || ctx.object_num_fields(mh) <= MH_STATIC_OWNER {
        return None;
    }
    match ctx.get_field(mh, MH_STATIC_OWNER) {
        Value::Int(raw) if raw != 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

/// `CRATONVM_MH_BINDTO_STATIC_OWNER` -- default ON (round 15 wave 5, lane mh4);
/// `0`/`false`/`off`/`no` stops `bindTo` carrying a static handle's recorded
/// owner into [`MH_STATIC_OWNER`], so the bound handle resolves its class by
/// name again. Both modes. Asked per `bindTo` of an owner-recording static
/// handle; memoised in a [`cratonvm_types::flags::MemoSlot`] since JIT round
/// 16 wave 11 (lane mhbind): `R16MhFloor bind-static` read the environment
/// once per bind (`CRATONVM_DBG_FLAGREADS=1`: 63k of 200k reads).
pub(crate) fn mh_bindto_static_owner_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_STATIC_OWNER");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_MH_CAST_CHAIN` -- default ON (round 12 wave 7, lane mh5);
/// `0`/`false`/`off`/`no` stops recording [`MH_CAST_CHAIN`] (so nothing is
/// judged through it), keeps `bindTo` minting a virtual handle's `type` from
/// `MH_DESC`, and marks every `explicitCastArguments` copy explicit, exactly
/// as before. Asked when a handle is stamped or bound; memoised in a
/// [`cratonvm_types::flags::MemoSlot`] since JIT round 16 wave 11 (lane
/// mhbind): every `bindTo` asked it (`bound_cast_chain`), and `R16MhFloor
/// bind-each` read the environment once per bind (`CRATONVM_DBG_FLAGREADS=1`:
/// 166k of 200k reads). A test's thread override still applies (the slot is
/// reset while one is installed).
pub(crate) fn cast_chain_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_CAST_CHAIN");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// One entry of [`MH_CAST_CHAIN`]: the type `desc` and whether the conversion
/// into it is `explicitCastArguments`' (JLS 5.5 casts, interfaces uncast)
/// rather than `asType`'s.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CastLink {
    pub(crate) explicit: bool,
    pub(crate) desc: String,
}

/// [`MH_CAST_CHAIN`], parsed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CastChain {
    /// The descriptor a bound value was refused a cast to, if any.
    pub(crate) bound_refusal: Option<String>,
    /// Outermost first; the last is the unstamped handle's own type.
    pub(crate) links: Vec<CastLink>,
}

impl CastChain {
    /// Does [`mh_dispatch`] have anything to judge (see [`MH_MARK_CAST_CHAIN`])?
    pub(crate) fn is_active(&self) -> bool {
        self.links.len() >= 2 || self.bound_refusal.is_some()
    }
}

/// The byte index just past one field-descriptor token starting at `i`
/// (`V` included, for a return), or `None` when `i` does not start one.
pub(crate) fn descriptor_token_end(b: &[u8], mut i: usize) -> Option<usize> {
    while b.get(i) == Some(&b'[') {
        i += 1;
    }
    match b.get(i)? {
        b'L' => {
            let semi = b.get(i..)?.iter().position(|&c| c == b';')?;
            Some(i + semi + 1)
        }
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'V' => Some(i + 1),
        _ => None,
    }
}

/// Parse [`MH_CAST_CHAIN`]'s text; `None` for anything malformed (the caller
/// then judges nothing). Entries need no separator: each method descriptor's
/// end is found by walking its tokens (a class name may contain any of the
/// characters a separator would use).
pub(crate) fn parse_cast_chain(s: &str) -> Option<CastChain> {
    let b = s.as_bytes();
    let mut chain = CastChain::default();
    let mut i = 0;
    if b.first() == Some(&b'!') {
        let end = descriptor_token_end(b, 1)?;
        chain.bound_refusal = Some(s.get(1..end)?.to_string());
        i = end;
    }
    while i < b.len() {
        let explicit = match b[i] {
            b'S' => false,
            b'E' => true,
            _ => return None,
        };
        let start = i + 1;
        if b.get(start) != Some(&b'(') {
            return None;
        }
        let close = start + desc_close_paren(s.get(start..)?)?;
        let end = descriptor_token_end(b, close + 1)?;
        chain.links.push(CastLink {
            explicit,
            desc: s.get(start..end)?.to_string(),
        });
        i = end;
    }
    Some(chain)
}

/// The inverse of [`parse_cast_chain`].
pub(crate) fn render_cast_chain(chain: &CastChain) -> String {
    let mut out = String::new();
    if let Some(target) = &chain.bound_refusal {
        out.push('!');
        out.push_str(target);
    }
    for link in &chain.links {
        out.push(if link.explicit { 'E' } else { 'S' });
        out.push_str(&link.desc);
    }
    out
}

/// [`MH_CAST_CHAIN`]'s text, `None` when the handle is too narrow to carry
/// the slot or holds no chain. Takes no reference and cannot collect.
pub(crate) fn mh_cast_chain_text(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if ctx.object_num_fields(mh) <= MH_CAST_CHAIN {
        return None;
    }
    match ctx.get_field(mh, MH_CAST_CHAIN) {
        Value::Object(Some(s)) => ctx.read_string(s).filter(|t| !t.is_empty()),
        _ => None,
    }
}

/// Store `chain` in [`MH_CAST_CHAIN`] and set or clear
/// [`MH_MARK_CAST_CHAIN`] to match. Allocates the text (a GC point): returns
/// `mh`'s current address. A handle too narrow for the slot is returned
/// untouched.
pub(crate) fn mh_store_cast_chain(ctx: &mut dyn NativeContext, mh: ObjectRef, chain: &CastChain) -> ObjectRef {
    if ctx.object_num_fields(mh) <= MH_CAST_CHAIN {
        return mh;
    }
    let text = render_cast_chain(chain);
    let pin = ctx.pin_native_root(mh);
    let s = ctx.create_string(&text);
    let mh = ctx.read_native_pin(pin, mh);
    ctx.unpin_native_roots(pin);
    ctx.set_field(mh, MH_CAST_CHAIN, Value::Object(Some(s)));
    let bits = mh_marking_bits(&*ctx, mh);
    let bits = if chain.is_active() || adapter_single_stamp_judged(&*ctx, mh, chain) {
        bits | MH_MARK_CAST_CHAIN
    } else {
        bits & !MH_MARK_CAST_CHAIN
    };
    mh_write_marking_bits(&*ctx, mh, bits);
    mh
}

/// Must the ONE stamp over an ADAPTER be judged like a chain (interpreter round
/// i2 wave 58, lane L4; `CRATONVM_MH_ADAPTER_STAMP_CONVERTS`)? A single
/// `asType` / `explicitCastArguments` over a DIRECT handle needs no chain: the
/// doors and the leaf convert from its `type` to `MH_DESC`. An adapter's arm
/// (`FILTER`, `DROP`, `INSERT`, ...) instead hands its arguments, as passed, to
/// its own targets and filters, which expect the adapter's UNSTAMPED type -- so
/// Groovy 5's `explicitCastArguments(filterArguments(plus(long,long), 1,
/// Number.longValue), (Object,int)Object)` passed a raw `int` to
/// `Number.longValue` as its receiver and answered `5 + 0`
/// (`L4W58MixedWidthArguments` `groovy-chain-*`; `def a = 5L; a += 1` is `5`).
/// The one link holds that unstamped type, and [`mh_dispatch`]'s chain judge
/// converts the arguments into it and the result back out.
pub(crate) fn adapter_single_stamp_judged(ctx: &dyn NativeContext, mh: ObjectRef, chain: &CastChain) -> bool {
    chain.links.len() == 1
        && crate::nbflags().mh_adapter_stamp_converts
        && adapter_kind_single_stamp_judged(mh_kind_width_guarded(ctx, mh), chain, true)
}

/// [`adapter_single_stamp_judged`] on a kind, with the switch as a parameter
/// (the unit test asks both answers).
pub(crate) fn adapter_kind_single_stamp_judged(kind: i32, chain: &CastChain, enabled: bool) -> bool {
    enabled
        && chain.links.len() == 1
        && chain.bound_refusal.is_none()
        && kind_has_authoritative_adapted_type(kind)
        && !kind_has_authoritative_type(kind)
}

/// Drop whatever [`MH_CAST_CHAIN`] a copy inherited. Width-guarded; allocates
/// nothing.
pub(crate) fn mh_clear_cast_chain(ctx: &dyn NativeContext, mh: ObjectRef) {
    if ctx.object_num_fields(mh) <= MH_CAST_CHAIN {
        return;
    }
    ctx.set_field(mh, MH_CAST_CHAIN, Value::Object(None));
    mh_write_marking_bits(ctx, mh, mh_marking_bits(ctx, mh) & !MH_MARK_CAST_CHAIN);
}

/// The chain a copy of `this` stamped over its current `type` (`old_desc`)
/// by one `asType` (`explicit == false`) or `explicitCastArguments` carries:
/// `old_desc` in front of `this`'s own chain. `None` -- record nothing --
/// when the switch is off, the handle is too narrow or of a kind whose `type`
/// is not authoritative, `old_desc` is unreadable, `this`'s chain does not
/// parse, or the chain would pass [`MH_CAST_CHAIN_MAX_LINKS`]. Takes no
/// reference and cannot collect.
pub(crate) fn stamped_cast_chain(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    old_desc: Option<&str>,
    explicit: bool,
) -> Option<CastChain> {
    if !cast_chain_enabled() || ctx.object_num_fields(this) <= MH_CAST_CHAIN {
        return None;
    }
    let old_desc = old_desc?;
    split_descriptor_params(old_desc)?;
    let kind = match ctx.get_field(this, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    if !kind_has_authoritative_adapted_type(kind) {
        return None;
    }
    let mut chain = match mh_cast_chain_text(&*ctx, this) {
        Some(text) => parse_cast_chain(&text)?,
        None => CastChain::default(),
    };
    if chain.links.len() >= MH_CAST_CHAIN_MAX_LINKS {
        return None;
    }
    chain.links.insert(
        0,
        CastLink {
            explicit,
            desc: old_desc.to_string(),
        },
    );
    Some(chain)
}

/// A value on its way through the links of a [`CastChain`], as
/// [`cast_chain_step`] follows it. `Boxed(p, raw)` is the wrapper a
/// primitive -> reference link made of the primitive `p` (never allocated:
/// its class is all a later link asks); `Opaque` is a value the judge cannot
/// follow (a raw primitive typed as a reference, a `null` or a non-wrapper
/// typed as a primitive, anything past a `void`) -- nothing further is judged
/// for it, and the leaf's own conversion decides it as before.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ChainValue {
    Plain(Value),
    Boxed(&'static str, Value),
    Opaque,
}

/// A primitive descriptor token as a `'static` string, or `None`.
pub(crate) fn static_primitive_desc(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "Z" => DESC_BOOLEAN,
        "B" => DESC_BYTE,
        "C" => DESC_CHAR,
        "S" => DESC_SHORT,
        "I" => DESC_INT,
        "J" => DESC_LONG,
        "F" => DESC_FLOAT,
        "D" => DESC_DOUBLE,
        _ => return None,
    })
}

/// The raw value of `v`, which stands where the primitive `p` is typed: the
/// value itself, or a wrapper object (the `invokeWithArguments` door and the
/// adapters pass primitives boxed) whose primitive widens to `p`, unboxed and
/// widened. `None` for anything else (a `null`, a non-wrapper, a wrapper that
/// does not widen): the leaf's conversion owns those.
pub(crate) fn chain_primitive(ctx: &dyn NativeContext, v: ChainValue, p: &str) -> Option<Value> {
    match v {
        ChainValue::Plain(Value::Object(Some(obj))) => {
            if ctx.object_is_array(obj) {
                return None;
            }
            let wrapper = format!(
                "L{};",
                ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))?
            );
            let src = primitive_desc_for_wrapper(&wrapper)?;
            if !primitive_widens_to(src, p) {
                return None;
            }
            Some(widen_primitive_to_descriptor(ctx.get_field(obj, 0), p))
        }
        ChainValue::Plain(Value::Object(None)) | ChainValue::Boxed(..) | ChainValue::Opaque => None,
        ChainValue::Plain(raw) => Some(raw),
    }
}

/// One link of a [`CastChain`] for one value statically typed `from`, on its
/// way to `to`: `asType`'s conversion (`explicit == false`) or
/// `explicitCastArguments`' (`MethodHandleImpl.valueConversion`, `strict` /
/// `!strict`), as far as it can FAIL:
///
/// | from -> to | `asType` | `explicitCastArguments` |
/// |---|---|---|
/// | ref -> ref | checked cast | checked cast, an interface target uncast |
/// | ref -> prim | unbox + widen; `null` NPE, other wrapper / non-wrapper CCE | unbox + JLS 5.5 cast; `null` is zero, `Boolean` 1/0; non-wrapper CCE |
/// | prim -> prim | widen | JLS 5.5 cast; to `boolean` the low bit of `(byte) x` |
/// | prim -> ref | box, then the cast | box, then the cast (interfaces uncast) |
///
/// A primitive -> reference box's class is the source primitive's wrapper,
/// whose supertypes are a closed table ([`reference_accepts_wrapper`]), so its
/// cast is decided without a lookup. Every refusal is HotSpot's class and
/// wording. Everything this cannot follow becomes [`ChainValue::Opaque`] and
/// is never refused.
pub(crate) fn cast_chain_step(
    ctx: &mut dyn NativeContext,
    v: ChainValue,
    from: &str,
    to: &str,
    explicit: bool,
) -> Result<ChainValue, MethodCallFailed> {
    if from == to || v == ChainValue::Opaque {
        return Ok(v);
    }
    if from == DESC_VOID || to == DESC_VOID {
        // A dropped return, or a zero introduced from `void`: nothing past it.
        return Ok(ChainValue::Opaque);
    }
    match (static_primitive_desc(from), static_primitive_desc(to)) {
        (None, None) => match v {
            ChainValue::Plain(value @ Value::Object(Some(_))) => {
                match leaf_reference_refusal(ctx, value, to, explicit) {
                    Some(refusal) => Err(refusal),
                    None => Ok(v),
                }
            }
            ChainValue::Plain(Value::Object(None)) => Ok(v),
            ChainValue::Boxed(p, raw) => boxed_reference_cast(ctx, p, raw, to, explicit),
            _ => Ok(ChainValue::Opaque),
        },
        (Some(p), None) => match chain_primitive(&*ctx, v, p) {
            Some(raw) => boxed_reference_cast(ctx, p, raw, to, explicit),
            None => Ok(ChainValue::Opaque),
        },
        (None, Some(q)) => match v {
            ChainValue::Boxed(p, raw) => {
                if explicit {
                    Ok(ChainValue::Plain(explicit_cast_primitive(raw, q)))
                } else if primitive_widens_to(p, q) {
                    Ok(ChainValue::Plain(widen_primitive_to_descriptor(raw, q)))
                } else {
                    let (Some(from_w), Some(to_w)) =
                        (wrapper_desc_for_primitive(p), wrapper_desc_for_primitive(q))
                    else {
                        return Ok(ChainValue::Opaque);
                    };
                    Err(RuntimeError::ClassCastException {
                        message: format!(
                            "Cannot cast {} to {}",
                            mh_class_get_name(from_w),
                            mh_class_get_name(to_w)
                        ),
                    }
                    .into())
                }
            }
            ChainValue::Plain(value @ Value::Object(_)) => {
                match adapt_leaf_arg(ctx, value, q, None, explicit)? {
                    // A class this context cannot name: kept as passed.
                    Value::Object(_) => Ok(ChainValue::Opaque),
                    raw => Ok(ChainValue::Plain(raw)),
                }
            }
            ChainValue::Plain(raw) => Ok(ChainValue::Plain(if explicit {
                explicit_cast_primitive(raw, q)
            } else {
                widen_primitive_to_descriptor(raw, q)
            })),
            ChainValue::Opaque => Ok(v),
        },
        (Some(p), Some(q)) => match chain_primitive(&*ctx, v, p) {
            Some(raw) => Ok(ChainValue::Plain(if explicit {
                explicit_cast_primitive(raw, q)
            } else {
                widen_primitive_to_descriptor(raw, q)
            })),
            None => Ok(ChainValue::Opaque),
        },
    }
}

/// The reference cast of a box of the primitive `p` (value `raw`) to `to`.
pub(crate) fn boxed_reference_cast(
    ctx: &mut dyn NativeContext,
    p: &'static str,
    raw: Value,
    to: &str,
    explicit: bool,
) -> Result<ChainValue, MethodCallFailed> {
    let Some(wrapper) = wrapper_desc_for_primitive(p) else {
        return Ok(ChainValue::Opaque);
    };
    if reference_accepts_wrapper(to, wrapper)
        || (explicit && descriptor_names_an_interface(&*ctx, to))
    {
        return Ok(ChainValue::Boxed(p, raw));
    }
    Err(RuntimeError::ClassCastException {
        message: format!(
            "Cannot cast {} to {}",
            mh_class_get_name(wrapper),
            mh_class_get_name(to)
        ),
    }
    .into())
}

/// Judge `extra` -- the arguments a dispatch of `mh` received, typed by its
/// `type` (`outer`) -- through every link of `chain`, and a bound value's
/// recorded refusal. `Err` is the first refusal, in link order; a list that
/// does not line up with the types (a collector's arity, a fabricated type)
/// is not judged.
///
/// `Ok(Some(values))` when the links CHANGED a primitive the unstamped handle
/// takes (an explicit middle link's narrowing: `twiceLong.asType((int)long)
/// .explicitCastArguments((long)long)` hands `twiceLong` `(long)(int) x`),
/// with that position's converted value and every other as passed; `Ok(None)`
/// when the arguments go on as passed. Only a raw primitive is ever
/// substituted, so nothing is allocated. A value the links re-box (an
/// `Object -> int -> Object` round trip) goes on as passed: see the page
/// named at [`MH_CAST_CHAIN`].
#[cfg(test)]
pub(crate) fn mh_cast_chain_judge_args(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    chain: &CastChain,
    outer: Option<&str>,
    extra: &[Value],
) -> Result<Option<Vec<Value>>, MethodCallFailed> {
    mh_cast_chain_judge_args_rebox(ctx, mh, chain, outer, extra).map(|(values, _)| values)
}

/// A position whose value the links re-boxed (a primitive middle link under a
/// reference parameter of the unstamped handle): `(index, primitive, raw)`.
/// HotSpot's adapter hands the target `<Wrapper>.valueOf(raw)`, not the
/// caller's object (round 13 wave 1, lane mhffm).
pub(crate) type ChainRebox = (usize, &'static str, Value);

/// [`mh_cast_chain_judge_args`], also answering the positions the links
/// re-box ([`ChainRebox`]); the caller allocates those boxes (see
/// [`mh_cast_chain_rebox_args`]).
pub(crate) fn mh_cast_chain_judge_args_rebox(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    chain: &CastChain,
    outer: Option<&str>,
    extra: &[Value],
) -> Result<(Option<Vec<Value>>, Vec<ChainRebox>), MethodCallFailed> {
    if let Some(target) = chain.bound_refusal.as_deref() {
        let bound = ctx.get_field(mh, MH_BOUND);
        if matches!(bound, Value::Object(Some(_))) {
            return Err(reference_cast_failure(ctx, bound, target));
        }
    }
    let Some((mut from, _)) = outer.and_then(split_descriptor_params) else {
        return Ok((None, Vec::new()));
    };
    if from.len() != extra.len() {
        return Ok((None, Vec::new()));
    }
    let mut values: Vec<ChainValue> = extra.iter().map(|v| ChainValue::Plain(*v)).collect();
    for link in &chain.links {
        let Some((to, _)) = split_descriptor_params(&link.desc) else {
            return Ok((None, Vec::new()));
        };
        if to.len() != from.len() {
            return Ok((None, Vec::new()));
        }
        for (i, value) in values.iter_mut().enumerate() {
            *value = cast_chain_step(ctx, *value, &from[i], &to[i], link.explicit)?;
        }
        from = to;
    }
    // `from` is now the unstamped handle's own parameter list.
    let mut changed = false;
    let mut reboxes: Vec<ChainRebox> = Vec::new();
    let out: Vec<Value> = values
        .iter()
        .zip(from.iter())
        .zip(extra.iter())
        .enumerate()
        .map(|(i, ((value, ty), passed))| match value {
            ChainValue::Plain(raw)
                if !matches!(raw, Value::Object(_)) && static_primitive_desc(ty).is_some() =>
            {
                changed |= raw != passed;
                *raw
            }
            ChainValue::Boxed(p, raw) if static_primitive_desc(ty).is_none() => {
                reboxes.push((i, *p, *raw));
                *passed
            }
            _ => *passed,
        })
        .collect();
    Ok((changed.then_some(out), reboxes))
}

/// `CRATONVM_MH_CAST_CHAIN_CONVERTS` -- default ON (round 13 wave 1, lane
/// mhffm, both modes): the values a stacked `asType` / `explicitCastArguments`
/// chain CHANGES reach the target and the caller -- an argument a primitive
/// middle link re-boxes is handed on as `<Wrapper>.valueOf(raw)`, and a
/// return a middle link converts is delivered converted
/// (`r12w7-mh5-cast-chain-judges-but-does-not-convert` residuals 1 and 2).
/// `0`: the chain only judges, as in round 12.
pub(crate) fn cast_chain_converts() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_CAST_CHAIN_CONVERTS")
    })
}

/// Allocate the boxes [`mh_cast_chain_judge_args_rebox`] asked for into
/// `values` (the arguments as they go on), keeping `mh` and every reference
/// argument pinned across the allocations. Answers `mh`'s and the values'
/// current addresses.
pub(crate) fn mh_cast_chain_rebox_args(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    values: Vec<Value>,
    reboxes: &[ChainRebox],
) -> (ObjectRef, Vec<Value>) {
    let mh_pin = ctx.pin_native_root(mh);
    let (_arg_base, handles) = pin_mh_args(ctx, &values);
    let mut boxed: Vec<(usize, usize, Value)> = Vec::with_capacity(reboxes.len());
    for &(index, primitive, raw) in reboxes {
        let value = box_value_canonical(ctx, raw, primitive);
        let pin = pin_mh_value(ctx, value);
        boxed.push((index, pin, value));
    }
    let mut out = read_pinned_mh_args(&*ctx, &handles, &values);
    for (index, pin, value) in boxed {
        if let Some(slot) = out.get_mut(index) {
            *slot = read_pinned_mh_arg(&*ctx, pin, value);
        }
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    (mh, out)
}

/// Judge a dispatch's `result` back out through `chain`: from the unstamped
/// type's return, through every middle type's, to `outer`'s. The result is
/// returned unchanged unless a link refuses it.
pub(crate) fn mh_cast_chain_judge_return(
    ctx: &mut dyn NativeContext,
    chain: &CastChain,
    outer: Option<&str>,
    result: MethodCallResult,
) -> MethodCallResult {
    let value = match &result {
        Ok(Some(v)) => *v,
        _ => return result,
    };
    let (Some(outer), Some(last)) = (outer, chain.links.last()) else {
        return result;
    };
    let mut v = ChainValue::Plain(value);
    let mut from = return_type_desc(&last.desc).to_string();
    for idx in (0..chain.links.len()).rev() {
        let to = match idx.checked_sub(1).and_then(|above| chain.links.get(above)) {
            Some(above) => return_type_desc(&above.desc),
            None => return_type_desc(outer),
        };
        v = match cast_chain_step(ctx, v, &from, to, chain.links[idx].explicit) {
            Ok(next) => next,
            Err(refusal) => return Err(refusal),
        };
        from = to.to_string();
    }
    // Round 13 wave 1 (lane mhffm): deliver what the links made of the value
    // (`cast_chain_converts`). A raw primitive only where it keeps the leaf's
    // `Value` variant, because the doors box a primitive result against the
    // LEAF's return type; a re-box whatever the leaf answered, since an object
    // passes the doors' boxing untouched.
    let outer_ret = return_type_desc(outer);
    let leaf_ret = return_type_desc(&last.desc);
    match v {
        ChainValue::Plain(raw)
            if !matches!(raw, Value::Object(_))
                && raw != value
                && std::mem::discriminant(&raw) == std::mem::discriminant(&value)
                && static_primitive_desc(outer_ret).is_some()
                && static_primitive_desc(leaf_ret).is_some()
                && cast_chain_converts() =>
        {
            Ok(Some(raw))
        }
        ChainValue::Boxed(p, raw)
            if static_primitive_desc(outer_ret).is_none() && cast_chain_converts() =>
        {
            Ok(Some(box_value_canonical(ctx, raw, p)))
        }
        // Round 13 wave 3 (lane ffm2): the outer return has another `Value`
        // variant than the leaf's (`retLong.explicitCastArguments(()int)
        // .asType(()double)`). Delivered in the LEAF's variant, chosen so the
        // doors' own leaf -> outer conversion lands exactly on what the links
        // made; a value that has no such spelling keeps the old answer.
        ChainValue::Plain(raw)
            if !matches!(raw, Value::Object(_))
                && !matches!(value, Value::Object(_))
                && std::mem::discriminant(&raw) != std::mem::discriminant(&value)
                && static_primitive_desc(outer_ret).is_some()
                && static_primitive_desc(leaf_ret).is_some()
                && cast_chain_converts()
                && cast_chain_cross_variant() =>
        {
            // Unchanged when the doors' direct conversion already lands there
            // (no middle link converted anything), so every such result keeps
            // its exact leaf value.
            if door_return_conversion(value, leaf_ret, outer_ret)
                .is_some_and(|direct| same_primitive_bits(direct, raw))
            {
                return result;
            }
            match chain_return_in_leaf_variant(raw, leaf_ret, outer_ret) {
                Some(leaf_value) => Ok(Some(leaf_value)),
                None => result,
            }
        }
        _ => result,
    }
}

/// `CRATONVM_MH_CAST_CHAIN_CROSS_VARIANT` -- default ON (round 13 wave 3, lane
/// ffm2, both modes): [`mh_cast_chain_judge_return`] delivers a middle-converted
/// primitive return whose outer type has another `Value` variant than the
/// leaf's (`r12w7-mh5-cast-chain-judges-but-does-not-convert`, "Still open"
/// item 1). `0`: such a return goes on unconverted, as in round 13 wave 1.
pub(crate) fn cast_chain_cross_variant() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_CAST_CHAIN_CROSS_VARIANT")
    })
}

/// `converted` (what a cast chain made of a primitive return, typed by the
/// OUTER return `outer_ret`) respelled in the `Value` variant of the LEAF's
/// return `leaf_ret`, such that the doors' leaf -> outer conversion
/// ([`widen_return_value`] or [`narrow_return_value`], what
/// `box_return_against_target` applies) gives back exactly `converted`.
///
/// Every consumer of `mh_dispatch`'s result keys its boxing or conversion on
/// the leaf's return type, so a value in the outer variant cannot be handed
/// back raw (a `Double` boxed as a `Long`). The respelling is verified by
/// running the doors' own conversion on it and comparing bit for bit; `None`
/// when no leaf-typed value maps to `converted` (e.g. `(double)(float)
/// Integer.MAX_VALUE`, which no `int` widens to) or when the candidate falls
/// outside a sub-int leaf type's range, and the caller keeps the unconverted
/// result.
pub(crate) fn chain_return_in_leaf_variant(
    converted: Value,
    leaf_ret: &str,
    outer_ret: &str,
) -> Option<Value> {
    let leaf = leaf_ret.as_bytes().first().copied()?;
    let outer = outer_ret.as_bytes().first().copied()?;
    let candidate = match leaf {
        b'I' | b'S' | b'B' | b'C' | b'Z' => {
            let i = match converted {
                Value::Long(l) => l as i32,
                Value::Float(f) => f as i32,
                Value::Double(d) => d as i32,
                _ => return None,
            };
            let in_range = match leaf {
                b'B' => i8::try_from(i).is_ok(),
                b'S' => i16::try_from(i).is_ok(),
                b'C' => u16::try_from(i).is_ok(),
                b'Z' => i == 0 || i == 1,
                _ => true,
            };
            if !in_range {
                return None;
            }
            Value::Int(i)
        }
        b'J' => Value::Long(match converted {
            Value::Int(i) => i64::from(i),
            Value::Float(f) => f as i64,
            Value::Double(d) => d as i64,
            _ => return None,
        }),
        b'F' => Value::Float(match converted {
            Value::Int(i) => i as f32,
            Value::Long(l) => l as f32,
            Value::Double(d) => d as f32,
            _ => return None,
        }),
        b'D' => Value::Double(match converted {
            Value::Int(i) => f64::from(i),
            Value::Long(l) => l as f64,
            Value::Float(f) => f64::from(f),
            _ => return None,
        }),
        _ => return None,
    };
    let back = widen_return_value(candidate, leaf, outer)
        .or_else(|| narrow_return_value(candidate, leaf, outer))?;
    same_primitive_bits(back, converted).then_some(candidate)
}

/// The conversion `box_return_against_target` applies to a leaf-typed
/// primitive result on its way to the outer return type, or `None` when it
/// applies none.
pub(crate) fn door_return_conversion(value: Value, leaf_ret: &str, outer_ret: &str) -> Option<Value> {
    let leaf = leaf_ret.as_bytes().first().copied()?;
    let outer = outer_ret.as_bytes().first().copied()?;
    widen_return_value(value, leaf, outer).or_else(|| narrow_return_value(value, leaf, outer))
}

/// Two primitive `Value`s of one variant with the same bits (a NaN equals
/// itself; `0.0` and `-0.0` differ).
pub(crate) fn same_primitive_bits(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        _ => false,
    }
}

/// The chain `bindTo(recv)` carries from the stamped handle `this` (kind
/// `kind`) to the handle it mints: every link minus its leading parameter,
/// and, when `recv` fails the cast one of those links makes of it (the outer
/// cast, to `this`'s `type`, is `bindTo`'s own and was judged already), that
/// link's type as the bound refusal. `None` when there is nothing to carry.
/// Only a REFERENCE cast is recorded; a primitive leading type's conversion
/// (a `null` bound through an `Integer` to an `int`) is left to the leaf,
/// which converts a bound value like any other. Takes no reference and
/// cannot collect.
pub(crate) fn bound_cast_chain(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    recv: Value,
    kind: i32,
) -> Option<CastChain> {
    if !cast_chain_enabled() || !kind_has_authoritative_type(kind) {
        return None;
    }
    let chain = parse_cast_chain(&mh_cast_chain_text(&*ctx, this)?)?;
    let outer = mh_declared_descriptor(ctx, this)?;
    let (outer_params, _) = split_descriptor_params(&outer)?;
    let mut from = outer_params.first()?.clone();
    let mut refusal = chain.bound_refusal.clone();
    if refusal.is_none() {
        let mut v = ChainValue::Plain(recv);
        for link in &chain.links {
            let Some(to) = split_descriptor_params(&link.desc)
                .and_then(|(params, _)| params.into_iter().next())
            else {
                break;
            };
            match cast_chain_step(ctx, v, &from, &to, link.explicit) {
                Ok(next) => v = next,
                Err(_) => {
                    if to.starts_with('L') || to.starts_with('[') {
                        refusal = Some(to);
                    }
                    break;
                }
            }
            from = to;
        }
    }
    let links = chain
        .links
        .iter()
        .map(|link| {
            descriptor_without_first_param(&link.desc).map(|desc| CastLink {
                explicit: link.explicit,
                desc,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CastChain {
        bound_refusal: refusal,
        links,
    })
}

/// `CRATONVM_MH_VARARGS_ARRAY_TYPE` -- default ON (JIT round 16, lane mh;
/// MH5-2, backlog MH16-B1; a correctness fix, both modes):
/// `asVarargsCollector(arrayType)` over a handle whose trailing parameter is
/// not `arrayType` answers a collector of type `type.changeParameterType(last,
/// arrayType)`, as `MethodHandleImpl.makeVarargsCollector` does (it `asType`s
/// the target first): `two(Object,Object)Object.asVarargsCollector(String[]
/// .class).type()` is `(Object,String[])Object` on HotSpot 25 (`R16MhErrors`
/// `collect-str-arr`), and the collected array is a `String[]`
/// ([`mh_collector_declared_array_desc`]). `0`/`false`/`off`/`no` keeps the
/// receiver's type and collects into the parameter's own type, as before.
/// Read on `asVarargsCollector` and on a collector's entry (cold-ish paths).
pub(crate) fn mh_varargs_array_type_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_VARARGS_ARRAY_TYPE")
}

/// `asVarargsCollector`'s answer for a receiver whose trailing parameter is
/// not the requested array type: the receiver `asType`d to `new_desc` (a stamped
/// copy, [`mh_with_stamped_type`]: a reference cast at the trailing position)
/// and marked a collector. `None` when the type cannot be built (the caller then
/// marks a plain copy, as before).
pub(crate) fn mh_as_varargs_collector_retyped(
    ctx: &mut dyn NativeContext,
    receiver: Option<Value>,
    new_desc: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(target))) = receiver else {
        return None;
    };
    // The build allocates: the receiver is rooted across it.
    let pin = ctx.pin_native_root(target);
    let built = build_method_type_from_descriptor(ctx, new_desc);
    let target = ctx.read_native_pin(pin, target);
    ctx.unpin_native_roots(pin);
    let new_mt = match built {
        Ok(Some(mt)) => mt,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    let stamped = match mh_with_stamped_type(ctx, Value::Object(Some(target)), new_mt, false) {
        Ok(Some(Value::Object(Some(h)))) => h,
        Ok(_) => return None,
        Err(e) => return Some(Err(e)),
    };
    mh_set_varargs_collector(&*ctx, stamped, true);
    Some(Ok(Some(Value::Object(Some(stamped)))))
}

/// For a collector `mh` whose DECLARED trailing parameter is an array type
/// other than its leaf's (`target_desc`, `MH_DESC`) trailing reference
/// parameter -- what [`mh_as_varargs_collector_retyped`] mints -- the leaf
/// descriptor with that array in the trailing position, which
/// [`mh_entry_adapt`] collects against (`R16MhErrors` `collect-str-arr`: the
/// array HotSpot hands the target is a `String[]`). `None` for every other
/// handle, or with [`mh_varargs_array_type_enabled`] off. Reads only (the
/// declared render).
pub(crate) fn mh_collector_declared_array_desc(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    target_desc: &str,
) -> Option<String> {
    if !mh_varargs_array_type_enabled() {
        return None;
    }
    let declared = mh_declared_descriptor(ctx, mh)?;
    let (dp, _) = split_descriptor_params(&declared)?;
    let (lp, lr) = split_descriptor_params(target_desc)?;
    // The same trailing position: equal arity, or a virtual handle's declared
    // type one longer (its receiver).
    if lp.is_empty() || !(dp.len() == lp.len() || dp.len() == lp.len() + 1) {
        return None;
    }
    let array = dp.last()?;
    let leaf_last = lp.last()?;
    if !array.starts_with('[') || array == leaf_last {
        return None;
    }
    if !(leaf_last.starts_with('L') || leaf_last.starts_with('[')) {
        return None;
    }
    Some(format!("({}{}){}", lp[..lp.len() - 1].concat(), array, lr))
}

/// `asVarargsCollector(t)` / `asFixedArity()` — a COPY of the receiver carrying
/// the requested marking, leaving the receiver untouched.
///
/// Falls back to marking the receiver in place when [`mh_clone_handle`] cannot
/// own it, which is the pre-2026-08-21 behaviour for exactly those handles.
pub(crate) fn mh_with_varargs_marking(
    ctx: &mut dyn NativeContext,
    receiver: Option<Value>,
    on: bool,
) -> MethodCallResult {
    let this = match receiver {
        Some(Value::Object(Some(o))) => o,
        other => return Ok(Some(other.unwrap_or(Value::Object(None)))),
    };
    let handle = match mh_clone_handle(ctx, this) {
        Some(copy) => copy,
        None => this,
    };
    mh_set_varargs_collector(ctx, handle, on);
    Ok(Some(Value::Object(Some(handle))))
}

/// `asType(t)` / `explicitCastArguments(h, t)` — a COPY of the receiver
/// carrying the requested `type`, leaving the receiver untouched.
///
/// Both are PURE on every JDK, and both used to stamp the new type onto the
/// RECEIVER and hand it back. That is not a cosmetic identity difference: two
/// `asType` calls off one handle overwrote each other, so the FIRST result
/// silently acquired the second's type (`probes/MhIdentityProbe.java` rows
/// I36-I39, where `q.asType(A)` reported `A` until `q.asType(B)` ran and then
/// reported `B`). It also made the refusal predicate read a type its own
/// previous call had installed — the deviation `mh_astype_refusal`'s own doc
/// records.
///
/// `newType == type` returns the RECEIVER, because that is the JDK's own first
/// line (`if (newType == type) return this;`) and because it keeps the common
/// no-op case allocation-free. Measured as I34/I35.
///
/// The copy carries the varargs marking with it. Since round 15 wave 6 (lane
/// mh5, MH4-1), while `CRATONVM_MH_ASTYPE_FIXED_COLLECTOR` is on, `asType`
/// of a collector answers the JDK's fixed-arity composition before reaching
/// here ([`mh_astype_varargs_collector`], whose `asCollector` adapter is what
/// collects: `probes/MhVarargsNullProbe.java` D01/D02), and
/// `explicitCastArguments` clears the marking on this function's copy. With
/// the switch off the copy still collects at dispatch, as before.
///
/// `explicit`: the stamp is `explicitCastArguments`' (round 12 wave 7 moved
/// the marking here from the shim). The copy records the receiver's `type` in
/// front of the receiver's own [`MH_CAST_CHAIN`] (`stamped_cast_chain`), so a
/// stamp over a stamp keeps the middle type. A FIRST stamp's explicit cast is
/// the conversion into the unstamped handle and marks the copy
/// ([`MH_MARK_EXPLICIT_CAST`], which the leaf reads); a later stamp's is a
/// middle link's, recorded on that link, and the copy keeps the receiver's
/// marking. With nothing recorded (`CRATONVM_MH_CAST_CHAIN=0`, a kind whose
/// `type` is not authoritative) an explicit stamp marks the copy, as before.
pub(crate) fn mh_with_stamped_type(
    ctx: &mut dyn NativeContext,
    receiver: Value,
    new_type: ObjectRef,
    explicit: bool,
) -> MethodCallResult {
    let this = match receiver {
        Value::Object(Some(o)) => o,
        other => return Ok(Some(other)),
    };
    // Compare Class mirrors, never their lossy descriptor rendering.
    let old_type = match ctx.get_field_by_name(this, "type") {
        Value::Object(Some(old)) => Some(old),
        _ => None,
    };
    let old_desc = old_type.and_then(|old| methodtype_to_descriptor(ctx, old));
    let same = old_type.is_some_and(|old| method_types_same_classes(&*ctx, old, new_type));
    if same {
        return Ok(Some(Value::Object(Some(this))));
    }
    // Read before the clone allocates (it takes no reference).
    let chain = stamped_cast_chain(ctx, this, old_desc.as_deref(), explicit);
    // `new_type` is read after the clone's allocation: rooted across it.
    let type_pin = ctx.pin_native_root(new_type);
    let (handle, copied) = match mh_clone_handle(ctx, this) {
        Some(copy) => (copy, true),
        None => (this, false),
    };
    let new_type = ctx.read_native_pin(type_pin, new_type);
    ctx.unpin_native_roots(type_pin);
    ctx.set_field_by_name(handle, "type", Value::Object(Some(new_type)));
    if !copied {
        // Too narrow to copy: stamped in place, unmarked, as before.
        return Ok(Some(Value::Object(Some(handle))));
    }
    let handle = match chain {
        Some(chain) => {
            if explicit && chain.links.len() == 1 {
                mh_mark_explicit_cast(ctx, handle);
            }
            mh_store_cast_chain(ctx, handle, &chain)
        }
        None => {
            // Whatever the copy inherited describes the receiver's type, not
            // this one's.
            mh_clear_cast_chain(ctx, handle);
            if explicit {
                mh_mark_explicit_cast(ctx, handle);
            }
            handle
        }
    };
    Ok(Some(Value::Object(Some(handle))))
}

/// `CRATONVM_MH_ASTYPE_FIXED_COLLECTOR` -- default ON (round 15 wave 6, lane
/// mh5; proposal MH4-1, `r15w5-mh4-review-findings-FIXED-20260930.md` item 1).
/// `0`/`false`/`off`/`no` restores the old `asType` of a variable-arity handle
/// (a stamped COPY that still collects at dispatch, by runtime arity) and its
/// `explicitCastArguments` (the same), and the old "cannot convert" wording
/// for a collector asked for too few parameters. Both modes. Read only for a
/// receiver marked a collector.
pub(crate) fn mh_astype_fixed_collector_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_ASTYPE_FIXED_COLLECTOR")
}

/// `asType(newType)` of a VARIABLE-ARITY receiver, as
/// `MethodHandleImpl.AsVarargsCollector.asTypeUncached` answers it, or `None`
/// for every other receiver and every way of not knowing (the caller then runs
/// the ordinary refusal and stamp). Round 15 wave 6, lane mh5 (MH4-1).
///
/// The JDK collects ONCE, at `asType`, and answers a FIXED-arity handle:
///
/// * the same arity, with a trailing parameter the collector's array type is
///   assignable from: `asFixedArity().asType(newType)`;
/// * any other arity `>= collectArg` (or a trailing type that is no such
///   array): `asFixedArity().asCollector(arrayType, newArity - collectArg)
///   .asType(newType)`.
///
/// The stamped copy this model made instead kept the collector marking, so it
/// answered `isVarargsCollector() == true` and collected any OTHER arity at
/// dispatch (`R15Mh4AdapterArityMore` rows `astype-fixed` / `-spread`: HotSpot
/// `false` and `WrongMethodTypeException`). Both steps are composed from this
/// model's own pieces, exactly as the JDK composes them: the copy with the
/// marking cleared ([`mh_clone_handle`], what `asFixedArity` answers), the
/// registered `asCollector` native (an `MH_KIND_COLLECT` adapter, so the
/// collection the old copy made at dispatch is recorded in the handle), then
/// the ordinary `asType` refusal and stamp of that handle, whose `type` now
/// says the collected arity: a collected element that does not convert to the
/// component is the refusal HotSpot's inner `asType` raises
/// (`cannot convert MethodHandle(String,String,String)String to
/// (String,int,int)String`), at `asType` rather than at the call. An arity
/// below `collectArg` is left to [`mh_astype_refusal_desc`] ("cannot build
/// collector"). `newType` equal to the receiver's type is the receiver itself.
pub(crate) fn mh_astype_varargs_collector(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallResult> {
    if !mh_is_varargs_collector(&*ctx, this) || !mh_astype_fixed_collector_enabled() {
        return None;
    }
    let old_mt = match ctx.get_field_by_name(this, "type") {
        Value::Object(Some(mt)) => mt,
        _ => return None,
    };
    let old_desc = methodtype_to_descriptor(ctx, old_mt)?;
    let new_desc = methodtype_to_descriptor(ctx, new_type)?;
    if old_desc == new_desc {
        return None;
    }
    let (old_params, _) = split_descriptor_params(&old_desc)?;
    let (new_params, _) = split_descriptor_params(&new_desc)?;
    let array_desc = old_params.last().filter(|p| p.starts_with('['))?;
    let collect_arg = old_params.len() - 1;
    let new_arity = new_params.len();
    if new_arity < collect_arg {
        return None;
    }
    // `type.parameterType(collectArg).isAssignableFrom(newType
    // .parameterType(collectArg))`; a pair that cannot be told keeps the old
    // path rather than guess.
    let passthrough = new_arity == collect_arg + 1
        && mh_array_assignable_to(&*ctx, &new_params[collect_arg], array_desc)?;
    // `arrayType` is the receiver's own trailing parameter class.
    let mirror = if passthrough {
        None
    } else {
        let Value::Object(Some(ptypes)) = ctx.get_field_by_name(old_mt, "ptypes") else {
            return None;
        };
        match ctx.get_array_element(ptypes, collect_arg) {
            Value::Object(Some(m)) => Some(m),
            _ => return None,
        }
    };
    // Every reference above was read before this function's first
    // allocation (the copy); `newType` and the mirror are rooted across it
    // and the `asCollector` call. `type_pin` (lowest) releases both.
    let type_pin = ctx.pin_native_root(new_type);
    let mirror = mirror.map(|m| (ctx.pin_native_root(m), m));
    let Some(fixed) = mh_clone_handle(ctx, this) else {
        ctx.unpin_native_roots(type_pin);
        return None;
    };
    mh_set_varargs_collector(ctx, fixed, false);
    let base = match mirror {
        None => Ok(fixed),
        Some((mirror_pin, mirror)) => {
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.invoke_virtual(
                fixed,
                "asCollector",
                MH_AS_COLLECTOR_DESC,
                &[
                    Value::Object(Some(mirror)),
                    Value::Int((new_arity - collect_arg) as i32),
                ],
            )
            .and_then(|answer| mh_handle_answer(answer, "asCollector"))
        }
    };
    let new_type = ctx.read_native_pin(type_pin, new_type);
    ctx.unpin_native_roots(type_pin);
    let base = match base {
        Ok(base) => base,
        Err(e) => return Some(Err(e)),
    };
    // `.asType(newType)` of the fixed-arity handle: the registered `asType`
    // native's own two steps (neither allocates before its refusal is final;
    // `mh_with_stamped_type` roots what it reads across its copy).
    if let Some(refusal) = mh_astype_refusal(ctx, base, new_type) {
        return Some(Err(refusal));
    }
    Some(mh_with_stamped_type(
        ctx,
        Value::Object(Some(base)),
        new_type,
        false,
    ))
}

/// Copy a synthetic `MethodHandle` slot for slot, or `None` when the receiver
/// is not one of ours.
///
/// `asFixedArity()` and `asVarargsCollector()` are PURE on every JDK: each
/// returns a NEW handle (`MethodHandleImpl$AsVarargsCollector` wrapping the
/// target, or the wrapped target itself) and leaves the receiver exactly as it
/// was. CratonVM models a handle as ONE mutable object, so both used to flip
/// the marking on the RECEIVER and hand it back — which means one library
/// calling `h.asFixedArity()` silently changed how every OTHER holder of `h`
/// dispatched it. Spring's `FunctionReference.executeFunctionViaMethodHandle`
/// re-reads `methodHandle.isVarargsCollector()` on every evaluation of the same
/// long-lived registered handle, so that is not a hypothetical: measured as
/// rows I03/I04/I09/I10/I28 of `probes/MhIdentityProbe.java`, where a handle
/// stopped collecting its arguments after an unrelated `asFixedArity()`.
///
/// Width is the ownership test. A handle narrower than the synthetic layout is
/// a real-JDK one, or `panama.rs`'s compact downcall layout whose slot 0 is a
/// native address — copying either would be a guess about slots this file does
/// not own, so the caller keeps the old in-place behaviour for them.
///
/// `asType` is deliberately NOT converted: it stamps the new type in place on
/// every hot MethodHandle path and several shims (`explicitCastArguments`,
/// `mh_astype_refusal`) are written against that. Its impurity stays a recorded
/// deviation — `probes/MhIdentityProbe.java` rows I13/I14.
pub(crate) fn mh_clone_handle(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let n = ctx.object_num_fields(this);
    if n <= MH_VARARGS {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    // `alloc_object` can collect and relocate the receiver; the copy loop below
    // allocates nothing, so one pin around the allocation is enough.
    let this_pin = ctx.pin_native_root(this);
    let copy = ctx.alloc_object(cid, n);
    let this = ctx.read_native_pin(this_pin, this);
    for i in 0..n {
        let v = ctx.get_field(this, i);
        ctx.set_field(copy, i, v);
    }
    ctx.unpin_native_roots(this_pin);
    // The copy is another handle; the source's link does not describe it.
    mh_clear_field_link(&*ctx, copy);
    // Per bit, as `MH_MARKS` says (every bit is inherited today, so this
    // stores what the loop copied).
    mh_write_marking_bits(
        &*ctx,
        copy,
        mh_marking_bits(&*ctx, copy) & MH_MARKS_INHERITED,
    );
    Some(copy)
}

thread_local! {
    /// Armed for exactly one [`mh_dispatch`] — the one entered from
    /// `MethodHandle.invokeWithArguments`, the GENERIC door.
    ///
    /// # Why the door matters
    ///
    /// A `null` sitting in a varargs-collector's trailing array slot is
    /// ambiguous: it can BE the array, or it can be one element the collector
    /// must wrap. The JDK decides from the CALL SITE's static type, in
    /// `MethodHandleImpl$AsVarargsCollector.asType` — the "pass it straight
    /// through" shortcut is taken only when the caller's trailing parameter
    /// type is assignable to the collector's array type. So:
    ///
    /// ```text
    ///   mh.invokeExact((String[]) null)   ->  Arrays.toString(null)      null
    ///   mh.invokeWithArguments(nullArg)   ->  Arrays.toString({null})    [null]
    /// ```
    ///
    /// because `invokeWithArguments` always adapts to `genericMethodType(n)`,
    /// whose trailing parameter is `Object` and is never assignable to
    /// `String[]`. CratonVM derives varargs behaviour from the RUNTIME values
    /// at dispatch (see [`collect_trailing_varargs`]), and a null carries no
    /// runtime type — so the door it came through is the only signal left.
    /// Measured on HotSpot 25 in `probes/MhVarargsNullProbe.java`, C01/C02
    /// against B01; Spring's SpEL `#varargsFunctionHandle(null)`
    /// (`VariableAndFunctionTests`) is the reported victim.
    ///
    /// # Why it is TAKEN rather than scoped
    ///
    /// Only the OUTERMOST handle is adapted to the generic type; an adapter's
    /// inner target is invoked at its own exact type. [`mh_dispatch`] clears
    /// the flag on entry, so the recursive dispatches an adapter arm makes see
    /// `false` — which is what HotSpot does for e.g.
    /// `filterArguments(collector, 0, f).invokeWithArguments(null)`.
    pub(crate) static MH_ENTRY_CALL_SITE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

thread_local! {
    /// The type of the ENCLOSING adapter, set by an adapter that hands its own
    /// arguments to an inner handle unchanged (`guardWithTest`). A collector
    /// (`asCollector`) running inside it boxes a raw `Z` / `C` / `B` / `S`
    /// operand by this type: the operand is still a `Value::Int`, and the inner
    /// dispatch has no call site of its own to ask. Consulted only when no
    /// entry call site is armed.
    pub(crate) static MH_ADAPTER_SITE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn swap_adapter_site(site: Option<String>) -> Option<String> {
    MH_ADAPTER_SITE.with(|s| std::mem::replace(&mut *s.borrow_mut(), site))
}

pub(crate) fn current_adapter_site() -> Option<String> {
    MH_ADAPTER_SITE.with(|s| s.borrow().clone())
}

/// Arm [`MH_ENTRY_CALL_SITE`] for the next [`mh_dispatch`] on this thread.
/// Call it IMMEDIATELY before the dispatch — anything in between that itself
/// dispatches a handle would consume it.
///
/// `descriptor` is the type the JDK would `asType` to: `genericMethodType(m)`
/// for `invokeWithArguments`, and the literal call-site descriptor for
/// `invoke` (see `cratonvm_native_api::poly_call_site`). For a virtual or
/// special handle it is the RECEIVER-STRIPPED form, because that is the shape
/// `mh_dispatch` compares it against.
pub(crate) fn arm_entry_call_site(descriptor: &str) {
    MH_ENTRY_CALL_SITE.with(|f| *f.borrow_mut() = Some(descriptor.to_string()));
}

/// Read and clear [`MH_ENTRY_CALL_SITE`].
pub(crate) fn take_entry_call_site() -> Option<String> {
    MH_ENTRY_CALL_SITE.with(|f| f.borrow_mut().take())
}

/// `genericMethodType(m)` as a descriptor — what `invokeWithArguments` always
/// adapts to, and the reason a `null` is COLLECTED on that door: its trailing
/// parameter is `Object`, which no array type is ever assignable from.
pub(crate) fn generic_method_type_descriptor(m: usize) -> String {
    let mut s = String::with_capacity(2 + m * 18 + 18);
    s.push('(');
    for _ in 0..m {
        s.push_str(DESC_OBJECT);
    }
    s.push(')');
    s.push_str(DESC_OBJECT);
    s
}

/// Drop a descriptor's FIRST parameter — the receiver, for a virtual or
/// special handle whose call site names it but whose `MH_DESC` does not.
pub(crate) fn descriptor_without_first_param(desc: &str) -> Option<String> {
    let close = desc_close_paren(desc)?;
    // `parse_descriptor_param_and_return`, NOT `parse_descriptor_types`: the
    // latter unwraps a class type to its NAME (`java/lang/String`), so
    // rejoining its output produces `(java/lang/String)V` — a string that is
    // not a descriptor and that every comparison downstream then reads wrong.
    let (params, _) = crate::lang_class::parse_descriptor_param_and_return(desc);
    if params.is_empty() {
        return None;
    }
    Some(format!("({}){}", params[1..].concat(), &desc[close + 1..]))
}

/// Render one JVM type descriptor the way a `ClassCastException` message does:
/// a class as its dotted binary name, an array as its descriptor with dots.
///
/// MEASURED on HotSpot 25 (`probes/MhVarargsNullProbe.java` G02/H03):
/// `Cannot cast [Ljava.lang.String; to java.lang.String`.
pub(crate) fn jvm_type_display(desc: &str) -> String {
    if let Some(inner) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        return inner.hidden_dotted();
    }
    desc.hidden_dotted()
}

/// May `value` be cast to the reference type `desc`?
///
/// FAILS OPEN. Every "no" produced here becomes a `ClassCastException` a
/// caller did not get before, so the only ones worth producing are the ones
/// the class hierarchy positively contradicts — an unloaded target, an
/// interface, a class this context cannot resolve, all answer `true`. Same
/// rule, and the same reason, as `NativeContext::aastore_element_assignable`'s
/// "must never produce a FALSE ArrayStoreException".
pub(crate) fn entry_value_casts_to(ctx: &dyn NativeContext, value: ObjectRef, desc: &str) -> bool {
    if desc == DESC_OBJECT {
        return true;
    }
    // ARRAY-NESS COMES FROM THE HEAP, NOT FROM THE CLASS NAME. A CratonVM array
    // object's `class_id_of_object` resolves to its COMPONENT class, so a
    // `String[]` reports `java/lang/String` here — reading array-ness off that
    // name inverted every array row at once: `fa.invokeWithArguments(new
    // String[]{"x"})` threw `Cannot cast java.lang.String to
    // [Ljava.lang.String;` for an argument that IS a `String[]`, and the
    // already-packed `String[]` of B06 sailed through the component check it
    // was supposed to fail.
    let have_is_array = ctx.object_is_array(value);
    let want_is_array = desc.starts_with('[');
    if have_is_array != want_is_array {
        // An array is only ever castable to another array (or to `Object`,
        // handled above), and a non-array is never castable to one. The shape
        // settles both without asking the hierarchy.
        return false;
    }
    if want_is_array {
        // Covariance is `checkcast`'s array rule (wave 60, lane L4: this
        // failed open for every array, so `invoke` handed an `Integer[]` to a
        // `String[]` parameter); "cannot tell" still admits.
        return reference_array_target_admitted(ctx, value, desc) != Some(false);
    }
    let want = match desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        Some(inner) => inner,
        None => return true,
    };
    let have = match ctx.class_name_of_id(ctx.class_id_of_object(value)) {
        Some(n) => n,
        None => return true,
    };
    if have == want {
        return true;
    }
    match (ctx.class_id_by_name(&have), ctx.class_id_by_name(want)) {
        (Some(h), Some(w)) => ctx.is_subclass(h, w),
        _ => true,
    }
}

/// Does the CALL SITE name the collector's trailing array type, so that the
/// JDK's `AsVarargsCollector.asType` takes its passthrough shortcut?
///
/// The shortcut's condition is `arity matches AND
/// arrayType.isAssignableFrom(newType.parameterType(collectArg))`. Measured
/// rows: G01/G06/G09 (exact type -> passthrough), G03/G04 (`Object[]` param,
/// `String[]` site -> still passthrough, because the assignability is real),
/// G02/G05/G07 (`Object` site -> collect).
pub(crate) fn call_site_names_trailing_array(call_site: Option<&str>, target_desc: &str) -> bool {
    let cs = match call_site {
        Some(cs) => cs,
        None => return false,
    };
    let (target_params, _) = crate::lang_class::parse_descriptor_param_and_return(target_desc);
    let (site_params, _) = crate::lang_class::parse_descriptor_param_and_return(cs);
    if target_params.len() != site_params.len() || target_params.is_empty() {
        return false;
    }
    let want = &target_params[target_params.len() - 1];
    let have = &site_params[site_params.len() - 1];
    if want == have {
        return true;
    }
    if !want.starts_with('[') || !have.starts_with('[') {
        return false;
    }
    // `Object[]` is assignable from every reference array.
    want[1..].starts_with('L') && &want[1..] == DESC_OBJECT
}

/// The `WrongMethodTypeException` an entry door raises when the supplied
/// arity cannot be adapted to the handle at all.
pub(crate) fn entry_wrong_method_type(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    call_site: &str,
) -> MethodCallFailed {
    let old_desc = match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt),
        _ => mh_read_desc(ctx, mh),
    };
    let old_shown = old_desc
        .as_deref()
        .and_then(|d| method_type_display_in(&*ctx, d))
        .unwrap_or_else(|| "(?)?".to_string());
    let new_shown =
        method_type_display_in(&*ctx, call_site).unwrap_or_else(|| call_site.to_string());
    mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{old_shown} to {new_shown}"),
    )
}

/// The `ClassCastException` an entry door's cast raises.
pub(crate) fn entry_class_cast(ctx: &mut dyn NativeContext, value: ObjectRef, want: &str) -> MethodCallFailed {
    let have = entry_value_type_display(ctx, value);
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/ClassCastException",
        &format!("Cannot cast {have} to {}", jvm_type_display(want)),
    )
}

/// The name a `ClassCastException` gives the VALUE's class.
///
/// An array object's `class_id_of_object` resolves to its COMPONENT class here
/// — measured: a `String[]` reported `java.lang.String`, so the message read
/// `Cannot cast java.lang.String to java.lang.String` — so the array's own name
/// has to be rebuilt around it. The eight primitive component letters are the
/// JVMS §4.3.2 descriptor spelling, not an inference; the REFERENCE shape is
/// the one measured against HotSpot (`probes/MhVarargsNullProbe.java` B06/G02:
/// `Cannot cast [Ljava.lang.String; to java.lang.String`).
pub(crate) fn entry_value_type_display(ctx: &dyn NativeContext, value: ObjectRef) -> String {
    let base = ctx
        .class_name_of_id(ctx.class_id_of_object(value))
        .unwrap_or_else(|| "java/lang/Object".to_string());
    if !ctx.object_is_array(value) {
        return base.class_text(&*ctx);
    }
    // Every array is named from the heap's own element-kind read, as
    // `aastore`'s message does: a multi-dimensional array's header names its
    // component, itself an array class, so reading the header was one
    // dimension short (wave 24); and a PRIMITIVE array's header does not name
    // `int` at all -- an `int[]` read `java/lang/Object` there and was
    // reported as `[Ljava.lang.Object;` (wave 25 follow-up:
    // `arrayElementSetter(int[].class).invoke(arr, 0, (Object) new int[0])`
    // printed `class [Ljava.lang.Object; cannot be cast to class
    // java.lang.Number` where HotSpot prints `class [I ...`,
    // `tools/probes/interp/L4/L4W25PrimitiveArraySetter.java`). A reference
    // array's answer is unchanged: `[L<component>;`, or `[Ljava.lang.Object;`
    // when the header names no component.
    cratonvm_native_api::array_store::aastore_value_external_name(ctx, value)
}

/// One argument of an entry-door adaptation: reference casts are CHECKED,
/// primitives are left to `adapt_invoke_args`/`build_varargs_array` downstream.
pub(crate) fn entry_cast_check(
    ctx: &mut dyn NativeContext,
    value: Value,
    want: &str,
) -> Result<(), MethodCallFailed> {
    if !matches!(want.as_bytes().first(), Some(b'L') | Some(b'[')) {
        return Ok(());
    }
    let obj = match value {
        // `null` casts to every reference type, which is exactly why the null
        // rows of this family are about COLLECTION and not about casting.
        Value::Object(Some(o)) => o,
        _ => return Ok(()),
    };
    if entry_value_casts_to(ctx, obj, want) {
        return Ok(());
    }
    // Beneath an `explicitCastArguments` stamp a reference reaches an
    // INTERFACE parameter uncast (round 12 wave 7, lane mh5; the leaf arms
    // skip it under the same scope since wave 27): `explicitCastArguments(
    // takesRunnable, (Object)int).invoke((Object) "s")` runs the target on
    // HotSpot and was refused here.
    if mh_explicit_scope()
        && descriptor_names_an_interface(&*ctx, want)
        && explicit_interface_uncast_enabled()
    {
        return Ok(());
    }
    Err(entry_class_cast(ctx, obj, want))
}

/// `asType(callSiteType)` — the step every `MethodHandle.invoke` and
/// `invokeWithArguments` performs before the invocation itself, applied to
/// CratonVM's argument list.
///
/// # Why this is a second function and not a flag on `collect_trailing_varargs`
///
/// That one is a REPAIR: it reshapes an argument list that arrived flat, and it
/// is deliberately permissive because it also serves adapter chains and
/// internal Rust callers whose arity it cannot vouch for (JRuby's
/// `insertArguments`-built call sites are the case it was written for). This
/// one is a CONTRACT: on a door where the call-site type is known, the JDK
/// either adapts exactly or throws, and the difference between the two is
/// visible — a fixed-arity handle handed two arguments is a
/// `WrongMethodTypeException` on every JDK and was a silently gathered array
/// here (`probes/MhVarargsNullProbe.java` H02/H10, `MhIdentityProbe` I06).
///
/// Applied only to MH_KIND_STATIC and MH_KIND_CONSTRUCTOR, and only when the
/// static handle has no bound receiver — the kinds where `MH_DESC` is exactly
/// the parameter list the caller supplies, so the arity comparison means what
/// it says. Every other kind keeps the permissive path.
pub(crate) fn mh_entry_adapt(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    call_site: &str,
    target_desc: &str,
    is_collector: bool,
    params: &[Value],
) -> Result<Vec<Value>, MethodCallFailed> {
    // Round 16 (lane mh, MH5-2): a collector retyped by `asVarargsCollector`
    // collects into its declared array type.
    let retyped = if is_collector {
        mh_collector_declared_array_desc(ctx, mh, target_desc)
    } else {
        None
    };
    let target_desc = retyped.as_deref().unwrap_or(target_desc);
    let (ptypes, _) = crate::lang_class::parse_descriptor_param_and_return(target_desc);
    let n = ptypes.len();
    let m = params.len();
    let trailing_array = n >= 1 && ptypes[n - 1].starts_with('[');

    if is_collector && trailing_array {
        // "The caller must supply, at a minimum, N-1 arguments, where N is the
        // arity of the target" — MethodHandle.asVarargsCollector's javadoc.
        if m + 1 < n {
            return Err(entry_wrong_method_type(ctx, mh, call_site));
        }
        if m == n && call_site_names_trailing_array(Some(call_site), target_desc) {
            for (i, p) in params.iter().enumerate() {
                entry_cast_check(ctx, *p, &ptypes[i])?;
            }
            return Ok(params.to_vec());
        }
        for i in 0..n - 1 {
            entry_cast_check(ctx, params[i], &ptypes[i])?;
        }
        let component = ptypes[n - 1][1..].to_string();
        for p in &params[n - 1..] {
            entry_collect_element_check(ctx, *p, &component)?;
        }
        // Round 15 wave 6 (lane mh5): the array's allocation can relocate the
        // LEADING arguments, which were pushed into the answer before it and
        // handed to the leaf at their old addresses. The checks above
        // allocate only to throw, so the leading arguments are pinned across
        // the build alone and read back through their pins.
        let (pin_base, handles) = pin_mh_args(ctx, &params[..n - 1]);
        let arr = build_varargs_array(ctx, &component, &params[n - 1..], Some(call_site), n - 1);
        let mut out = read_pinned_mh_args(ctx, &handles, &params[..n - 1]);
        if pin_base != usize::MAX {
            ctx.unpin_native_roots(pin_base);
        }
        out.push(Value::Object(arr));
        return Ok(out);
    }

    if m != n {
        return Err(entry_wrong_method_type(ctx, mh, call_site));
    }
    for (i, p) in params.iter().enumerate() {
        entry_cast_check(ctx, *p, &ptypes[i])?;
    }
    Ok(params.to_vec())
}

/// One element on its way into a collector's array.
///
/// A reference component is the ordinary cast check. A PRIMITIVE component
/// refuses `null`, which the JDK reports through the unboxing call it was
/// about to make — MEASURED for `int` (`probes/MhVarargsNullProbe.java` G08)
/// and transcribed:
///
/// ```text
/// Cannot invoke "java.lang.Number.intValue()" because the return value of
/// "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
/// ```
///
/// Only the six `Number` primitives get that text. `char` and `boolean` unbox
/// through `Character`/`Boolean` rather than `Number`, their message was not
/// measured, and inventing one would be a guess — they keep the old coercion.
pub(crate) fn entry_collect_element_check(
    ctx: &mut dyn NativeContext,
    value: Value,
    component: &str,
) -> Result<(), MethodCallFailed> {
    let numeric = match component {
        "I" => "intValue",
        "J" => "longValue",
        "F" => "floatValue",
        "D" => "doubleValue",
        "S" => "shortValue",
        "B" => "byteValue",
        _ => return entry_cast_check(ctx, value, component),
    };
    if matches!(value, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some(format!(
                "Cannot invoke \"java.lang.Number.{numeric}()\" because the return value of \
                 \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, \
                 Object, boolean)\" is null"
            )),
        }
        .into());
    }
    Ok(())
}

/// Mint one of the `__mh_*_wrapper__` combinator carriers that `MH_BOUND`
/// points at.
///
/// Ten of them exist (`insert`, `collect`, `collect_args`, `spread`, `fold`,
/// `filter`, `retfilter`, `catch`, `permute`, `guard`) and not one is a
/// stand-in for anything. Each is a 2- or 3-slot tuple holding the state one
/// `MethodHandles` combinator captured — the target handle, a filter/guard
/// handle or `MethodHandle[]`, and an `int` position or count — so the matching
/// `MH_KIND_*` arm of `mh_dispatch` can apply the combinator at invoke time.
/// No native is registered on any of these names, no bytecode ever names one,
/// and no entry in the JDK 25 module image declares one (checked with `javap`
/// and against the full `jimage list`, 2026-08-11).
///
/// # Why this is not `try_alloc_concurrent_synthetic`
///
/// That funnel is the **compatibility stand-in** door: it asks
/// `try_ensure_synthetic_class`, which stamps `ClassOrigin::CompatibilityStub`,
/// and a stand-in is the one thing `--jdk-only` forbids. Every carrier site
/// used it, so a strict run recorded ten `compatibility-class-requested`
/// violations reading *"VM-requested stand-in: ensure_synthetic_class called
/// with no class file on any classpath entry"* and then threw
/// `NoClassDefFoundError: __mh_insert_wrapper__` out of
/// `MethodHandles.insertArguments`. The `NoClassDefFoundError` is only the
/// FIRST carrier the workload reaches, never the only one: a probe that reaches
/// each combinator independently, catching per step, named all ten on one run
/// (2026-08-11) while `dropArguments` and `asVarargsCollector` — the two whose
/// state is a single reference and which therefore need no carrier at all —
/// passed.
///
/// The classification was simply wrong, and the census says so in its own
/// `reason` string: there is no class file for these names on any classpath
/// because there is no class. Contract §1 item 6 makes a class the VM creates
/// without any class file legitimate in **both** modes, and
/// `ensure_vm_internal_class` is its door — the same one
/// `vm_exec::heap_alloc_object` takes for `cratonvm/synthetic/AnonymousObject$N`
/// on word-for-word this reasoning ("a VM bookkeeping type, not a compatibility
/// substitution"). That door is demonstrably open in strict mode rather than
/// merely declared to be: a `--jdk-only` run of the shipped binary under
/// `CRATONVM_DBG_ANONALLOC=1` minted 16 `AnonymousObject$N` and its census
/// recorded a violation for none of them.
///
/// This is not a way around the policy. The question the trait declaration
/// poses is whether the JVM specification says a class file must exist for the
/// name; for a carrier CratonVM invented to hold its own combinator state, it
/// does not.
///
/// # What each mode sees
///
/// * `JdkOnly` — the refusal disappears and the ten violations leave the
///   census. This is the whole change.
/// * `Compatible` — unchanged; that mode fabricates through either door. The
///   two second-order differences both run the safe way: `fabricate_class`
///   stops running a full-classpath rescan per carrier looking for real bytes
///   that cannot exist, and the carriers stop being counted against a
///   zero-stub census they were never evidence for.
///
/// Infallible because `ensure_vm_internal_class` is — the door that never
/// refuses is the point — so the call sites drop the `?` they carried for a
/// refusal that was never theirs to propagate.
#[track_caller]
pub(crate) fn alloc_mh_carrier(ctx: &mut dyn NativeContext, name: &str, num_fields: usize) -> ObjectRef {
    let cid = ctx.ensure_vm_internal_class(name, num_fields);
    // The width clamp is `try_alloc_concurrent_synthetic`'s, kept verbatim: if
    // the resolved class declares MORE slots than this site asks for,
    // allocating the smaller number leaves every carrier write past the
    // requested width silently discarded. The carriers have no
    // `synthetic_stub_fields` arm, so the class declares 0 and `max` is the
    // caller's own number — the same arithmetic the old funnel performed for
    // them, kept rather than simplified away because the day someone adds that
    // arm is the day dropping it becomes a truncating write.
    let n = num_fields.max(ctx.class_num_total_fields(cid));
    ctx.try_alloc_object_gc_safe(cid, n)
        .unwrap_or_else(|| ctx.alloc_object(cid, n))
}

/// [`alloc_mh_carrier`] with `slots.len()` slots, slot `i` holding `slots[i]`.
///
/// gc-common w20-f: every combinator that minted a carrier this way stored the
/// handles it was handed (targets, filters, cleanup and loop handles, the
/// argument arrays) AFTER the carrier's allocation, at their addresses from
/// before it -- a collection there left the carrier naming dead or moved
/// objects, which the adapter then dispatched through. The references are
/// rooted across the allocation here and stored at their current addresses.
#[track_caller]
pub(crate) fn alloc_mh_carrier_holding(ctx: &mut dyn NativeContext, name: &str, slots: &[Value]) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let handles: Vec<Option<cratonvm_native_api::NativeHandle>> = slots
        .iter()
        .map(|v| match *v {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        })
        .collect();
    let wrapper: ObjectRef = alloc_mh_carrier(&mut *scope, name, slots.len());
    for (i, (v, h)) in slots.iter().zip(handles.iter()).enumerate() {
        let v = match h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => *v,
        };
        scope.set_field(wrapper, i, v);
    }
    wrapper
}

/// Returns true if a method descriptor has exactly two parameters.
/// Used to distinguish instance setters "(Lowner;value)V" (2 params)
/// from static setters "(value)V" (1 param).
pub(crate) fn desc_has_two_params(desc: &str) -> bool {
    let end = match desc_close_paren(desc) {
        Some(i) => i,
        None => return false,
    };
    let params = desc.get(1..end).unwrap_or("");
    let mut count = 0;
    let mut i = 0;
    let bytes = params.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => {
                count += 1;
                i += 1;
            }
            '[' => {
                // skip array prefix
                while i < bytes.len() && bytes[i] as char == '[' {
                    i += 1;
                }
                // then one type
                if i < bytes.len() {
                    if bytes[i] as char == 'L' {
                        while i < bytes.len() && bytes[i] as char != ';' {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                count += 1;
            }
            'L' => {
                while i < bytes.len() && bytes[i] as char != ';' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                count += 1;
            }
            _ => return false,
        }
    }
    count == 2
}

pub(crate) fn serialization_hook_neutral_result(
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<Option<Value>> {
    match (method_name, descriptor) {
        ("readObject", "(Ljava/io/ObjectInputStream;)V")
        | ("readObjectNoData", "()V")
        | ("writeObject", "(Ljava/io/ObjectOutputStream;)V") => Some(None),
        ("readResolve", "()Ljava/lang/Object;") | ("writeReplace", "()Ljava/lang/Object;") => {
            Some(Some(args.first().copied().unwrap_or(Value::Object(None))))
        }
        _ => None,
    }
}

/// `CRATONVM_MH_NEUTRAL_HOOK_CURRENT_RECEIVER` -- default ON (round 15 wave 1,
/// lane mh; `r15w1-mh-neutral-serialization-hook-answers-a-stale-receiver-FIXED-20260930.md`);
/// `0`/`false`/`off`/`no` makes `mh_dispatch_body`'s `SPECIAL` and `VIRTUAL`
/// arms hand [`neutralize_missing_serialization_hook`] the receiver as it was
/// BEFORE the leaf call again, which a collection inside the call leaves
/// stale. With it on, the receiver is re-read from its pin after the call.
/// Read once.
pub(crate) fn mh_neutral_hook_current_receiver_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_NEUTRAL_HOOK_CURRENT_RECEIVER")
    })
}

pub(crate) fn neutralize_missing_serialization_hook(
    result: MethodCallResult,
    args: &[Value],
) -> MethodCallResult {
    match result {
        Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchMethodError {
                class_name,
                method_name,
                method_descriptor,
            },
        ))) => {
            if let Some(result) =
                serialization_hook_neutral_result(&method_name, &method_descriptor, args)
            {
                if crate::nbflags().dbg_reflection_factory {
                    eprintln!(
                        "[rf-ser] neutral MethodHandle missing hook {}.{}{}",
                        class_name, method_name, method_descriptor
                    );
                }
                Ok(result)
            } else {
                Err(MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError {
                        class_name,
                        method_name,
                        method_descriptor,
                    },
                )))
            }
        }
        other => other,
    }
}

pub(crate) const MH_KIND_STATIC: i32 = 0;
pub(crate) const MH_KIND_VIRTUAL: i32 = 1;
pub(crate) const MH_KIND_SPECIAL: i32 = 2;
pub(crate) const MH_KIND_CONSTRUCTOR: i32 = 3;
#[allow(dead_code)]
pub(crate) const MH_KIND_GETTER: i32 = 4;
#[allow(dead_code)]
pub(crate) const MH_KIND_SETTER: i32 = 5;
pub(crate) const MH_KIND_PERMUTE: i32 = 6;
pub(crate) const MH_KIND_GUARD: i32 = 7;
/// C26: dropArgumentsTrusted adapter. MH_BOUND holds the wrapped inner MH.
/// MH_DESC is the widened descriptor (used for invokeExact arity check).
/// Dispatch reads the inner MH's MH_DESC and forwards a trimmed argument
/// slice (outer-arity minus inner-arity extras are discarded from the
/// position encoded in MH_CLASS as a decimal pos string).
pub(crate) const MH_KIND_DROP: i32 = 8;
/// Round-9 perf: StringConcatFactory CallSite target. MH_CLASS holds the
/// concatenation recipe string (`\u{0001}` argument placeholder,
/// `\u{0002}` constant placeholder), MH_BOUND is a synthetic 1-field
/// holder whose field 0 is the Object[] of constants in recipe order.
/// `extra_args` to mh_dispatch are the dynamic call-site arguments.
/// Returns a `java/lang/String` ObjectRef. See `p58_make_concat*`.
pub(crate) const MH_KIND_STRING_CONCAT: i32 = 9;
/// Lambda factory target produced by a *reflective* `LambdaMetafactory`
/// call (log4j2 `ServiceLoaderUtil`, ES CLI bootstrap, etc.). MH_CLASS holds
/// the synthetic lambda-proxy `ClassId` as a decimal string; MH_DESC holds the
/// factory descriptor (`(captures...)FunctionalInterface`). MH_BOUND holds an
/// `Object[]` of captures accumulated via `bindTo` (the single-slot MH_BOUND
/// used by the other kinds cannot represent a multi-capture factory). When
/// invoked, the factory gathers captures (`bound[]` ++ invoke args), allocates
/// a proxy instance of the proxy class, and returns it — the interpreter's
/// SAM dispatch then routes the proxy's abstract method to the impl handle.
pub(crate) const MH_KIND_LAMBDA_FACTORY: i32 = 10;
/// Record-deserialization constructor produced by our intercept of
/// `java.io.ObjectStreamClass$RecordSupport.deserializationCtr(ObjectStreamClass)`.
/// `MH_CLASS` holds the record class's internal name; `MH_BOUND` holds the
/// `ObjectStreamClass` describing the stream layout. When `invokeExact(byte[]
/// primValues, Object[] objValues)` is called by `ObjectInputStream.readRecord`,
/// the dispatch arm reflectively maps each canonical record component (by name)
/// to its slot in `primValues`/`objValues` and invokes the canonical
/// constructor — bypassing the real `MethodHandles.foldArguments`/
/// `insertArguments`/`arrayElementGetter` combinator chain, which CratonVM's
/// synthetic MethodHandles cannot execute (see
/// `register_array_element_accessor_bridges`).
pub(crate) const MH_KIND_RECORD_DESER: i32 = 11;
/// Constant method handle produced by `MethodHandles.constant(type, value)`.
/// A nullary handle that ignores all arguments and returns a captured value.
/// `MH_BOUND` holds the (boxed) constant; `MH_DESC` is `()<type>` so the
/// dispatch arm can coerce the boxed value to a primitive return type.
///
/// CratonVM needs this because in real-JDK mode `MethodHandles.constant`
/// runs genuine JDK bytecode (`MethodHandleImpl.makeConstantReturning` →
/// `LambdaForm.createConstantForm` → `BoundMethodHandle.<clinit>` →
/// `ClassSpecializer` runtime *species* class generation). The VM models
/// method handles with these `MH_KIND_*` shims instead of the real
/// `BoundMethodHandle`/`LambdaForm` machinery, so that bytecode path dies
/// generating a species class. `SwitchPoint.<clinit>` is the first thing
/// Apache Groovy's `IndyInterface.<clinit>` triggers (it builds `K_true`/
/// `K_false` via `MethodHandles.constant(boolean.class, …)`), so the failure
/// breaks EVERY Groovy `invokedynamic` call site. Shimming `constant`
/// keeps that init off the real species path entirely.
pub(crate) const MH_KIND_CONSTANT: i32 = 12;
/// The `MH_NAME` of `CallSite.makeUninitializedCallSite`'s `MH_KIND_CONSTANT`
/// placeholder (its `MH_CLASS` is empty), whose invocation throws
/// `IllegalStateException: uninitialized call site` (wave 47).
pub(crate) const UNINITIALIZED_CALL_SITE_NAME: &str = "uninit";
/// Identity method handle produced by `MethodHandles.identity(type)` — a
/// handle of type `(type)type` that returns its single argument unchanged.
/// `MH_DESC` is `(<type>)<type>`. After `bindTo(x)` the argument is captured
/// in `MH_BOUND`, so the dispatch arm returns that. In real-JDK mode
/// `identity` runs genuine JDK bytecode that yields a
/// `MethodHandleImpl$IntrinsicMethodHandle` (and for some types a
/// `BoundMethodHandle` species) — a real handle whose layout the `MH_KIND_*`
/// shims cannot read (slots 16–19 out of bounds), so `identity().invoke()` /
/// `.bindTo()` fail. Shimming keeps it inside the synthetic model.
pub(crate) const MH_KIND_IDENTITY: i32 = 13;
/// Dynamic-invoker handle produced by `CallSite.dynamicInvoker()`
/// (concrete on `MutableCallSite`/`VolatileCallSite`). `MH_BOUND` holds the
/// call site; on invocation the dispatch arm reads the site's CURRENT
/// `target` and delegates to it. This shim exists to avoid the real
/// `CallSite.makeDynamicInvoker` → `MethodHandle.bindArgumentL` →
/// `BoundMethodHandle` species path, which hangs on CratonVM.
/// `SwitchPoint.<init>` calls `mcs.dynamicInvoker()`, so without this every
/// `new SwitchPoint()` (and therefore Groovy's `IndyInterface.<clinit>`)
/// hangs.
pub(crate) const MH_KIND_DYNAMIC_INVOKER: i32 = 14;
/// Argument-insertion adapter produced by `MethodHandles.insertArguments(
/// target, pos, values…)`. `MH_BOUND` holds a 3-field wrapper:
/// field 0 = target MH, field 1 = the bound `Object[] values`, field 2 = pos.
/// On invocation the bound values are spliced into the incoming args at `pos`
/// and the target is dispatched. In real-JDK mode the genuine
/// `insertArguments` builds a `BoundMethodHandle` species (unimplemented), and
/// the old synthetic stub silently dropped the bound values — both wrong for
/// Groovy, whose `IndyInterface` fallback binds the call site / metadata into
/// its dispatch handle via `insertArguments`.
pub(crate) const MH_KIND_INSERT: i32 = 15;
/// Argument-collector adapter produced by `MethodHandle.asCollector(
/// arrayType, count)`. `MH_BOUND` holds a 2-field wrapper: field 0 = target
/// MH, field 1 = `count`. On invocation the trailing `count` arguments are
/// collected into a fresh `Object[]` and appended to the leading args before
/// the target is dispatched. Groovy's `IndyInterface` fallback uses
/// `asCollector(Object[].class, paramCount)` to turn the call site's spread
/// arguments into the `Object[]` its `selectMethod`/`make` dispatcher expects.
pub(crate) const MH_KIND_COLLECT: i32 = 16;
/// Argument-spreader adapter produced by `MethodHandle.asSpreader(arrayType,
/// count)` — the inverse of `asCollector`. `MH_BOUND` holds a 2-field wrapper
/// (field 0 = target MH, field 1 = count). On invocation the trailing array
/// argument is spread into its elements before the target is dispatched.
/// Groovy's `IndyInterface` dispatch chains use `asSpreader` to turn an
/// `Object[]` back into positional arguments for the resolved method.
pub(crate) const MH_KIND_SPREAD: i32 = 17;

/// Argument-filter adapter produced by `MethodHandles.filterArguments(target,
/// pos, filters...)`. `MH_BOUND` holds a 3-field wrapper (field 0 = target MH,
/// field 1 = filters `MethodHandle[]`, field 2 = pos). On invocation each
/// non-null filter is applied to the argument at `pos + i` (replacing it with
/// `filter.invoke(arg)`) before the target is dispatched. Groovy's
/// `TypeTransformers` uses this to coerce a `Closure` argument into a SAM
/// interface / number / array via a per-argument transform handle — without it
/// the raw `Closure` reaches a method expecting e.g. a Gradle `Action`, and the
/// callee's `action.execute(...)` throws `NoSuchMethodError: …Closure.execute`.
pub(crate) const MH_KIND_FILTER: i32 = 18;

/// Argument-fold adapter produced by `MethodHandles.foldArguments(target,
/// [pos,] combiner)`. `MH_BOUND` holds a 3-field wrapper (field 0 = target MH,
/// field 1 = combiner MH, field 2 = pos). On invocation the combiner is applied
/// to `combiner.parameterCount()` arguments starting at `pos`; if the combiner
/// returns a value it is spliced in at `pos` (so the target sees the combiner's
/// result followed by all the original arguments), then the target is
/// dispatched. Groovy's `TypeTransformers.TO_REFLECTIVE_PROXY` is
/// `foldArguments(Proxy.newProxyInstance…, new ConvertedClosure(closure,name))`
/// — without a real fold the `ConvertedClosure` handler is never built and the
/// proxy is created with no interfaces (its SAM method then 404s).
pub(crate) const MH_KIND_FOLD: i32 = 19;

/// "Invoker" handle produced by `MethodHandles.exactInvoker(type)` /
/// `MethodHandles.invoker(type)` / `MethodHandles.spreadInvoker(type, N)`.
/// No `MH_BOUND` wrapper is needed: per the JDK contract, invoking this
/// handle as `invoker.invoke(target, arg1, arg2, ...)` is equivalent to
/// `target.invokeExact(arg1, arg2, ...)` — the target handle is supplied as
/// the FIRST argument at each call, not captured at creation time. Apache
/// Groovy's `IndyInterface.<clinit>` builds exactly one of these
/// (`CACHED_INVOKER = MethodHandles.exactInvoker(methodType(Object.class,
/// Object[].class))`) and every generic (non-special-cased) Groovy
/// `invokedynamic` call site funnels through it: the JIT-produced call-site
/// bytecode does `insertArguments`/`foldArguments` chains that ultimately
/// invoke `CACHED_INVOKER` with the real dispatch target (built by
/// `fromCacheHandle`/`selectMethodHandle`) as its leading argument. Before
/// this handle existed, `exactInvoker`/`invoker`/`spreadInvoker` returned an
/// inert stub with no `MH_KIND` set, so `mh_dispatch` on it hit the
/// `mh_read_class == None` fast-fail and silently returned `null` — the
/// closure body of every Groovy `beans { ... }`-style dynamic DSL call
/// (Spring's `GroovyBeanDefinitionReader`) never actually ran, registering
/// zero beans with no visible exception.
pub(crate) const MH_KIND_INVOKER: i32 = 20;

/// Exception-catching adapter produced by `MethodHandles.catchException`.
/// `MH_BOUND` holds a 3-field wrapper: field 0 = target MH, field 1 = caught
/// exception `Class`, field 2 = handler MH. On a matching Java exception from
/// the target, dispatch invokes the handler with the thrown exception followed
/// by the leading original arguments that fit the handler's type.
pub(crate) const MH_KIND_CATCH: i32 = 21;

/// Return-value-filtering adapter produced by `MethodHandles.filterReturnValue`.
/// `MH_BOUND` holds a 2-field wrapper: field 0 = target MH, field 1 = filter MH
/// (unary, applied to the target's return value). Dispatch invokes `target`
/// with the incoming args, then passes its result through `filter`, returning
/// the filter's result in place of the target's raw one.
///
/// Was previously a no-op stub (`filterReturnValue` returned `target`
/// unchanged, silently dropping `filter`). JRuby 10.x's
/// `org.jruby.runtime.invokedynamic.VariableSite.ivar` bootstrap builds its
/// instance-variable-getter call-site targets by filtering the raw
/// `IRubyObject.getInstanceVariable(String)` result (which is a genuine Java
/// `null` for an unset ivar -- normal at that raw layer) through a handle that
/// substitutes the JRuby runtime's `nil` singleton. With the filter dropped,
/// `mh.invoke()` returned the raw `null` straight through; the caller (e.g.
/// `@canonical_segments ||= ...`'s truthiness test) then fed that `null` into
/// `org.jruby.ir.targets.indy.IsTrueSite.init`, which unconditionally calls
/// `obj.getRuntime()` on it -- `NullPointerException`. Found chasing the
/// residual `JRubyScriptTemplateTests` failure left after the array-vs-scalar
/// SAM-mismatch fix (2026-07-15); `require 'erb'; require 'ostruct'` alone
/// reproduces it standalone, no Spring needed.
pub(crate) const MH_KIND_RETURN_FILTER: i32 = 22;

/// `MethodHandles.collectArguments(target, pos, filter)` adapter. Distinct
/// from `MH_KIND_COLLECT` (`MethodHandle.asCollector`, which SPREADS one
/// trailing array argument into N individual target params -- the inverse
/// direction) and from `MH_KIND_FOLD` (`foldArguments`, which also runs a
/// combiner over a slice of args at `pos` but SPLICES its result in ADDITION
/// to -- not instead of -- the full original arg list). `collectArguments`
/// consumes `filter.type().parameterCount()` args starting at `pos` by
/// calling `filter` on them, then REPLACES that consumed range with filter's
/// single (non-void) result before dispatching `target` -- the args outside
/// the consumed range pass through unchanged, but the consumed ones do not
/// reappear. Was previously a complete no-op stub (returned `target`
/// unmodified, silently dropping `pos`/`filter` entirely) -- same failure
/// shape as the `filterReturnValue` no-op bug fixed earlier in this
/// investigation (commit `3af9ab62`). Confirmed live via
/// `CRATONVM_DBG_MH_DISPATCH` tracing + `javap` decompile of the real
/// `com.headius.invokebinder-1.14.jar`'s `Binder.collect(int, int, Class,
/// MethodHandle)`, which JRuby 10.x's `BuildDynamicStringSite` uses (via
/// `MethodHandles.collectArguments` under the hood) to reduce each
/// `(ThreadContext, IRubyObject)` pair produced by an earlier
/// `MethodHandles.permuteArguments` step (itself correct -- verified against
/// real, unmodified invokebinder bytecode computing an intentional
/// `[0, 0, 1, ...]`-shaped "ctx-per-dynamic-value" reorder array) down to a
/// single `to_s`-guarded `IRubyObject`. With the no-op stub, that reduction
/// never happened: the duplicated `ThreadContext` from the permute step
/// survived unchanged all the way to `BuildDynamicStringSite.buildString`,
/// landing in the argument slot its `IRubyObject` parameter expects, and
/// `RubyString.append`/`appendAsStringOrAny` threw `ClassCastException:
/// ThreadContext cannot be cast to IRubyObject` -- reached via
/// `JRubyScriptTemplateTests`'s `require 'ostruct'` (`ostruct.rb:477`,
/// string interpolation in `OpenStruct`'s class body).
pub(crate) const MH_KIND_COLLECT_ARGS: i32 = 23;

/// `MethodHandles.arrayElementGetter(arrayClass)` — a handle of type
/// `(T[],int)T` that reads `array[index]`. `MH_CLASS` holds the array
/// descriptor (`[I`, `[Ljava/lang/String;`, …), `MH_DESC` the full
/// `([I I)I`-shaped accessor descriptor, so `mh.type()` and the
/// `invokeExact` return-boxing both read the right component type.
///
/// Was previously an INERT handle: `register_array_element_accessor_bridges`
/// allocated a bare 17-slot `java/lang/invoke/MethodHandle` with a `()V`
/// `type` and NO synthetic metadata at all — slots 16–20 (`MH_CLASS` …
/// `MH_BOUND`) did not exist on a 17-slot object. That was enough for
/// `ObjectStreamClass$RecordSupport.<clinit>`, whose `PRIM_VALUE_EXTRACTORS`
/// map only needs the handle to be non-null (the record rebuild itself is
/// pinned separately as `MH_KIND_RECORD_DESER`), but ANY actual invocation
/// silently produced zero: `MethodHandle.invokeExact` read `MH_DESC` (slot
/// 18) and `MH_KIND` (slot 19) out of bounds — the GC guard logged exactly
/// those two indices with `num_slots=17` — and `mh_dispatch`'s `mh_read_class`
/// then found a null `MH_CLASS` and took its `None` fast-fail, returning
/// `null`, which the call-site return coercion turned into `Value::Int(0)`.
/// regression-suite `RJdkHandles.adaptation` (`arrayElementGetter`,
/// `RJdkHandles.java:147`) failed on `aget.invokeExact(new int[]{7,8,9}, 1)
/// == 8` in BOTH `--real-jdk` and `--jdk-only`, reading 0.
pub(crate) const MH_KIND_ARRAY_GET: i32 = 24;

/// `MethodHandles.arrayElementSetter(arrayClass)` — a handle of type
/// `(T[],int,T)void` that stores `array[index] = value`. Sibling of
/// `MH_KIND_ARRAY_GET`; see that constant for the inert-handle defect both
/// factories shared.
pub(crate) const MH_KIND_ARRAY_SET: i32 = 25;

/// `MethodHandles.countedLoop(iterations, init, body)` —
/// `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`'s "whole JDK 9

/// API absent" bucket: the whole loop family plus `tryFinally` had no native
/// at all, so real JDK bytecode ran and NPE'd/AbstractMethodError'd building
/// `LambdaForm`/`MethodHandleImpl$LoopClauses` machinery this VM's `MH_KIND_*`
/// shim model doesn't have. `MH_BOUND` holds a 3-slot carrier `(iterations,
/// init, body)`; dispatch runs `v = init.invoke(A); for i in 0..iterations
/// .invoke(A) { v = body.invoke(v, i, A); }`, measured against HotSpot 25
/// (`l.countedLoop` = 10 for a 5-iteration `value+i` accumulator).
pub(crate) const MH_KIND_LOOP_COUNTED: i32 = 26;

/// `MethodHandles.whileLoop(init, pred, body)` — sibling of
/// `MH_KIND_LOOP_COUNTED`. `MH_BOUND` holds `(init, pred, body)`; dispatch
/// runs `v = init.invoke(A); while (pred.invoke(v, A)) { v = body.invoke(v,
/// A); }`.
pub(crate) const MH_KIND_LOOP_WHILE: i32 = 27;

/// `MethodHandles.doWhileLoop(init, body, pred)` — note the ARGUMENT ORDER
/// differs from `whileLoop`: body before pred. `MH_BOUND` holds `(init, body,
/// pred)`; dispatch runs the body once unconditionally before the first test
/// (`do { v = body.invoke(v, A); } while (pred.invoke(v, A));`), so a
/// predicate that is false on entry still yields one iteration.
pub(crate) const MH_KIND_LOOP_DOWHILE: i32 = 28;

/// `MethodHandles.iteratedLoop(iterator, init, body)`. `MH_BOUND` holds
/// `(init, body, iterator)`; dispatch takes the `Iterator` from `iterator`
/// (over its prefix of the arguments) or, when it is null, from the leading
/// argument's `Iterable.iterator()`, calls `Iterator.hasNext()`/
/// `Iterator.next()` via `NativeContext::invoke_virtual` and folds
/// `v = body.invoke(v, item, A)` over each element. Until wave 44 only the
/// `iterator == null` form was implemented: a non-null `iterator` was dropped.
pub(crate) const MH_KIND_LOOP_ITERATE: i32 = 29;

/// `MethodHandles.tryFinally(target, cleanup)` — `MH_BOUND` holds `(target,
/// cleanup)`. Dispatch runs `target`; on normal return, `cleanup.invoke(null,
/// result, A)` runs and ITS result is returned. On an exception, `cleanup
/// .invoke(thrown, defaultResult, A)` runs and then the ORIGINAL exception is
/// rethrown — unless cleanup itself throws, in which case that new exception
/// replaces it (JDK contract; measured `g.tryFinallyThrows` = the target's
/// `IllegalStateException` propagates even though cleanup ran).
pub(crate) const MH_KIND_TRY_FINALLY: i32 = 30;

/// `MethodHandles.arrayLength(arrayClass)` — a handle of type `(T[])int`
/// that reads `array.length`. Sibling of `MH_KIND_ARRAY_GET`/`_SET` (same
/// factory family, same inert-handle species of bug): this one was never
/// bridged at all, so real JDK 9 bytecode ran and hit the exact
/// `AbstractMethodError` `MH_KIND_ARRAY_GET`'s doc comment predicts for a
/// primitive-array `viewAsType`/`copyWith` path — measured
/// (`k.arrayLength`, `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`)
/// unambiguously: "one factory, one dispatch arm, no loop semantics", the
/// cheapest row in the whole 16-row list to confirm the wiring on.
pub(crate) const MH_KIND_ARRAY_LENGTH: i32 = 31;

/// `MethodHandles.tableSwitch(fallback, targets...)` (interpreter round i1
/// wave 44, lane L4; see `make_table_switch_adapter`). `MH_BOUND` holds the
/// carrier `(fallback, target0, .., targetN-1)`; dispatch reads the leading
/// `int` selector and invokes `target[selector]` when `0 <= selector < N`,
/// else `fallback`, with ALL the arguments (the selector included), as the
/// JDK's `tableSwitch` does. The carrier's slot 0 holds `N` (an `Int`), then
/// the fallback, then the targets.
pub(crate) const MH_KIND_TABLE_SWITCH: i32 = 32;

/// `MethodHandles.throwException(rtype, exType)` (interpreter round i1 wave
/// 48, lane L4; see `make_throw_exception_handle`): a handle of type
/// `(exType)rtype` that throws its argument. `MH_BOUND` holds the exception
/// after `bindTo` (whose type is then `()rtype`); otherwise dispatch throws
/// its first argument, and a `null` one is HotSpot's `athrow` NPE.
pub(crate) const MH_KIND_THROW: i32 = 33;

// ---------------------------------------------------------------------------
// Round-9 perf: LambdaMetafactory CallSite cache.
// ---------------------------------------------------------------------------
//
// Each `(invokedType, samMethodType, implMethod, instantiatedType)` tuple
// is bootstrap-invariant — the JDK canonicalises bootstrap arg objects so
// repeated lookups for the same lambda site share the same ObjectRef
// identities. The cache stores the materialised `ConstantCallSite` so the
// (target MH + CCS) allocation pair runs at most once per shape, not once
// per invoke.
//
// Cache hits skip:
//   * `alloc_method_handle` (1 synthetic + 3 string allocations)
//   * MH_TYPE field plumbing (1 hash-lookup + set_field_by_name)
//   * `alloc_concurrent_synthetic` for the ConstantCallSite
//
// Process-global so all threads share the same cached entry, keyed by the
// VM and (interpreter round i1 wave 9) the `Lookup`'s lookup class, which is
// the spun proxy's HOST (`NativeContext::register_lambda_proxy_host`): the
// host names the class (`<host>$$Lambda/0x..`), is its nest host and pins its
// loader, so two lookup classes must not share one proxy. A host's rows are
// dropped when it unloads (`forget_lambda_callsites_of_unloaded_hosts`, from
// the VM's lambda-proxy unload hook, which drops the proxies they name).
//
// GC: cached `ObjectRef`s are reported as roots via
// `gc_scan_lambda_callsite_cache_roots` and remapped via
// `gc_update_lambda_callsite_cache_refs` — same contract as the
// `INTEGER_CACHE` in `lang_math.rs`. Without these hooks a moving GC
// would leave stale pointers behind after compaction.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct LambdaKey {
    /// Raw id of the lookup class (the proxy's host); `0` when the `Lookup`
    /// carried no readable one, and then no host is recorded either.
    pub(crate) host: u32,
    pub(crate) invoked: usize,
    pub(crate) sam: usize,
    pub(crate) impl_: usize,
    pub(crate) instantiated: usize,
}

/// `(vm_identity, bootstrap-arg identities)`.
///
/// gc-common w8-a (`common-b-process-global-root-sources`, row
/// `lambda-callsites`): the cache is shared by every VM in the process, so it
/// leads its key with the VM that bootstrapped the call site. Without that, VM
/// B's collection rooted and remapped VM A's `CallSite`s, and a VM whose
/// bootstrap args sat at an address another (live or dead) VM had used could
/// be handed that VM's `CallSite`.
pub(crate) type LambdaCacheKey = (usize, LambdaKey);

pub(crate) static LAMBDA_CALLSITE_CACHE: std::sync::OnceLock<
    parking_lot::Mutex<rustc_hash::FxHashMap<LambdaCacheKey, ObjectRef>>,
> = std::sync::OnceLock::new();

pub(crate) fn lambda_callsite_cache(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<LambdaCacheKey, ObjectRef>> {
    LAMBDA_CALLSITE_CACHE.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Per-VM teardown: drop every cached `CallSite` of `vm`. Idempotent. Another
/// VM's collection never scans these rows (both GC hooks walk only the
/// collecting VM's), so this returns memory and retires raw addresses into a
/// heap that is going away.
pub fn forget_vm_lambda_callsite_cache(vm: usize) {
    lambda_callsite_cache()
        .lock()
        .retain(|&(owner, _), _| owner != vm);
}

/// Class-unload hook: drop `vm`'s cached `CallSite`s whose host (lookup
/// class, [`LambdaKey::host`]) unloaded. Called from the VM's
/// `invokedynamic::forget_unloaded_lambda_proxy_rows`, in the same pass that
/// drops the proxies those rows' factory handles name; a surviving row would
/// hand out a `CallSite` allocating instances of an unregistered proxy id.
/// Rows with no host (`0`) are never dropped here.
pub fn forget_lambda_callsites_of_unloaded_hosts(vm: usize, is_unloaded: &dyn Fn(u32) -> bool) {
    lambda_callsite_cache()
        .lock()
        .retain(|&(owner, k), _| owner != vm || k.host == 0 || !is_unloaded(k.host));
}

pub(crate) fn lambda_key_of(o: Option<ObjectRef>) -> usize {
    o.map(|r| r.as_ptr() as usize).unwrap_or(0)
}

/// The host of a reflectively bootstrapped lambda: the lookup class of the
/// `Lookup` in `args[0]` (slot 0, `lookupClass`, as the `lookupClass()`
/// native reads it) — HotSpot's `caller.lookupClass()`. A class id, not a
/// reference, so it is read once on entry and needs no pin. Only a mirror the
/// VM's reverse map knows is accepted; anything else records no host.
pub(crate) fn reflective_lambda_host(ctx: &dyn NativeContext, lookup: Option<&Value>) -> Option<ClassId> {
    let Some(Value::Object(Some(lookup))) = lookup else {
        return None;
    };
    match ctx.get_field(*lookup, 0) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    }
}

/// The cache key for the four bootstrap-arg objects `objs`, each read back
/// through the pin taken on it before `build_reflective_lambda_callsite`.
///
/// gen r4w4/rooting2: the insert used to reuse the key computed on ENTRY,
/// i.e. the pre-build addresses. The build runs Java, so after a moving cycle
/// that key named addresses no live object held, the pointer map of that
/// cycle had already been applied, and the entry could never hit again -- or,
/// once the young generation reused the vacated addresses, could hit for a
/// different tuple of objects and hand back another lambda's CallSite.
pub(crate) fn lambda_key_from_pins(
    ctx: &dyn NativeContext,
    host: u32,
    objs: [Option<ObjectRef>; 4],
    pins: [Option<usize>; 4],
) -> LambdaKey {
    let cur = |i: usize| match (objs[i], pins[i]) {
        (Some(o), Some(p)) => Some(ctx.read_native_pin(p, o)),
        (o, _) => o,
    };
    LambdaKey {
        host,
        invoked: lambda_key_of(cur(0)),
        sam: lambda_key_of(cur(1)),
        impl_: lambda_key_of(cur(2)),
        instantiated: lambda_key_of(cur(3)),
    }
}

/// GC root scan hook — see `lang_math::gc_scan_value_of_cache_roots`.
/// Reports every cached lambda `CallSite` ObjectRef so the GC keeps it
/// live across compaction. Only `vm`'s rows (gc-common w8-a).
///
/// gen r4w4/rooting2: the KEY objects are reported too. The keys are object
/// identities stored as addresses, and `gc_update_lambda_callsite_cache_refs`
/// can only rewrite an address the collector moved. A key object nothing
/// else held used to die with its entry still in place, and under the moving
/// young collector its from-space address is handed out again on the next
/// cycle, so a later bootstrap whose four arguments happened to land on the
/// four dead addresses would be answered with the dead tuple's CallSite.
/// Rooted, a key object lives exactly as long as its entry and is remapped
/// with it. The objects are `MethodType`s and `MethodHandle`s a class's
/// constant pool already keeps alive for the common case, so the added
/// retention is limited to reflective bootstrap arguments.
///
/// Interpreter round i1 wave 9: every value is visited with its row's owner,
/// the host class id (`None` for a host-less row), so the VM can make a row
/// of a user-loader host loader-conditional (`native_roots::defer_or_root`)
/// instead of rooting it outright — rooted, the key `MethodType`s' mirrors
/// kept that loader, and so the host, alive forever. A deferred row lives
/// exactly as long as the host's loader; when the host unloads, the row goes
/// with it in the same pause (`forget_lambda_callsites_of_unloaded_hosts`).
pub fn gc_scan_lambda_callsite_cache_roots(
    vm: usize,
    visit: &mut dyn FnMut(Option<u32>, ObjectRef),
) {
    let cache = lambda_callsite_cache().lock();
    for (&(owner, k), v) in cache.iter() {
        if owner != vm {
            continue;
        }
        let host = (k.host != 0).then_some(k.host);
        visit(host, *v);
        for addr in [k.invoked, k.sam, k.impl_, k.instantiated] {
            if addr != 0 {
                // SAFETY: a non-zero key component is the address of a live
                // object: it was read from a current pinned reference when the
                // entry was inserted (`lambda_key_from_pins`), this scan keeps
                // it reachable (outright, or through its host's loader until
                // the host unloads and the row is dropped), and
                // `gc_update_lambda_callsite_cache_refs` rewrites it on every
                // move. Non-zero and 8-aligned by construction, as every
                // `ObjectRef` it was taken from was.
                visit(host, unsafe { ObjectRef::from_raw(addr as *mut u8) });
            }
        }
    }
}

/// GC post-compaction hook — remaps cached `CallSite` entries through the
/// GC's pointer map. Keys (bootstrap arg ObjectRef identities) are also
/// remapped so a post-GC lookup with the same logical bootstrap args
/// still hits the cache. The remap rebuilds the table entry-by-entry
/// because cache key membership is hash-sensitive to the remapped value.
/// Only `vm`'s rows are touched: another VM's are not in this pointer map's
/// heap (gc-common w8-a). All of them are taken out before any is put back,
/// so a remapped key can never collide with a not-yet-remapped one.
pub fn gc_update_lambda_callsite_cache_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let mut cache = lambda_callsite_cache().lock();
    let mine: Vec<LambdaCacheKey> = cache
        .keys()
        .filter(|&&(owner, _)| owner == vm)
        .copied()
        .collect();
    let old: Vec<(LambdaKey, ObjectRef)> = mine
        .into_iter()
        .filter_map(|key| cache.remove(&key).map(|v| (key.1, v)))
        .collect();
    let remap = |addr: usize| -> usize {
        if addr == 0 {
            return 0;
        }
        match pointer_map.get(&addr) {
            Some(&new_addr) => new_addr,
            None => addr,
        }
    };
    for (k, mut v) in old {
        let k_new = LambdaKey {
            host: k.host,
            invoked: remap(k.invoked),
            sam: remap(k.sam),
            impl_: remap(k.impl_),
            instantiated: remap(k.instantiated),
        };
        let old_v = v.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_v) {
            // C20 — runtime guard (was `debug_assert!` only). After class
            // unloading the pointer map can contain stale `addr → 0`
            // entries; in release builds the previous `debug_assert!` was
            // a no-op and `ObjectRef::from_raw(0)`'s own null check is
            // also debug-only, so a null entry would silently fabricate
            // an invalid `NonNull<u8>` that the next GC root scan would
            // dereference. Skip the entry instead — the lambda CallSite
            // is unrooted from the cache for this cycle and will be
            // rebuilt on next bootstrap; never insert a wild pointer.
            if new_addr == 0 {
                continue;
            }
            v = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
        cache.insert((vm, k_new), v);
    }
}

/// Allocate a fully-described MethodHandle.
/// Is the method this handle resolves to declared `ACC_VARARGS` (`T...`)?
///
/// Answered from the class file's own access flags, which is the only place the
/// distinction lives: `void m(String[] a)` and `void m(String... a)` have the
/// same descriptor and differ solely by this bit.
///
/// **Two screens before the method-table walk.** `declared_methods` allocates a
/// `Vec<MethodMetadata>` with a `String` per entry, and `alloc_method_handle`
/// runs on every handle creation — including the field-accessor and combinator
/// kinds that have no method at all. So this returns early unless the handle is
/// for a real method AND the descriptor's LAST parameter is an array, which
/// every variable-arity method's is (JLS §8.4.1).
///
/// The superclass walk is not optional: `findVirtual(sub, name, type)` resolves
/// an INHERITED method, and its `ACC_VARARGS` bit lives on the declaring class,
/// not on the receiver's.
pub(crate) fn method_is_variable_arity(
    ctx: &dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
) -> bool {
    if !(kind == MH_KIND_STATIC
        || kind == MH_KIND_VIRTUAL
        || kind == MH_KIND_SPECIAL
        || kind == MH_KIND_CONSTRUCTOR)
    {
        return false;
    }
    if class.is_empty() || name.is_empty() {
        return false;
    }
    match split_descriptor_params(desc) {
        Some((params, _)) if params.last().is_some_and(|p| p.starts_with('[')) => {}
        _ => return false,
    }
    let mut cid = match ctx.class_id_by_name(class) {
        Some(c) => c,
        None => return false,
    };
    // Bounded: a hierarchy deeper than this is a cycle, and a cycle here would
    // hang handle creation rather than answer it wrong.
    for _ in 0..64 {
        let found = ctx
            .declared_methods(cid)
            .into_iter()
            .find(|m| m.name == name && m.descriptor == desc);
        if let Some(m) = found {
            // ACC_VARARGS, JVMS Table 4.6-A.
            return (m.access_flags & 0x0080) != 0;
        }
        cid = match ctx.superclass_of(cid) {
            Some(s) => s,
            None => return false,
        };
    }
    false
}

/// # JVMS 6.5 census: this is the `MethodHandle` mint the boot WARN names
///
/// `java.lang.invoke.MethodHandle` is `ACC_ABSTRACT`, so no `new` in any image
/// could have produced this object.
/// `cratonvm_native_api::instantiable::observe_uninstantiable_receiver` reports
/// that once per class per boot, and on a stock `cratonvm Hello` the
/// `requester=` it prints is the allocation below -- not because this site is
/// special, but because it is the first of this file's four `MethodHandle`
/// mints the run reaches. AUDITED 2026-09-01: the violation is real, both
/// repairs were assessed, and both are wrong HERE. The allocation is left
/// exactly as it is and the WARN is left firing on purpose.
///
/// ## What the object is, before deciding what to do about it
///
/// A 22-slot carrier (`MH_VARARGS + 1`) whose slots 16.. hold CratonVM's own
/// description of the handle -- class, name, descriptor, `MH_KIND`, the
/// `MH_BOUND` combinator state, the varargs-collector bit -- and whose low
/// slots are deliberately left to the real JDK layout so
/// `set_field_by_name(mh, "type", ..)` still lands on `MethodHandle.type`
/// (`MH_BASE`'s note). It is not an opaque token: it is returned to Java from
/// every `Lookup.find*` / `unreflect*`, stored in fields declared
/// `MethodHandle`, `checkcast`-ed to `java/lang/invoke/MethodHandle`, and used
/// as the receiver of signature-polymorphic `invoke` / `invokeExact` /
/// `invokeBasic`.
///
/// ## (a) mint a concrete JDK subclass -- rejected
///
/// The workspace has already run this experiment in the opposite direction.
///
/// `panama::DOWNCALL_CARRIER_CLASS` used to be a bespoke CONCRETE class,
/// `java/lang/foreign/DowncallHandle`. Moving it TO this same abstract
/// `java/lang/invoke/MethodHandle` is recorded there as "the whole of the
/// `--jdk-only` fix", and it names the price of a carrier that is not literally
/// `MethodHandle`: `vm/src/vm/vm_exec.rs`'s
/// `is_method_handle_signature_polymorphic_receiver` has to name the class or
/// `invokeExact` stops linking signature-polymorphically;
/// `vm/src/runtime/interpreter/typecheck.rs` has to hard-code that the class is
/// castable to `MethodHandle` or every `checkcast` on a returned handle throws;
/// and `asType` has to stop writing the `type` field. Stamping
/// `java/lang/invoke/DirectMethodHandle` here re-incurs all three, and adds an
/// `ensure_class_initialized` of the JDK's own `java.lang.invoke` bootstrap to
/// the first `findStatic` of every run. Two of those three files are outside
/// this audit's write scope.
///
/// ## (b) mint a `cratonvm/internal/...` stand-in -- rejected
///
/// For those same three reasons, plus one the naming convention cannot fix.
///
/// Today's fiction fails in the harmless direction: because the class IS the
/// abstract type, `instanceof MethodHandle` and `checkcast MethodHandle` both
/// answer correctly and only `getClass()` lies -- and nothing in the corpus
/// reads `getClass()` on a handle. Renaming without a matching arm in
/// `typecheck.rs`'s `synthetic_implements` converts a wrong `getClass()` that
/// nothing reads into a wrong `checkcast` that lambda linkage and
/// `invokedynamic` reach on every call. That is a strictly worse trade, so it
/// is not made.
///
/// ## (c) why this cannot be closed from this file at all
///
/// The census dedupes by CLASS NAME. Repairing all four `MethodHandle` mints in
/// this file would not delete the WARN line: `java/lang/invoke/MethodHandle` is
/// also minted by `native-builtins/src/panama.rs` (`alloc_downcall_handle`, on
/// the documented reasoning above) and by
/// `native-builtins/src/classloader.rs`, and the report would simply re-point
/// `requester=` at whichever of those ran first. Closing this species is a
/// coordinated change across those two files and the two VM files named above,
/// not a local edit -- which is why nothing here is behind a kill switch:
/// nothing about how these handles are minted changed, so there is nothing to
/// A/B.
///
/// (The WARN became one exit-census line on 2026-09-01 and went behind
/// `CRATONVM_DBG=exit-census` in nio21 wave 8. nio21 wave 11 L1 re-read this
/// audit; the row stays open on
/// `docs/feature-designs/native-dispatch-architecture.md`.)
pub(crate) fn alloc_method_handle(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    alloc_method_handle_with_receiver(ctx, class, name, desc, kind, None, None)
}

/// [`alloc_method_handle`] whose `type()` names `receiver` as the leading
/// parameter of a VIRTUAL/SPECIAL handle instead of `class`.
///
/// `findSpecial(refc, name, type, specialCaller)` and
/// `unreflectSpecial(m, specialCaller)` hand back a handle typed
/// `(specialCaller, ...)`: the JDK narrows the receiver to the caller
/// (`restrictReceiver`), since an `invokespecial` may only be made on an
/// instance of the caller. Typing it `(refc, ...)` made every
/// `invokeExact(sub, ...)` at a `(Sub,...)` call site throw
/// `WrongMethodTypeException`. Dispatch still resolves in `class` (`MH_CLASS`);
/// only the `type` field changes.
///
/// `near` (round 16 wave 17, lane mhapp): the class the handle was looked up
/// on, whose defining loader resolves the `type`'s names
/// ([`build_method_type_from_descriptor_near`]); `None` is the loader-blind
/// lookup, and so is `CRATONVM_MH_LOADER_SCOPED_TYPES=0`.
pub(crate) fn alloc_method_handle_with_receiver(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
    receiver: Option<&str>,
    near: Option<ClassId>,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // C15: Allocate MH_VARARGS+1 slots so our synthetic fields (at slots 16-21)
    // live PAST the real JDK's instance-field count (6). This prevents
    // `set_field_by_name(mh, "type", ...)` — which resolves to slot 0 — from
    // overwriting our class/name/desc/kind/bound data.
    // W7-19 §5.2 moved this from `MH_BOUND + 1` to `MH_VARARGS + 1` for the
    // `asVarargsCollector` marking. Widening is safe in one direction only:
    // handles minted HERE gain a slot, handles minted elsewhere do not, which
    // is why every reader of `MH_VARARGS` width-guards. Wave 20 widened it
    // again, by the same rule, for `MH_STATIC_SETTLED`, round 12 wave 5 for
    // `MH_SPECIAL_CALLER`, round 12 wave 7 for `MH_CAST_CHAIN`, and SBR wave
    // 5 for the field-accessor link (`MH_FIELD_LINK`..; still below
    // `panama::DOWNCALL_BASE`).
    let mh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_SLOT_COUNT)?;
    // The `specialCaller` a SPECIAL handle's leaf receiver cast needs (see
    // `MH_SPECIAL_CALLER`); nothing when it is the dispatch class itself.
    let special_caller = receiver
        .filter(|r| kind == MH_KIND_SPECIAL && !r.is_empty() && *r != class)
        .filter(|_| special_caller_cast_enabled());
    // GC-safety: the `create_string` calls below (and `build_method_type_
    // from_descriptor` further down) can trigger a collection that
    // relocates `mh`; `cls`/`nm` are each also read again after a LATER
    // `create_string` call of their own. Pin everything now and re-read the
    // forwarded reference right before each use (mirrors `create_field_object`).
    // Interpreter round i3 wave 71 (lane L4,
    // `CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS`): a field accessor's member
    // strings come from the intern pool, so every handle of one field holds
    // the same three objects (see `mh_finder_shares_member_words`).
    let shared = mh_finder_shares_member_words(kind);
    let mh_pin = ctx.pin_native_root(mh);
    let cls = mh_member_string(ctx, class, shared);
    let cls_pin = ctx.pin_native_root(cls);
    let nm = mh_member_string(ctx, name, shared);
    let nm_pin = ctx.pin_native_root(nm);
    let caller_pinned = special_caller.map(|r| {
        let s = ctx.create_string(r);
        (ctx.pin_native_root(s), s)
    });
    let dc = mh_member_string(ctx, desc, shared);
    let mh = ctx.read_native_pin(mh_pin, mh);
    let cls = ctx.read_native_pin(cls_pin, cls);
    let nm = ctx.read_native_pin(nm_pin, nm);
    let caller = caller_pinned.map(|(pin, s)| ctx.read_native_pin(pin, s));
    ctx.set_field(mh, MH_CLASS, Value::Object(Some(cls)));
    ctx.set_field(mh, MH_NAME, Value::Object(Some(nm)));
    ctx.set_field(mh, MH_DESC, Value::Object(Some(dc)));
    ctx.set_field(mh, MH_KIND, Value::Int(kind));
    ctx.set_field(mh, MH_BOUND, Value::Object(None));
    // Not settled: written for the reason the marking below is.
    ctx.set_field(mh, MH_STATIC_SETTLED, Value::Int(0));
    ctx.set_field(mh, MH_SPECIAL_CALLER, Value::Object(caller));
    // No chain: an unstamped handle (written for the reason the marking is).
    ctx.set_field(mh, MH_CAST_CHAIN, Value::Object(None));
    // Not linked (`mh_field_link_try`), written for the same reason.
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(None));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(0));
    // No bound static owner (the same reason).
    ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(0));
    // The marking. Written unconditionally rather than left to the allocator so
    // the read in `mh_is_varargs_collector` never has to interpret an unwritten
    // slot: a raw slot can read back as stale padding, and "is this handle a
    // collector" must not be answered from one.
    //
    // `Lookup.find*`/`unreflect*` on a method declared `T...` hand back a
    // VARIABLE-ARITY handle — the JDK applies `asVarargsCollector` at the end
    // of `getDirectMethod`, and it is what makes `mh.asType(longerType)`
    // collect the trailing arguments instead of failing on the arity. Until
    // this line, only an explicit `asVarargsCollector()` call set it, so
    // `MethodHandles.lookup().findStatic(C, "m", (String,String[])String)
    // .isVarargsCollector()` answered `false` where every JDK answers `true`
    // (`probes/VarargsCollectorProbe.java`, six rows) and the `asType` that
    // depends on it threw `WrongMethodTypeException`.
    let varargs = method_is_variable_arity(ctx, class, name, desc, kind);
    mh_write_marking_bits(&*ctx, mh, if varargs { MH_MARK_VARARGS } else { 0 });
    // Populate the real-JDK MethodHandle.type:MethodType field at its
    // resolved slot (0) so `mh.type()` and JDK-internal reads (LambdaForm,
    // MemberName, Invokers, ObjectStreamClass) see a MethodType, not null.
    // C21: Always populate `type` — fall back to `()V` when no usable
    // descriptor was supplied (empty-desc alloc sites for missing-arg
    // failure paths in lookup_find_static_getter/setter, linkCallSite, etc.)
    // so JDK-internal `erasedType()` / `parameterSlotCount()` walks never
    // observe a null MethodType.
    // The `type()` of an UNBOUND virtual/special method handle includes the
    // receiver as its LEADING parameter, exactly as HotSpot does
    // (`Lookup.unreflect`/`findVirtual` on `int m(Object)` -> type
    // `(Recv,Object)int`, parameterCount 2). Our raw `desc` is the bytecode
    // descriptor WITHOUT the receiver, so prepend `Lclass;` for the `type`
    // field only. (MH_DESC — used by the invoke dispatch and the invokeExact
    // arity check — stays the raw descriptor; the receiver comes from args[0]
    // there.) Without this, `mh.type().parameterCount()` was one short, and
    // Groovy's IndyInterface `Selector.correctCoerce` threw
    // `GroovyBugError: argument array length and parameter array length should
    // be the same` when dispatching any instance call (e.g. Gradle/Groovy
    // SpringRepositoriesExtension). STATIC/CONSTRUCTOR/GETTER keep raw `desc`.
    //
    // A CONSTRUCTOR handle needs the mirror adjustment at the other end: the
    // bytecode descriptor of `<init>` returns `V`, but `findConstructor` and
    // `unreflectConstructor` hand back a handle whose type RETURNS THE CLASS —
    // `(int)Bean`, not `(int)void`, which `probes/L3MemberNameProbe` diffs
    // against the host JDK. `MH_DESC` keeps the raw `(...)V` for dispatch and
    // the `invokeExact` arity check, exactly as for the receiver-prepend above.
    let recv_desc;
    let recv_class = receiver.filter(|r| !r.is_empty()).unwrap_or(class);
    let type_desc: &str = if (kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL)
        && !recv_class.is_empty()
        && desc.starts_with('(')
    {
        recv_desc = format!("(L{};{}", recv_class, &desc[1..]);
        recv_desc.as_str()
    } else if kind == MH_KIND_CONSTRUCTOR && !class.is_empty() && desc.ends_with(")V") {
        recv_desc = format!("{}L{};", &desc[..desc.len() - 1], class);
        recv_desc.as_str()
    } else {
        desc
    };
    // The member's owner, for a VIRTUAL handle (`CRATONVM_MH_INSTANCE_OWNER_ID`):
    // `near` is the class `class` names (the `refc` / `Method.clazz` mirror).
    let instance_owner =
        near.filter(|_| kind == MH_KIND_VIRTUAL && !class.is_empty() && !class.starts_with('['));
    let near = near.filter(|_| mh_loader_scoped_types_enabled());
    // Wave 71 (`CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS`): the shared table's
    // `MethodType` for the descriptor, as `bindTo` takes it -- the object a
    // fresh build answers, built once.
    let built = if shared && type_desc.starts_with('(') {
        method_type_through_table_near(ctx, type_desc, near)
    } else {
        build_method_type_from_descriptor_near(ctx, type_desc, near)
    };
    let mt_opt = match built {
        Ok(Some(mt)) => Ok(Some(mt)),
        Ok(None) => build_method_type_from_descriptor(ctx, "()V"),
        Err(e) => Err(e),
    };
    // `build_method_type_from_descriptor` allocates too; re-read `mh` once
    // more before its final use, then release the whole pinned batch.
    let mh = ctx.read_native_pin(mh_pin, mh);
    if let Ok(Some(mt)) = mt_opt {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    mh_record_instance_owner(&*ctx, mh, instance_owner);
    ctx.unpin_native_roots(mh_pin);
    Ok(mh)
}

/// `CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS` (default OFF, opt-in, performance;
/// interpreter round i3 wave 71, lane L4; proposal
/// `i70-L4-proposal-finder-handles-share-their-member-words-20261103`).
pub(crate) fn mh_finder_shared_member_words_enabled() -> bool {
    crate::nbflags().mh_finder_shared_member_words
}

/// Does a mint of `kind` share its member words (wave 71)? Field accessors
/// only (`GETTER` / `SETTER`: `findGetter`, `findSetter`, the static finders,
/// `unreflectGetter` / `unreflectSetter`, a `VarHandle`'s `toMethodHandle`),
/// under the switch. Their `MH_CLASS` / `MH_NAME` / `MH_DESC` are a field's
/// owner, name and descriptor -- names HotSpot interns -- and every reader of
/// those slots reads the text or the word, never writes through the object
/// (`bindTo`'s shared mint already hands the same three objects to every
/// bound copy). Their `type` comes from the shared table: HotSpot's
/// `MethodType`s are interned too (`MethodType.methodType` answers one object
/// per type), so two finder handles of one field answer `a.type() == b.type()`
/// there as well.
pub(crate) fn mh_finder_shares_member_words(kind: i32) -> bool {
    (kind == MH_KIND_GETTER || kind == MH_KIND_SETTER) && mh_finder_shared_member_words_enabled()
}

/// A member string of a fresh handle: the pooled object when `shared`
/// (wave 71, [`mh_finder_shares_member_words`]), else `create_string`'s
/// (a pool hit, or a fresh string). A GC point either way.
pub(crate) fn mh_member_string(ctx: &mut dyn NativeContext, text: &str, shared: bool) -> ObjectRef {
    if shared {
        ctx.intern_string(text)
    } else {
        ctx.create_string(text)
    }
}

/// A classless, descriptorless synthetic `MethodHandle` of `kind` named `name`
/// whose real-JDK `type` field is the caller's `MethodType` `mt` (when there is
/// one), with the spread count `spread` in `MH_NAME` (`spreadInvoker` only).
///
/// gc-common w19-c (stale-handle triage): `MethodHandles.exactInvoker` /
/// `invoker` / `spreadInvoker` and `CallSite.makeUninitializedCallSite` read
/// the `MethodType` out of `args` -- a copy no collection updates -- AFTER
/// [`alloc_method_handle`], which builds a `MethodType` of its own (the GC
/// point its own comments name). A collection there stored a vacated address
/// into the new handle's `type`. `mt` is rooted across the allocation now, and
/// the handle across the count's string.
pub(crate) fn mh_alloc_typed(
    ctx: &mut dyn NativeContext,
    name: &str,
    kind: i32,
    mt: Option<ObjectRef>,
    spread: Option<i32>,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let mt_h = mt.map(|m| scope.root(m));
    let mh: ObjectRef = alloc_method_handle(&mut *scope, "", name, "", kind)?;
    if let Some(h) = &mt_h {
        let mt_now = scope.get(h);
        scope.set_field_by_name(mh, "type", Value::Object(Some(mt_now)));
    }
    let mh_h = scope.root(mh);
    if let Some(n) = spread {
        let s = scope.create_string(&n.to_string());
        let mh_now = scope.get(&mh_h);
        scope.set_field(mh_now, MH_NAME, Value::Object(Some(s)));
    }
    let mh = scope.get(&mh_h);
    Ok(Some(Value::Object(Some(mh))))
}

/// The `MH_KIND_INVOKER` handle `MethodHandles.exactInvoker(t)` /
/// `invoker(t)` / `spreadInvoker(t, lead)` return (interpreter round i1 wave
/// 27, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// Its `type` is the INVOKER's own ([`invoker_type_descriptor`]: HotSpot's
/// `Invokers.invokerType(t)` = `t.insertParameterTypes(0, MethodHandle.class)`,
/// and for a spread invoker the trailing parameters replaced by one
/// `Object[]`), and the TARGET's descriptor is kept in `MH_DESC`, where the
/// dispatch arm reads the spread arity. The handle used to carry `t` itself
/// as its `type` (`mh_alloc_typed`), so `type()` was one parameter short and
/// every adapter chained on an invoker (`foldArguments(exactInvoker(t), ..)`,
/// Groovy's shape) computed its own `type` one parameter off.
///
/// A `t` this file cannot read back into a descriptor (or a `lead` outside
/// `0..=t.parameterCount()`) keeps the old handle: `type` = `t`, `MH_DESC`
/// empty.
///
/// `exact` (wave 28): `exactInvoker`'s handle, marked
/// [`MH_MARK_EXACT_INVOKER`] so the dispatch arm checks its target's type
/// instead of converting it. Only a handle whose own `type` was installed
/// keeps `MH_DESC` (and so is checked, [`invoker_has_own_type`]); one whose
/// own `MethodType` could not be built drops it, and dispatches its target
/// as it did before wave 28.
pub(crate) fn mh_alloc_invoker(
    ctx: &mut dyn NativeContext,
    mt: Option<ObjectRef>,
    spread: Option<i32>,
    exact: bool,
) -> MethodCallResult {
    let target_desc = mt.and_then(|m| methodtype_to_descriptor(ctx, m));
    let own_desc = target_desc
        .as_deref()
        .and_then(|d| invoker_type_descriptor(d, spread));
    let (Some(target_desc), Some(own_desc)) = (target_desc, own_desc) else {
        return mh_alloc_typed(ctx, "", MH_KIND_INVOKER, mt, spread);
    };
    // `mt` is not read after this point: both descriptors are strings.
    let mh = alloc_method_handle(ctx, "", "", &target_desc, MH_KIND_INVOKER)?;
    let mh_pin = ctx.pin_native_root(mh);
    let own = match build_method_type_from_descriptor(ctx, &own_desc) {
        Ok(own) => own,
        Err(e) => {
            ctx.unpin_native_roots(mh_pin);
            return Err(e);
        }
    };
    match own {
        Some(own) => {
            let mh = ctx.read_native_pin(mh_pin, mh);
            ctx.set_field_by_name(mh, "type", Value::Object(Some(own)));
            if exact {
                mh_mark_exact_invoker(&*ctx, mh);
            }
        }
        // `type` is then the TARGET's (`alloc_method_handle` built it from
        // `target_desc`): without its own type the handle must not claim
        // one, so it takes the old shape's empty `MH_DESC` (the spread arm
        // then reads the target arity from `type`, which is `t`).
        None => {
            let mh = ctx.read_native_pin(mh_pin, mh);
            ctx.set_field(mh, MH_DESC, Value::Object(None));
        }
    }
    if let Some(n) = spread {
        // The spread count, as `mh_alloc_typed` records it.
        let s = ctx.create_string(&n.to_string());
        let mh = ctx.read_native_pin(mh_pin, mh);
        ctx.set_field(mh, MH_NAME, Value::Object(Some(s)));
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    Ok(Some(Value::Object(Some(mh))))
}

/// An invoker's own type descriptor for the target descriptor `target`:
/// `(Ljava/lang/invoke/MethodHandle;` + the target's parameters + `)R`, or,
/// for `spreadInvoker(t, lead)`, the handle, the first `lead` parameters and
/// one `[Ljava/lang/Object;`. `None` for an unparseable descriptor or a `lead`
/// outside `0..=t.parameterCount()` (HotSpot refuses that at the factory).
pub(crate) fn invoker_type_descriptor(target: &str, spread: Option<i32>) -> Option<String> {
    let (params, ret) = split_descriptor_params(target)?;
    let mut s = String::from("(Ljava/lang/invoke/MethodHandle;");
    match spread {
        None => s.push_str(&params.concat()),
        Some(lead) => {
            let lead = usize::try_from(lead).ok().filter(|&l| l <= params.len())?;
            s.push_str(&params[..lead].concat());
            s.push_str("[Ljava/lang/Object;");
        }
    }
    s.push(')');
    s.push_str(&ret);
    Some(s)
}

/// Is `mh` an invoker whose `type` is its OWN, `(MethodHandle, t...)R`
/// ([`mh_alloc_invoker`] kept the target type `t` in `MH_DESC`)? Such a
/// `type` is authoritative, so an `invokeExact` call site on the invoker is
/// checked against it ([`exact_call_site_refusal`]) and an `invoke` call site
/// may not narrow it ([`invoke_narrowing_arg_refusal`]), as for the adapter
/// kinds -- interpreter round i1 wave 28, lane L4. An old-shape invoker
/// (`type` = `t`, no `MH_DESC`) is not checked: its `type` is one parameter
/// short.
pub(crate) fn invoker_has_own_type(ctx: &dyn NativeContext, mh: ObjectRef, kind: i32) -> bool {
    kind == MH_KIND_INVOKER && mh_read_desc(ctx, mh).is_some_and(|d| d.starts_with('('))
}

/// HotSpot's `NullPointerException` for a null target handle: the invoker's
/// `LambdaForm` asks the target for its `type()` (an exact invoker,
/// `Invokers.checkExactType`) or `asType(t)` (a plain or spread invoker,
/// `checkGenericType`), with the helpful message naming that call (measured on
/// 25.0.3, `L4W28InvokerChecks`). This arm used to answer `null` without
/// running anything.
pub(crate) fn invoker_null_target(exact: bool) -> MethodCallFailed {
    let called = if exact {
        "type()"
    } else {
        "asType(java.lang.invoke.MethodType)"
    };
    RuntimeError::NullPointerException {
        message: Some(format!(
            "Cannot invoke \"java.lang.invoke.MethodHandle.{called}\" because \"mh\" is null"
        )),
    }
    .into()
}

/// The `MH_KIND_INVOKER` arm's call of its target (interpreter round i1 wave
/// 28, lane L4;
/// `docs/feature-designs/profile-guided-inlining.md`),
/// `t` being the target type the invoker was minted for:
///
/// * an EXACT invoker is `target.invokeExact(args)` at the call-site type `t`
///   ([`mh_invoke_exact_door`]), so a target of another type throws
///   `WrongMethodTypeException: handle's method type (Object[])String but
///   found (Object[])Object`, as HotSpot's `Invokers.checkExactType` does;
/// * a plain or spread invoker is `target.invoke(args)` there: a target of
///   type `t` is dispatched as before, one whose type converts to `t` runs
///   through [`mh_invoke_door`] (the `asType(t)` conversions: an `int` leaf
///   behind an `Object` return is boxed, a `long` return widened), and one
///   that does not throws `asType`'s `cannot convert` refusal. A target whose
///   kind has no measured `type` (`kind_has_authoritative_adapted_type`), or
///   whose `type` is unknown, keeps the bare dispatch.
///
/// The answer is what the invoker's own leaf would give its door: raw for a
/// primitive `t` return (the doors box it, which the invoker's door would
/// then fail to widen against its own call site), else as the door left it.
/// `tail` is the invoker arm's [`MH_TAIL`] token. The bare dispatch passes it
/// on (the invoker's door converts the target's result as its own leaf's);
/// a door is run tail-capable only under a reference token and a reference
/// `t` return, where this hands the door's reference back unchanged (a
/// primitive one is unboxed here: post-call work).
pub(crate) fn invoker_call_target(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    args: &[Value],
    t: String,
    exact: bool,
    tail: u8,
) -> MethodCallResult {
    // `CRATONVM_MH_STRICT_INVOKEEXACT=0` restores the bare dispatch, both
    // kinds of invoker.
    if !mh_strict_exact_signature() {
        set_mh_tail(tail);
        return mh_dispatch(ctx, target, args);
    }
    let t_ret = return_type_desc(&t).to_string();
    let door_tail_ok = tail == b'L' && door_tail_token(&t_ret) == b'L';
    let result = if exact {
        mh_invoke_exact_door(ctx, target, args, Some(&t), door_tail_ok)?
    } else {
        let kind = match ctx.get_field(target, MH_KIND) {
            Value::Int(k) => k,
            _ => MH_KIND_VIRTUAL,
        };
        match mh_declared_descriptor(ctx, target) {
            Some(declared)
                if declared != t
                    && declared != "()V"
                    && kind_has_authoritative_adapted_type(kind) =>
            {
                if let Some(refusal) = mh_astype_refusal_desc(ctx, target, &declared, &t) {
                    return Err(refusal);
                }
                mh_invoke_door(ctx, target, args, Some(&t), door_tail_ok)?
            }
            // Wave 58 (lane L4): a target AT the invoker's type that is an
            // `asType` copy of a direct handle with a reference leaf return
            // still owes its own unboxing (`invoke_return_unbox`'s INNER
            // conversion): `invoker(()int).invokeExact(objLong.asType(()int))`
            // is HotSpot's `ClassCastException`.
            Some(declared)
                if invoker_target_owes_return_unbox(&*ctx, target, kind, &declared, &t) =>
            {
                // Read before the dispatch, which may move `target`.
                let leaf = mh_read_desc(ctx, target).unwrap_or_default();
                let marks = mh_marking_bits(&*ctx, target);
                let result = mh_dispatch(ctx, target, args);
                match invoke_return_unbox(
                    ctx,
                    &result,
                    marks,
                    kind,
                    &leaf,
                    Some(&declared),
                    Some(&t),
                ) {
                    Some(answer) => answer?,
                    None => return result,
                }
            }
            _ => {
                set_mh_tail(tail);
                return mh_dispatch(ctx, target, args);
            }
        }
    };
    Ok(invoker_result_unboxed(&*ctx, result, &t_ret))
}

/// Could the bare dispatch of an invoker's target of type `t` owe
/// [`invoke_return_unbox`]'s INNER conversion? A direct kind whose declared
/// type IS `t` with a primitive return, that an `asType` stamped (its
/// [`MH_CAST_CHAIN`] slot holds the stamp's record: one field read, so an
/// unstamped direct handle -- the common target -- keeps the bare, tail-capable
/// dispatch), under `CRATONVM_MH_INVOKE_RETURN_UNBOX`. Whether its LEAF returns
/// a reference is [`invoke_return_unbox`]'s to ask. Interpreter round i2 wave
/// 58, lane L4.
pub(crate) fn invoker_target_owes_return_unbox(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    kind: i32,
    declared: &str,
    t: &str,
) -> bool {
    let ret = return_type_desc(t);
    declared == t
        && ret.len() == 1
        && ret != "V"
        && kind_has_authoritative_type(kind)
        && crate::nbflags().mh_invoke_return_unbox
        && ctx.object_num_fields(target) > MH_CAST_CHAIN
        && matches!(ctx.get_field(target, MH_CAST_CHAIN), Value::Object(Some(_)))
}

/// A door's answer for an invoker whose target type returns `t_ret`: a
/// wrapper unboxed (and widened) to a primitive `t_ret`, `null` or nothing
/// for one its zero (`asType`'s `void` -> primitive); anything else as is.
pub(crate) fn invoker_result_unboxed(
    ctx: &dyn NativeContext,
    result: Option<Value>,
    t_ret: &str,
) -> Option<Value> {
    if wrapper_desc_for_primitive(t_ret).is_none() {
        return result;
    }
    match result {
        Some(Value::Object(Some(obj))) => match crate::lang_class::unbox_value(ctx, obj) {
            Value::Object(o) => Some(Value::Object(o)),
            raw => Some(widen_primitive_to_descriptor(raw, t_ret)),
        },
        Some(Value::Object(None)) | None => Some(mh_zero_value(t_ret)),
        raw => raw.map(|v| widen_primitive_to_descriptor(v, t_ret)),
    }
}

/// Round-9 perf: render a `Value` into its Java `String.valueOf(...)`
/// representation for the StringConcatFactory MH dispatch. Handles the
/// common 80% of cases — primitives and String/CharSequence/Object via
/// `Object.toString` (best-effort).  Non-string Objects fall back to a
/// label of the form `Class@hash` to keep concatenation lossless.
pub(crate) fn string_concat_render_value(ctx: &mut dyn NativeContext, v: Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj)) => {
            // Fast path: already a String.
            if let Some(s) = ctx.read_string(obj) {
                return s;
            }
            // `java.nio.file.Path` is a genuine interface with no `toString()`
            // body of its own; `ctx.invoke_virtual` below doesn't consult
            // `force_native_over_real_jdk_bytecode`/the `vm_exec.rs`
            // `check_override` allow-list the way the bytecode interpreter's
            // own `invokevirtual` handling does, so it silently resolves to
            // `Object.toString()` for `"literal" + aPath` string
            // concatenation, printing `java.nio.file.Path@<hash>` instead of
            // the real path text. Same family as
            // `path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
            // a third, distinct call site (this is the actual live
            // `MH_KIND_STRING_CONCAT` dispatch path — `vm/src/runtime/invokedynamic.rs`'s
            // own `execute_string_concat`/`value_to_string` has the identical
            // fix for whatever shapes still reach that older code path).
            // Route through the same display-string helper the registered
            // `Path.toString()` native itself uses, bypassing `invoke_virtual`
            // entirely for this type.
            let cid = ctx.class_id_of_object(obj);
            // Not for an array: its header carries its COMPONENT's class id,
            // so a `Path[]` passed the test below and had its "path" rendered.
            let is_array = matches!(ctx.heap_kind_of(obj), ObjectKind::Array);
            if let Some(path_id) = ctx
                .class_id_by_name("java/nio/file/Path")
                .filter(|_| !is_array)
            {
                if ctx.is_subclass(cid, path_id) {
                    return crate::phases_late::p57_path_display_string(ctx, obj);
                }
            }
            // Best-effort: call Object.toString(); if it returns a String,
            // unwrap it. Failure modes fall through to the class@hash form.
            //
            // GC-safety: `invoke_virtual` runs the object's real `toString()`,
            // which can trigger a collection that relocates `obj`; pin it and
            // re-read the forwarded reference before the fallback reads below.
            let obj_pin = ctx.pin_native_root(obj);
            let result = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            if let Ok(Some(Value::Object(Some(s_obj)))) = result {
                if let Some(s) = ctx.read_string(s_obj) {
                    return s;
                }
            }
            let cid = ctx.class_id_of_object(obj);
            let cls = ctx.class_name_of_id(cid).unwrap_or_else(|| "?".to_string());
            format!("{}@{:x}", cls, ctx.identity_hash_code(obj))
        }
        // ReturnAddress / Uninitialized aren't legal Java values reachable from
        // bytecode-level concat sites, but exhaustive match avoids a future-
        // proofing hazard. Render as a debug placeholder.
        Value::ReturnAddress(pc) => format!("returnAddress@{}", pc),
        Value::Uninitialized => "uninitialized".to_string(),
    }
}

/// Round-9: build a real StringConcatFactory target MH.
///
/// The MH dispatches to the `MH_KIND_STRING_CONCAT` arm in `mh_dispatch`,
/// which walks `recipe` and substitutes dynamic args (`\u{0001}`) and
/// pre-baked constants (`\u{0002}`). `constants` may be `None` for the
/// simple `makeConcat` form (no constants in the recipe).
///
/// Compromise: covers the simple-recipe path used by ~80% of javac-
/// emitted concatenations. Complex cases involving non-default MethodType
/// adaptations still fall back to per-arg `toString` rendering via
/// `string_concat_render_value`, which is correct but not bit-perfect for
/// every edge case (e.g. locale-sensitive `Float.toString`).
pub(crate) fn alloc_string_concat_method_handle(
    ctx: &mut dyn NativeContext,
    recipe: &str,
    constants: Option<cratonvm_types::ObjectRef>,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // Reuse the MethodHandle synthetic skeleton — same field layout AND the
    // same width as alloc_method_handle (W7-19 §5.2), but the class slot
    // carries the recipe string instead of a class name.
    // gen r4w3/rooting: `constants` is stored into the holder after two
    // `try_alloc_concurrent_synthetic` calls (GC points); pin it now. Pinned
    // before `mh_pin`, so it is released explicitly at the end.
    let constants_pin = constants.map(|c| ctx.pin_native_root(c));
    let mh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_VARARGS + 1)?;
    // GC-safety: see `alloc_method_handle` above -- the same triple-
    // `create_string` + subsequent-allocation shape, on the same
    // MethodHandle-skeleton object. Pin everything and re-read the
    // forwarded reference right before each use.
    let mh_pin = ctx.pin_native_root(mh);
    let cls = ctx.create_string(recipe);
    let cls_pin = ctx.pin_native_root(cls);
    let nm = ctx.create_string("concat");
    let nm_pin = ctx.pin_native_root(nm);
    let dc = ctx.create_string("()Ljava/lang/String;");
    let mh = ctx.read_native_pin(mh_pin, mh);
    let cls = ctx.read_native_pin(cls_pin, cls);
    let nm = ctx.read_native_pin(nm_pin, nm);
    ctx.set_field(mh, MH_CLASS, Value::Object(Some(cls)));
    ctx.set_field(mh, MH_NAME, Value::Object(Some(nm)));
    ctx.set_field(mh, MH_DESC, Value::Object(Some(dc)));
    ctx.set_field(mh, MH_KIND, Value::Int(MH_KIND_STRING_CONCAT));
    // Same reason as `alloc_method_handle`: never leave `MH_VARARGS` unwritten.
    mh_write_marking_bits(&*ctx, mh, 0);
    // Wrap the constants array in a 1-field holder so MH_BOUND is a single
    // ObjectRef (the rest of mh_dispatch assumes that shape).
    let holder =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/StringConcatFactory$Const", 1)?;
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.set_field(
        holder,
        0,
        match (constants, constants_pin) {
            (Some(arr), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, arr))),
            _ => Value::Object(None),
        },
    );
    ctx.set_field(mh, MH_BOUND, Value::Object(Some(holder)));
    // Populate the real-JDK `type:MethodType` field at slot 0 so JDK-internal
    // `mh.type()` walks see a non-null MethodType.
    let mt_opt = build_method_type_from_descriptor(ctx, "()Ljava/lang/String;");
    let mh = ctx.read_native_pin(mh_pin, mh);
    if let Ok(Some(mt)) = mt_opt {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    // gen r4w3/rooting: `constants_pin` (when present) is the lowest handle.
    ctx.unpin_native_roots(constants_pin.unwrap_or(mh_pin));
    Ok(mh)
}

/// The member of a DIRECT `findStatic` / `findVirtual` handle VALUE, as the
/// `CONSTANT_MethodHandle` that would name it: `(reference kind, class, name,
/// descriptor)`, the kind by JVMS table 5.4.3.5 (6 `invokeStatic`, 5
/// `invokeVirtual`, 9 `invokeInterface` for an interface's method, as
/// `revealDirect` reports it). `None` for any other handle: another kind, a
/// bound one, an `asType`-adapted copy (its `type` is not the member's own),
/// or an object without this model's slots. Interpreter round i1 wave 42,
/// lane L4: a dynamic constant that answers a lambda site's implementation
/// handle (`interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`,
/// the first stage of `i41-L4-proposal-native-linkages-read-a-method-handle-value-back`).
pub fn method_handle_value_member(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<(u8, String, String, String)> {
    // A real JDK `DelegatingMethodHandle` (an `IntrinsicMethodHandle` a
    // condy's bootstrap built) cracks as its target (`internalMemberName()`
    // delegates), so it is read back as its target (wave 45). Then the width
    // guard every reader of this model's slots makes: a real JDK handle
    // (a `DirectMethodHandle`) has none of them, and this function read
    // `MH_KIND` / `MH_BOUND` out of its bounds until wave 45.
    let mh = mh_door_handle(ctx, mh);
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    // A constructor handle's `MH_BOUND` holds its class id as an `Int` (see
    // `findConstructor`), never an object: only an object there is a binding.
    if !matches!(
        kind,
        MH_KIND_STATIC | MH_KIND_VIRTUAL | MH_KIND_CONSTRUCTOR | MH_KIND_SPECIAL
    ) || matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
    {
        return None;
    }
    let class = mh_read_class(&*ctx, mh)?;
    let name = mh_read_name(&*ctx, mh)?;
    let desc = mh_read_desc(&*ctx, mh)?;
    if class.is_empty() || class.starts_with('[') || name.is_empty() {
        return None;
    }
    // Wave 44 (row 4 of
    // `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`): a
    // `findSpecial` handle of a PRIVATE method of its own `specialCaller`
    // (`REF_invokeSpecial`, which `AbstractValidatingLambdaMetafactory` links
    // as `REF_invokeVirtual` when the lambda's class declares it; measured,
    // `L4W44LambdaSiteSpecialHandle` row `private`). A special handle of
    // another class (a `super` call) is not read back: HotSpot links it and
    // its `get()` then throws a `WrongMethodTypeException` this model does
    // not reproduce, so it keeps its old answer.
    if kind == MH_KIND_SPECIAL {
        let own_caller = ctx.object_num_fields(mh) > MH_SPECIAL_CALLER
            && !matches!(ctx.get_field(mh, MH_SPECIAL_CALLER), Value::Object(Some(_)));
        let private = ctx.class_id_by_name(&class).is_some_and(|cid| {
            ctx.declared_methods(cid).iter().any(|m| {
                m.name == name && m.descriptor == desc && m.access_flags & LK_ACC_PRIVATE != 0
            })
        });
        let own_type = direct_handle_own_type(MH_KIND_VIRTUAL, &class, &desc)?;
        if !own_caller || !private || mh_declared_descriptor(ctx, mh)? != own_type {
            return None;
        }
        return Some((7, class, name, desc));
    }
    // The direct handle's own type; any other is an adapter's.
    let own_type = direct_handle_own_type(kind, &class, &desc)?;
    if mh_declared_descriptor(ctx, mh)? != own_type {
        return None;
    }
    // Wave 43: a `findConstructor` handle (`REF_newInvokeSpecial`, its member
    // `<init>` with the `(..)V` descriptor, as the constant would name it).
    if kind == MH_KIND_CONSTRUCTOR {
        return (name == "<init>").then_some((8, class, name, desc));
    }
    let ref_kind = if kind == MH_KIND_STATIC {
        6
    } else if ctx
        .class_id_by_name(&class)
        .is_some_and(|cid| ctx.is_interface_class(cid))
    {
        9
    } else {
        5
    };
    Some((ref_kind, class, name, desc))
}

/// The `type` a DIRECT handle of `kind` over the member `class` / `desc` has:
/// a virtual handle's receiver leads, a constructor returns its class
/// (`alloc_method_handle`'s own rendering), a static handle is its descriptor.
/// Any other `type` on such a shim is an `asType` adapter's copy.
pub(crate) fn direct_handle_own_type(kind: i32, class: &str, desc: &str) -> Option<String> {
    match kind {
        MH_KIND_VIRTUAL => {
            let rest = desc.strip_prefix('(')?;
            Some(format!("(L{class};{rest}"))
        }
        MH_KIND_CONSTRUCTOR => {
            let params = desc.strip_suffix('V')?;
            Some(format!("{params}L{class};"))
        }
        _ => Some(desc.to_string()),
    }
}

/// `MethodHandle.toString()` of a handle VALUE that HotSpot's
/// `Lookup.revealDirect` certainly cannot crack (`"MethodHandle" +
/// type()`, `MethodHandle()String`), or `None` when it may be direct or this
/// model cannot tell. Certainly not direct: a `bindTo` of a static or virtual
/// handle, an `asType` copy of one whose `type` is not its member's own (not
/// a variable-arity collector, whose adaptations this does not model), and
/// the combinator kinds (`insertArguments`, `dropArguments`,
/// `permuteArguments`, `filterArguments`, `foldArguments`, `guardWithTest`,
/// `constant`, `identity`, `catchException`, `filterReturnValue`,
/// `collectArguments`, `tryFinally`), which HotSpot builds as bound or
/// LambdaForm handles with no member. `AbstractValidatingLambdaMetafactory`
/// turns the failure into `LambdaConversionException: <this> is not direct or
/// cannot be cracked` (measured, HotSpot 25). Interpreter round i1 wave 43,
/// lane L4 (row 4 of
/// `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`).
pub fn method_handle_value_not_direct(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<String> {
    // Width-guarded, as `MH_VARARGS` asks: not every handle has these slots.
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    let not_direct = match kind {
        MH_KIND_STATIC | MH_KIND_VIRTUAL => {
            if matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_))) {
                true
            } else if mh_is_varargs_collector(&*ctx, mh) {
                return None;
            } else {
                let class = mh_read_class(&*ctx, mh)?;
                let desc = mh_read_desc(&*ctx, mh)?;
                let own_type = direct_handle_own_type(kind, &class, &desc)?;
                mh_declared_descriptor(ctx, mh)? != own_type
            }
        }
        MH_KIND_INSERT
        | MH_KIND_DROP
        | MH_KIND_PERMUTE
        | MH_KIND_FILTER
        | MH_KIND_FOLD
        | MH_KIND_GUARD
        | MH_KIND_CONSTANT
        | MH_KIND_IDENTITY
        | MH_KIND_CATCH
        | MH_KIND_RETURN_FILTER
        | MH_KIND_COLLECT_ARGS
        | MH_KIND_TRY_FINALLY
        | MH_KIND_TABLE_SWITCH
        | MH_KIND_THROW => true,
        _ => false,
    };
    if !not_direct {
        return None;
    }
    let declared = mh_declared_descriptor(ctx, mh)?;
    let shown = method_type_display_in(&*ctx, &declared)?;
    Some(format!("MethodHandle{shown}"))
}

/// Read the class name string from a MethodHandle (field MH_CLASS).
pub(crate) fn mh_read_class(
    ctx: &dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
) -> Option<String> {
    match ctx.get_field(mh, MH_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// The declared exception classes (internal names) of a DIRECT handle's
/// member, as JDK 25 `VarHandles.exceptionTypes` reads them for
/// `SharedUtils.checkExceptions` (`Linker.upcallStub`), or `None` where
/// HotSpot answers "unknown": a bound or `asType`-adapted handle is a
/// `BoundMethodHandle` there. Round 13 wave 1 (lane mhffm). Only the kinds
/// whose `type` this model can predict (`findStatic` / `findVirtual`); a
/// special or constructor handle, or a `type` that does not render, is
/// `None` too (an accept, as before).
pub(crate) fn mh_direct_member_exceptions(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<Vec<String>> {
    // Round 19: a REAL JDK handle (every `--jdk-only` upcall target) has none
    // of this model's slots; reading them warned `field index OOB` on each
    // `upcallStub`. Its exceptions are the JDK's own `checkExceptions`'.
    if mh_is_real_handle(&*ctx, mh) {
        return None;
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    if !matches!(kind, MH_KIND_STATIC | MH_KIND_VIRTUAL)
        || matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
    {
        return None;
    }
    let class = mh_read_class(&*ctx, mh)?;
    let name = mh_read_name(&*ctx, mh)?;
    let desc = mh_read_desc(&*ctx, mh)?;
    // The direct handle's own type; any other is an adapter's.
    let own_type = if kind == MH_KIND_VIRTUAL {
        let rest = desc.strip_prefix('(')?;
        format!("(L{class};{rest}")
    } else {
        desc.clone()
    };
    if mh_declared_descriptor(ctx, mh)? != own_type {
        return None;
    }
    let owner = ctx.class_id_by_name(&class)?;
    Some(ctx.method_exceptions(owner, &name, &desc))
}

/// Round 14 wave 1 (lane ffm; `r13w13-ffm7-upcall-per-call-cost`): a PLAIN
/// direct static handle -- `findStatic`, nothing bound, no marking, its
/// `type` its member's own -- as `(class, name, desc, recorded owner)`, which
/// [`mh_dispatch`]'s static arm re-derives on every call. With no entry call
/// site and no tail token armed (an upcall arms neither), such a handle takes
/// exactly `invoke_static_settling` with these through that arm:
/// `collect_trailing_varargs` and `adapt_leaf_args` are identities on the
/// exact types, and `direct_return_boxes` is false (the type's return is the
/// member's). `None` for any other handle (the caller keeps `mh_dispatch`).
/// Field and string reads only: no GC point.
pub(crate) fn mh_plain_static_leaf(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<(String, String, String, Option<ClassId>)> {
    if crate::panama::is_downcall_handle(&*ctx, mh)
        || mh_is_real_handle(&*ctx, mh)
        || mh_marking_bits(&*ctx, mh) & MH_MARKS_DIVERT_DIRECT_LANE != 0
    {
        return None;
    }
    if !matches!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_STATIC)) {
        return None;
    }
    let bound = ctx.get_field(mh, MH_BOUND);
    if matches!(bound, Value::Object(Some(_))) {
        return None;
    }
    let class = mh_read_class(&*ctx, mh)?;
    let name = mh_read_name(&*ctx, mh)?;
    let desc = mh_read_desc(&*ctx, mh)?;
    if mh_declared_descriptor(ctx, mh)? != desc {
        return None;
    }
    Some((class, name, desc, mh_static_owner(bound)))
}

/// Read the method name string from a MethodHandle (field MH_NAME).
pub(crate) fn mh_read_name(
    ctx: &dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
) -> Option<String> {
    match ctx.get_field(mh, MH_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the descriptor string from a MethodHandle (field MH_DESC).
pub(crate) fn mh_read_desc(
    ctx: &dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
) -> Option<String> {
    // A real JDK handle is narrower than this model's layout: no `MH_DESC`
    // (the read was a heap OOB warning per call; wave 56, lane L4).
    if ctx.object_num_fields(mh) <= MH_DESC {
        return None;
    }
    match ctx.get_field(mh, MH_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Extract the return type of a method descriptor as an internal class name
/// (only for reference returns; `None` for primitive/void). E.g.
/// `(I)Ljava/util/function/Consumer;` → `Some("java/util/function/Consumer")`.
pub(crate) fn descriptor_return_internal_name(desc: &str) -> Option<String> {
    let ret = &desc[desc_close_paren(desc)? + 1..];
    if ret.starts_with('L') && ret.ends_with(';') {
        Some(ret[1..ret.len() - 1].to_string())
    } else {
        None
    }
}

/// One type char per descriptor parameter (`'L'` for any reference/array,
/// the primitive char otherwise) — the `capture_types` convention used by the
/// `invokedynamic` lambda bootstrap (`parse_descriptor_args`).
pub(crate) fn descriptor_param_chars(desc: &str) -> String {
    let mut out = String::new();
    let bytes = desc.as_bytes();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
    }
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'L' => {
                out.push('L');
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                out.push('L');
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
            c @ (b'J' | b'D' | b'F' | b'I' | b'B' | b'C' | b'S' | b'Z') => {
                out.push(c as char);
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Map our internal `MH_KIND_*` to a JVMS `reference_kind` byte (1..=9) for
/// `MethodHandleKind::from_tag`. Interface and virtual both map to
/// `REF_invokeVirtual` (5); the interpreter's lambda dispatch treats them
/// identically.
pub(crate) fn mh_kind_to_ref_kind(mh_kind: i32) -> u8 {
    match mh_kind {
        MH_KIND_STATIC => 6,      // REF_invokeStatic
        MH_KIND_VIRTUAL => 5,     // REF_invokeVirtual
        MH_KIND_SPECIAL => 7,     // REF_invokeSpecial
        MH_KIND_CONSTRUCTOR => 8, // REF_newInvokeSpecial
        MH_KIND_GETTER => 1,      // REF_getField
        MH_KIND_SETTER => 3,      // REF_putField
        _ => 6,
    }
}

/// Allocate a *frozen* `ConstantCallSite` wrapping `target_mh`.
///
/// The real JDK `ConstantCallSite.getTarget()` throws a bare
/// `IllegalStateException` unless `isFrozen` is set — normally by the
/// constructor, which `alloc_concurrent_synthetic` bypasses. Setting it here
/// lets the genuine `getTarget()` bytecode return the target in real-JDK builds
/// (where the synthetic `getTarget` native is not registered).
pub(crate) fn alloc_frozen_constant_call_site(
    ctx: &mut dyn NativeContext,
    target_mh: cratonvm_types::ObjectRef,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // gen r4w3/rooting: the allocation can run `<clinit>` (a GC point);
    // `target_mh` is stored after it, so pin and re-read.
    let target_pin = ctx.pin_native_root(target_mh);
    let ccs = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/ConstantCallSite", 2)?;
    let target_mh = ctx.read_native_pin(target_pin, target_mh);
    ctx.unpin_native_roots(target_pin);
    ctx.set_field(ccs, 0, Value::Object(Some(target_mh)));
    ctx.set_field_by_name(ccs, "target", Value::Object(Some(target_mh)));
    ctx.set_field_by_name(ccs, "isFrozen", Value::Int(1));
    Ok(ccs)
}

/// Build a real lambda-factory `CallSite` from the reflective
/// `LambdaMetafactory.metafactory` / `altMetafactory` arguments.
///
/// Returns a frozen `ConstantCallSite` whose target is an
/// `MH_KIND_LAMBDA_FACTORY` MethodHandle. Invoking that target (after the
/// caller's `bindTo` captures) allocates a genuine lambda proxy whose SAM is
/// dispatched to `impl_method` by the interpreter — the same machinery the
/// `invokedynamic` opcode uses. Returns `None` (caller falls back to a no-op
/// CallSite) when the arguments are insufficient or the host cannot register a
/// proxy.
pub(crate) fn build_reflective_lambda_callsite(
    ctx: &mut dyn NativeContext,
    // The lookup class (`reflective_lambda_host`): the proxy's host.
    host: Option<ClassId>,
    invoked_type: Option<cratonvm_types::ObjectRef>,
    invoked_name: &str,
    sam_type: Option<cratonvm_types::ObjectRef>,
    impl_method: Option<cratonvm_types::ObjectRef>,
    instantiated_type: Option<cratonvm_types::ObjectRef>,
    // `LambdaMetafactory.FLAG_SERIALIZABLE`, as passed by a reflective
    // `altMetafactory`. Plain `metafactory` has no flags word: `false`.
    serializable: bool,
    // `LambdaMetafactory.FLAG_MARKERS` interfaces (internal names), as passed by
    // a reflective `altMetafactory`. Always empty for plain `metafactory`.
    marker_interfaces: &[String],
    // `LambdaMetafactory.FLAG_BRIDGES` descriptors, as passed by a reflective
    // `altMetafactory`. Always empty for plain `metafactory`.
    bridge_descriptors: &[String],
) -> Result<Option<cratonvm_types::ObjectRef>, MethodCallFailed> {
    let Some(invoked_mt) = invoked_type else {
        return Ok(None);
    };
    let Some(impl_mh) = impl_method else {
        return Ok(None);
    };

    // Factory signature: (captures...)FunctionalInterface.
    let invoked_desc = descriptor_from_method_type(ctx, invoked_mt);
    let Some(functional_interface) = descriptor_return_internal_name(&invoked_desc) else {
        return Ok(None);
    };
    let capture_types = descriptor_param_chars(&invoked_desc);

    // SAM erased descriptor (default to the most common erasure if unreadable).
    let sam_desc = match sam_type {
        Some(mt) => descriptor_from_method_type(ctx, mt),
        None => "()Ljava/lang/Object;".to_string(),
    };
    let inst_desc = match instantiated_type {
        Some(mt) => descriptor_from_method_type(ctx, mt),
        None => sam_desc.clone(),
    };

    // Implementation method coordinates from the impl MethodHandle object.
    let Some(impl_class) = mh_read_class(ctx, impl_mh) else {
        return Ok(None);
    };
    if impl_class.is_empty() {
        return Ok(None);
    }
    let impl_member = mh_read_name(ctx, impl_mh).unwrap_or_default();
    let impl_desc = mh_read_desc(ctx, impl_mh).unwrap_or_default();
    let impl_kind = match ctx.get_field(impl_mh, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_STATIC,
    };
    let impl_ref_kind = mh_kind_to_ref_kind(impl_kind);

    // Register the proxy class + SAM-dispatch metadata in the VM.
    let proxy_cid = ctx.register_lambda_proxy(
        &functional_interface,
        invoked_name,
        &sam_desc,
        &impl_class,
        &impl_member,
        &impl_desc,
        impl_ref_kind,
        &inst_desc,
        &capture_types,
        serializable,
    );
    if proxy_cid == 0 {
        return Ok(None);
    }
    // Its host names it, is its nest host and pins its loader, as the
    // `invokedynamic` bootstrap's host does.
    if let Some(host) = host {
        ctx.register_lambda_proxy_host(proxy_cid, host);
    }
    // The proxy implements its functional interface PLUS every marker. That
    // list is not part of the SAM metadata above, so it rides a second
    // hand-off; without it `(Adder & Cloneable)`-style casts fail.
    ctx.register_lambda_proxy_markers(proxy_cid, marker_interfaces);
    // Likewise the FLAG_BRIDGES descriptors, which lambda dispatch admits
    // beside the SAM descriptor.
    ctx.register_lambda_proxy_bridges(proxy_cid, bridge_descriptors);

    // Factory MethodHandle: MH_CLASS = proxy ClassId (decimal), MH_DESC = the
    // factory signature (so type()/arity and capture-count are derivable).
    let factory_mh = alloc_method_handle(
        ctx,
        &proxy_cid.to_string(),
        invoked_name,
        &invoked_desc,
        MH_KIND_LAMBDA_FACTORY,
    );
    Ok(Some(alloc_frozen_constant_call_site(ctx, factory_mh?)?))
}

/// Core dispatch: given a populated MethodHandle and argument list, invoke it.
/// `extra_args` are the args passed to invoke() after `this` (the MH itself).
/// `MethodHandles.filterArguments` dispatch (`MH_KIND_FILTER`). Extracted from
/// `mh_dispatch`'s match to keep that already-huge function small enough for
/// rustc to compile without overflowing its analysis stack. `bound` is the
/// 3-field wrapper (target MH, filters `MethodHandle[]`, pos).
pub(crate) fn mh_dispatch_filter(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
    tail: u8,
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let filters = ctx.get_field(wrapper, 1);
    let pos = match ctx.get_field(wrapper, 2) {
        Value::Int(p) => p as usize,
        _ => 0,
    };
    let mut filtered: Vec<Value> = extra_args.to_vec();
    // GC-safety: the recursive `mh_dispatch` calls below (both inside the
    // per-filter loop and the final target dispatch) can trigger a
    // collection that relocates `target`; pin it and re-read the forwarded
    // reference before its final use.
    let target_pin = ctx.pin_native_root(target);
    // gen r4w3/rooting: each filter dispatch is a GC point; the filters array,
    // the not-yet-filtered args and the already-filtered results are all read
    // again afterwards. Pin them (a replaced value gets a fresh pin) and
    // re-read before each use.
    let filters_pin = pin_mh_value(ctx, filters);
    let (_, mut handles) = pin_mh_args(ctx, &filtered);
    if let Value::Object(Some(farr0)) = filters {
        let n = ctx.array_length(farr0);
        for i in 0..n {
            let idx = pos + i;
            if idx >= filtered.len() {
                break;
            }
            let farr = ctx.read_native_pin(filters_pin, farr0);
            // A null filter element means "leave this argument unchanged".
            if let Value::Object(Some(filter_mh)) = ctx.get_array_element(farr, i) {
                let arg = read_pinned_mh_arg(ctx, handles[idx], filtered[idx]);
                if let Some(v) = mh_dispatch(ctx, filter_mh, &[arg])? {
                    filtered[idx] = v;
                    handles[idx] = pin_mh_value(ctx, v);
                }
            }
        }
    }
    let filtered = read_pinned_mh_args(ctx, &handles, &filtered);
    let target = ctx.read_native_pin(target_pin, target);
    ctx.unpin_native_roots(target_pin);
    // The target is the adapter's tail call (the filters above were not;
    // see `MH_TAIL`).
    set_mh_tail(tail);
    mh_dispatch(ctx, target, &filtered)
}

/// Build a `MethodHandles.foldArguments` adapter (`MH_KIND_FOLD`). `target` and
/// `combiner` are the two handles; `pos` is the fold position (0 for the basic
/// form). The adapter's `type()` mirrors the target minus the folded result
/// parameter, but for CratonVM's dispatch only the wrapper fields matter.
pub(crate) fn make_fold_adapter(
    ctx: &mut dyn NativeContext,
    target: Option<Value>,
    pos: i32,
    combiner: Option<Value>,
) -> MethodCallResult {
    let target = match target {
        Some(Value::Object(Some(t))) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let combiner_ref = match combiner {
        Some(Value::Object(Some(c))) => c,
        // No combiner → behave like the bare target.
        _ => return Ok(Some(Value::Object(Some(target)))),
    };
    // GC-safety: `alloc_concurrent_synthetic`/`alloc_method_handle` below
    // can trigger a collection that relocates `target`/`combiner_ref`/
    // `wrapper` (each captured/produced above and read again after a
    // later allocation); pin them and re-read the forwarded references
    // before each use.
    let target_pin = ctx.pin_native_root(target);
    let combiner_pin = ctx.pin_native_root(combiner_ref);
    let wrapper = alloc_mh_carrier(ctx, "__mh_fold_wrapper__", 3);
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let target = ctx.read_native_pin(target_pin, target);
    let combiner_ref = ctx.read_native_pin(combiner_pin, combiner_ref);
    ctx.set_field(wrapper, 0, Value::Object(Some(target)));
    ctx.set_field(wrapper, 1, Value::Object(Some(combiner_ref)));
    ctx.set_field(wrapper, 2, Value::Int(pos));
    let desc = mh_type_descriptor(ctx, target)
        .or_else(|| mh_read_desc(ctx, target))
        .unwrap_or_default();
    let adapter = alloc_method_handle(ctx, "__adapter__", "fold", &desc, MH_KIND_FOLD)?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    // gen r4w3/rooting: `target`/`combiner_ref` are read again below, after
    // `alloc_method_handle` (a GC point) — re-read them, and keep `adapter`
    // pinned across `build_method_type_from_descriptor` (also a GC point).
    // `target_pin` (the lowest) is released last, at the end.
    let target = ctx.read_native_pin(target_pin, target);
    let combiner_ref = ctx.read_native_pin(combiner_pin, combiner_ref);
    let adapter_pin = ctx.pin_native_root(adapter);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    // type(): foldArguments REMOVES ONE PARAMETER, at `pos`, from the
    // target's type — the slot the combiner's non-void result fills is
    // synthesized at dispatch, not supplied by the caller (a void combiner
    // contributes nothing, so the type is unchanged). Measured
    // (`p.foldPrefix.type`, `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`): `foldArguments((String,String,String)String,
    // (String)String)` reported `(String,String,String)String` — the
    // target's raw, un-reduced arity — where HotSpot 25 says
    // `(String,String)String`. This already cost a real caller once: SpEL's
    // `FunctionReference` reads a handle's arity to decide whether to
    // re-wrap its arguments, and the stale type made it double-wrap. Chain
    // off the target's ADAPTED type (not `MH_DESC`) so a stack of adapters
    // stays arity-correct, matching every other combinator's `type()` fix
    // in this file.
    let combiner_is_void = mh_type_descriptor(ctx, combiner_ref)
        .or_else(|| mh_read_desc(ctx, combiner_ref))
        .and_then(|d| split_descriptor_params(&d))
        .map(|(_, ret)| ret == "V")
        .unwrap_or(false);
    if !combiner_is_void {
        if let Some(tdesc) = mh_type_descriptor(ctx, target) {
            if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                let p = (pos.max(0) as usize).min(params.len());
                if p < params.len() {
                    params.remove(p);
                }
                let new_desc = format!("({}){}", params.concat(), ret);
                if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &new_desc) {
                    let adapter = ctx.read_native_pin(adapter_pin, adapter);
                    ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                }
            }
        }
    }
    let adapter = ctx.read_native_pin(adapter_pin, adapter);
    ctx.unpin_native_roots(target_pin);
    Ok(Some(Value::Object(Some(adapter))))
}

/// `MethodHandles.foldArguments` dispatch (`MH_KIND_FOLD`). Extracted from
/// `mh_dispatch` to keep that match small enough for rustc. The combiner runs
/// over `combiner.parameterCount()` args starting at `pos`; a non-void result is
/// spliced in at `pos` before the (full) original args, then the target runs.
pub(crate) fn mh_dispatch_fold(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
    tail: u8,
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let combiner = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(c)) => c,
        _ => {
            set_mh_tail(tail);
            return mh_dispatch(ctx, target, extra_args);
        }
    };
    let pos = match ctx.get_field(wrapper, 2) {
        Value::Int(p) => (p as usize).min(extra_args.len()),
        _ => 0,
    };
    // The combiner consumes `combiner.parameterCount()` args starting at `pos`.
    let cdesc = mh_type_descriptor(ctx, combiner)
        .or_else(|| mh_read_desc(ctx, combiner))
        .unwrap_or_default();
    let (cparams, cret) = split_descriptor_params(&cdesc).unwrap_or_default();
    let take = cparams.len().min(extra_args.len().saturating_sub(pos));
    let combine_args: Vec<Value> = extra_args[pos..pos + take].to_vec();
    // GC-safety: this recursive `mh_dispatch` call can trigger a collection
    // that relocates `target` (captured above and dispatched again below);
    // pin it and re-read the forwarded reference before its final use.
    let target_pin = ctx.pin_native_root(target);
    // gen r4w3/rooting: the original args are passed to the target after the
    // combiner dispatch (a GC point); pin them and use the re-read copy.
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let combined = mh_dispatch(ctx, combiner, &combine_args)?;
    let target = ctx.read_native_pin(target_pin, target);
    let extra_args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
    let extra_args = extra_args.as_slice();
    ctx.unpin_native_roots(target_pin);
    // Splice a non-void combiner result in at `pos`; void combiners contribute
    // nothing (the target then sees the original arg list unchanged).
    let mut full: Vec<Value> = Vec::with_capacity(extra_args.len() + 1);
    full.extend_from_slice(&extra_args[..pos]);
    if cret != "V" {
        full.push(combined.unwrap_or(Value::Object(None)));
    }
    full.extend_from_slice(&extra_args[pos..]);
    // The target is the adapter's tail call (see `MH_TAIL`).
    set_mh_tail(tail);
    mh_dispatch(ctx, target, &full)
}

/// Construct a `MethodHandles.collectArguments(target, pos, filter)` adapter
/// (`MH_KIND_COLLECT_ARGS`). Mirrors `make_fold_adapter`'s wrapper shape
/// (target, combiner/filter, pos) -- only the DISPATCH-time splicing differs
/// (replace vs. splice-in-addition; see `MH_KIND_COLLECT_ARGS`'s doc
/// comment).
/// The descriptor `MethodHandles.collectArguments(target, pos, filter)` gives
/// its adapter, per the javadoc:
///
/// * a filter that RETURNS A VALUE consumes the target's parameter at `pos` and
///   supplies it, so the adapter's parameter list is the target's with the one
///   at `pos` REPLACED by the filter's whole parameter list;
/// * a `void` filter consumes nothing, so its parameters are INSERTED at `pos`
///   and the target keeps all of its own.
///
/// The return type is always the target's.
///
/// # Why the adapter's own type has to be right
///
/// `make_collect_args_adapter` used to hand `alloc_method_handle` the TARGET's
/// descriptor unchanged, so the adapter reported the arity it was built to
/// remove. Dispatch was unaffected (`mh_dispatch_collect_args` works off the
/// bound wrapper, not the descriptor), which is why this survived: the handle
/// invoked correctly and only LIED about its type.
///
/// A caller that reads the type back is where it surfaced.
/// `MethodHandles.explicitCastArguments` refuses on an arity mismatch alone,
/// and invokebinder's `Binder.invoke(target)` — which walks its transforms
/// calling each one's `up()` and then explicit-casts the result to the binder's
/// start type — is built entirely on that read. JRuby's
/// `InvokeSite.prepareBinder` folds the flat Ruby arguments into the
/// `IRubyObject[] args` parameter with `SmartBinder.collect(name, pattern,
/// collectorHandle)`, which lowers to exactly this combinator, so every JRuby
/// call site failed to link with
///
/// ```text
/// WrongMethodTypeException: cannot explicitly cast
///   MethodHandle(ThreadContext,IRubyObject,IRubyObject,IRubyObject[])IRubyObject
///   to (ThreadContext,IRubyObject,IRubyObject,IRubyObject,IRubyObject)IRubyObject
/// ```
///
/// — the target arity, uncollected, against the call site's own. That is both
/// `JRubyScriptTemplateTests` classes in the Spring Framework suite, and
/// `probes/MhCombinatorProbe.java`'s `collect-then-explicitCast` row is the
/// three-line version of it.
///
/// `None` when either descriptor cannot be parsed or `pos` is out of range; the
/// caller then keeps the target's descriptor, which is the previous behaviour
/// and no worse than it was.
pub(crate) fn collect_args_adapter_descriptor(
    target_desc: &str,
    filter_desc: &str,
    pos: i32,
) -> Option<String> {
    let (tparams, tret) = split_descriptor_params(target_desc)?;
    let (fparams, fret) = split_descriptor_params(filter_desc)?;
    let pos = usize::try_from(pos).ok()?;
    let mut out: Vec<String> = Vec::with_capacity(tparams.len() + fparams.len());
    if fret == "V" {
        if pos > tparams.len() {
            return None;
        }
        out.extend_from_slice(&tparams[..pos]);
        out.extend(fparams);
        out.extend_from_slice(&tparams[pos..]);
    } else {
        if pos >= tparams.len() {
            return None;
        }
        out.extend_from_slice(&tparams[..pos]);
        out.extend(fparams);
        out.extend_from_slice(&tparams[pos + 1..]);
    }
    Some(format!("({}){}", out.concat(), tret))
}

/// The descriptor of `MethodHandles.filterArguments(target, pos, filters)`.
///
/// JDK contract: the adapter has the target's arity and return type, and the
/// parameter at `pos + i` is replaced by the (sole) parameter type of
/// `filters[i]`; a `None` entry (a null filter) leaves that parameter alone.
///
/// Handing `alloc_method_handle` the target's descriptor unchanged was invisible
/// to the Groovy/JRuby callers this combinator was built for, whose filters all
/// map a type to itself. Netty's `PlatformDependent0` is the caller where it
/// surfaced: `filterArguments(address, 0, ofBuffer)` must be
/// `(Buffer)long`, and its `invokeExact((Buffer) buf)` call site was refused
/// with `expected (MemorySegment)long but found (Buffer)long`.
///
/// `None` when a descriptor cannot be parsed, a filter is not unary, or the
/// filters run past the target's parameters; the caller then keeps the target's
/// descriptor, which is the previous behaviour.
pub(crate) fn filter_args_adapter_descriptor(
    target_desc: &str,
    filter_descs: &[Option<String>],
    pos: i32,
) -> Option<String> {
    let (mut params, ret) = split_descriptor_params(target_desc)?;
    let pos = usize::try_from(pos).ok()?;
    if pos.checked_add(filter_descs.len())? > params.len() {
        return None;
    }
    for (i, fd) in filter_descs.iter().enumerate() {
        if let Some(fd) = fd {
            let (mut fparams, _) = split_descriptor_params(fd)?;
            if fparams.len() != 1 {
                return None;
            }
            params[pos + i] = fparams.pop()?;
        }
    }
    Some(format!("({}){}", params.concat(), ret))
}

pub(crate) fn make_collect_args_adapter(
    ctx: &mut dyn NativeContext,
    target: Option<Value>,
    pos: i32,
    filter: Option<Value>,
) -> MethodCallResult {
    let target = match target {
        Some(Value::Object(Some(t))) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let filter_ref = match filter {
        Some(Value::Object(Some(f))) => f,
        // No filter → behave like the bare target.
        _ => return Ok(Some(Value::Object(Some(target)))),
    };
    // GC-safety: `alloc_concurrent_synthetic`/`alloc_method_handle` below
    // can trigger a collection that relocates `target`/`filter_ref`/
    // `wrapper` (each captured/produced above and read again after a later
    // allocation); pin them and re-read the forwarded references before
    // each use.
    let target_pin = ctx.pin_native_root(target);
    let filter_pin = ctx.pin_native_root(filter_ref);
    let wrapper = alloc_mh_carrier(ctx, "__mh_collect_args_wrapper__", 3);
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let target = ctx.read_native_pin(target_pin, target);
    let filter_ref = ctx.read_native_pin(filter_pin, filter_ref);
    ctx.set_field(wrapper, 0, Value::Object(Some(target)));
    ctx.set_field(wrapper, 1, Value::Object(Some(filter_ref)));
    ctx.set_field(wrapper, 2, Value::Int(pos));
    let target_desc = mh_type_descriptor(ctx, target)
        .or_else(|| mh_read_desc(ctx, target))
        .unwrap_or_default();
    // The ADAPTER's descriptor, not the target's — see
    // `collect_args_adapter_descriptor` for what reads it back and why the
    // difference is not cosmetic. Falls back to the target's when either
    // descriptor is unreadable, which is what this line used to do always.
    let desc = mh_type_descriptor(ctx, filter_ref)
        .or_else(|| mh_read_desc(ctx, filter_ref))
        .and_then(|fd| collect_args_adapter_descriptor(&target_desc, &fd, pos))
        .unwrap_or(target_desc);
    let adapter: ObjectRef = alloc_method_handle(
        ctx,
        "__adapter__",
        "collectargs",
        &desc,
        MH_KIND_COLLECT_ARGS,
    )?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    ctx.unpin_native_roots(target_pin);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

/// `MethodHandles.collectArguments` dispatch (`MH_KIND_COLLECT_ARGS`). The
/// filter consumes `filter.parameterCount()` args starting at `pos`; a
/// non-void result REPLACES that consumed range (unlike `foldArguments`,
/// which keeps the full original list and splices the combiner's result in
/// ADDITION to it) before the target runs.
pub(crate) fn mh_dispatch_collect_args(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
    tail: u8,
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let filter = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(f)) => f,
        _ => {
            set_mh_tail(tail);
            return mh_dispatch(ctx, target, extra_args);
        }
    };
    let pos = match ctx.get_field(wrapper, 2) {
        Value::Int(p) => (p as usize).min(extra_args.len()),
        _ => 0,
    };
    // The filter consumes `filter.parameterCount()` args starting at `pos`.
    let fdesc = mh_type_descriptor(ctx, filter)
        .or_else(|| mh_read_desc(ctx, filter))
        .unwrap_or_default();
    let (fparams, fret) = split_descriptor_params(&fdesc).unwrap_or_default();
    let take = fparams.len().min(extra_args.len().saturating_sub(pos));
    let filter_args: Vec<Value> = extra_args[pos..pos + take].to_vec();
    // GC-safety: this recursive `mh_dispatch` call can trigger a collection
    // that relocates `target` (captured above and dispatched again below);
    // pin it and re-read the forwarded reference before its final use.
    let target_pin = ctx.pin_native_root(target);
    // gen r4w3/rooting: the unconsumed args are passed to the target after
    // the filter dispatch (a GC point); pin them and use the re-read copy.
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let filtered = mh_dispatch(ctx, filter, &filter_args)?;
    let target = ctx.read_native_pin(target_pin, target);
    let extra_args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
    let extra_args = extra_args.as_slice();
    ctx.unpin_native_roots(target_pin);
    // Replace the consumed [pos, pos+take) range with the filter's non-void
    // result (a void filter just consumes the range, contributing nothing).
    let mut full: Vec<Value> = Vec::with_capacity(extra_args.len());
    full.extend_from_slice(&extra_args[..pos]);
    if fret != "V" {
        full.push(filtered.unwrap_or(Value::Object(None)));
    }
    full.extend_from_slice(&extra_args[pos + take..]);
    // The target is the adapter's tail call (see `MH_TAIL`).
    set_mh_tail(tail);
    mh_dispatch(ctx, target, &full)
}

pub(crate) fn mh_exception_matches(
    ctx: &dyn NativeContext,
    thrown: cratonvm_types::ObjectRef,
    catch_type: cratonvm_types::ObjectRef,
) -> bool {
    let catch_id = match mirror_class_id(ctx, catch_type) {
        Some(id) => id,
        None => return false,
    };
    let thrown_id = ctx.class_id_of_object(thrown);
    thrown_id == catch_id || ctx.is_subclass(thrown_id, catch_id)
}

/// `MethodHandles.catchException(target, exType, handler)` refuses when the
/// handler's declared leading parameter is not a supertype of (or equal to)
/// `exType` — javadoc: "the handler must accept a leading parameter of type
/// exType or a supertype". Measured (`g.catchWrongType`,
/// `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`): a handler declared
/// `(IllegalStateException, String)` registered against `ClassCastException`
/// — an unrelated sibling exception type — was silently accepted where
/// HotSpot 25 throws `IllegalArgumentException` at this call. Every way of
/// not resolving both types to a real class hierarchy is an ACCEPT: this only
/// refuses on a positive negative-hierarchy reading, never on an unknown one.
pub(crate) fn mh_catch_exception_type_refusal(
    ctx: &mut dyn NativeContext,
    catch_type: Value,
    handler: Value,
) -> Option<MethodCallFailed> {
    let catch_mirror = match catch_type {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let catch_cid = mirror_class_id(ctx, catch_mirror)?;
    let handler_obj = match handler {
        Value::Object(Some(h)) => h,
        _ => return None,
    };
    // `Throwable.class.isAssignableFrom(exType)` first, as HotSpot asks it
    // (wave 98: the combinator checks above judge only direct handles).
    let throwable = ctx.class_id_by_name("java/lang/Throwable")?;
    if catch_cid != throwable && !ctx.is_subclass(catch_cid, throwable) {
        let name = ctx.class_name_of_id(catch_cid)?.class_text(&*ctx);
        return Some(cratonvm_types::error::RuntimeError::ClassCastException { message: name }.into());
    }
    let hdesc = mh_type_descriptor(ctx, handler_obj).or_else(|| mh_read_desc(ctx, handler_obj))?;
    let (hparams, _) = split_descriptor_params(&hdesc)?;
    // Interpreter round i4 wave 98, lane L4 (probe
    // `tools/probes/interp/L4/L4W98CombinatorTexts.java`): HotSpot's text,
    // `MethodHandles.catchException`'s
    // `newIllegalArgumentException("handler does not accept exception type "+exType)`
    // (`Class.toString`: `class java.lang.ClassCastException`); this read
    // `handler should adapt to java/lang/ClassCastException: (L...;...)...`.
    // A handler with no parameter, or a primitive or array first parameter,
    // cannot accept a `Throwable` either (`htype.parameterCount() < 1 ||
    // !htype.parameterType(0).isAssignableFrom(exType)`); those were accepted.
    let accepts = match hparams.first() {
        None => false,
        Some(first) => match first.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            Some(handler_name) => {
                let handler_cid = ctx.class_id_by_name(handler_name)?;
                catch_cid == handler_cid || ctx.is_subclass(catch_cid, handler_cid)
            }
            None => false,
        },
    };
    if accepts {
        return None;
    }
    let catch_name = ctx.class_name_of_id(catch_cid)?.class_text(&*ctx);
    Some(
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: format!("handler does not accept exception type class {catch_name}"),
        }
        .into(),
    )
}

pub(crate) fn box_direct_primitive_return(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    result: MethodCallResult,
    desc: &str,
) -> MethodCallResult {
    let boxes = direct_return_boxes(ctx, mh, desc);
    box_direct_primitive_return_pre(ctx, boxes, result, desc)
}

/// gen r4w3/rooting: the half of [`box_direct_primitive_return`] that reads
/// `mh`. `mh_dispatch` evaluates it BEFORE the dispatch's GC point (the
/// target invoke / `<clinit>`), because `mh` is an unrooted Rust local and
/// names a vacated from-space address once a young collection has moved it.
/// Pure field reads — not a GC point.
pub(crate) fn direct_return_boxes(ctx: &mut dyn NativeContext, mh: ObjectRef, desc: &str) -> bool {
    let ret_desc = return_type_desc(desc);
    if !matches!(ret_desc, "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C") {
        return false;
    }
    mh_type_returns_reference(ctx, mh)
}

/// Does [`mh_type_descriptor`]'s answer return a reference? The same answer,
/// without rendering the parameters (round 12 wave 7, lane mh5): every
/// primitive-returning leaf dispatch asked this, and building the whole
/// descriptor cost a mirror-to-name resolution and a `String` per parameter.
/// Exactly [`mh_type_descriptor`]'s fallback rule: the `type` is used when its
/// `ptypes` is an array of non-null mirrors (else `MH_DESC`), and a missing
/// `rtype` reads as `void`.
pub(crate) fn mh_type_returns_reference(ctx: &mut dyn NativeContext, mh: ObjectRef) -> bool {
    fn is_reference(d: &str) -> bool {
        matches!(d.as_bytes().first(), Some(b'L') | Some(b'['))
    }
    if let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") {
        if let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") {
            let n = ctx.array_length(ptypes);
            if (0..n).all(|i| matches!(ctx.get_array_element(ptypes, i), Value::Object(Some(_)))) {
                return match ctx.get_field_by_name(mt, "rtype") {
                    Value::Object(Some(r)) => is_reference(&*mirror_to_descriptor(&*ctx, r)),
                    _ => false,
                };
            }
        }
    }
    let desc = mh_read_desc(&*ctx, mh).unwrap_or_default();
    is_reference(return_type_desc(&desc))
}

/// [`box_direct_primitive_return`] with the `mh`-dependent decision
/// (`boxes`, from [`direct_return_boxes`]) already taken.
pub(crate) fn box_direct_primitive_return_pre(
    ctx: &mut dyn NativeContext,
    boxes: bool,
    result: MethodCallResult,
    desc: &str,
) -> MethodCallResult {
    if !boxes {
        return result;
    }
    let ret_desc = return_type_desc(desc);
    // CANONICAL — this is MethodHandle return adaptation, and HotSpot's
    // `asType` inserts a `valueOf` handle for the primitive->Object step.
    // Measured: `mh.asTypeInt` / `asTypeChar` / `asTypeBool` / `asTypeLong` /
    // `asTypeByte` / `asTypeShort` = true, and `mh.invokeAsObject` = true.
    // `ret_desc` was just narrowed to the eight primitive descriptors, and it
    // is the DECLARED return type of the target while `v` is what dispatch
    // produced, so the helper's variant guard is load-bearing here too.
    match result {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        Ok(Some(v)) => Ok(Some(box_value_canonical(ctx, v, ret_desc))),
        other => other,
    }
}

pub(crate) fn mh_guard_truthy(ctx: &dyn NativeContext, result: Option<Value>) -> bool {
    match result {
        Some(Value::Int(v)) => v != 0,
        Some(Value::Object(Some(obj))) => match crate::lang_class::unbox_value(ctx, obj) {
            Value::Int(v) => v != 0,
            Value::Long(v) => v != 0,
            Value::Float(v) => v != 0.0,
            Value::Double(v) => v != 0.0,
            Value::Object(Some(_)) => true,
            _ => false,
        },
        _ => false,
    }
}

/// A combinator's view of what its target raised: a Java exception a native
/// step raised as a `RuntimeError` (an adapter's `ClassCastException`, a
/// leaf's `NullPointerException`) is materialized into its `Throwable`, so
/// `catchException` / `tryFinally` see it as HotSpot's adapters do, where
/// every such failure is an ordinary thrown exception. Before this they saw
/// only `ExceptionThrown`: wave 7's cast chain made
/// `catchException(h.asType((Object)I), ClassCastException.class, ..)` raise
/// the cast as a `RuntimeError`, which sailed past the handler
/// (`R12Hunt5MhChains` row `iface`; `r12w8-orch-probe-findings-on-w8`
/// item 2). `CRATONVM_MH_COMBINATOR_CATCHES_NATIVE_THROWS=0` restores that.
/// Allocates: callers re-read their pins after it.
pub(crate) fn catchable_failure(ctx: &mut dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
    match result {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(re)))
            if combinator_catches_native_throws() =>
        {
            let Some((class_name, message)) = re.as_java_throwable() else {
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(re),
                ));
            };
            let message = message.map(|m| m.into_owned());
            let built = match &message {
                Some(m) => {
                    let msg_obj = ctx.create_string(m);
                    ctx.new_object_initialized(
                        class_name,
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(msg_obj))],
                    )
                }
                None => ctx.new_object_initialized(class_name, "()V", &[]),
            };
            match built {
                Ok(Some(Value::Object(Some(obj)))) => Err(MethodCallFailed::ExceptionThrown(obj)),
                _ => Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(re),
                )),
            }
        }
        other => other,
    }
}

/// `CRATONVM_MH_COMBINATOR_CATCHES_NATIVE_THROWS` -- default ON (round 12
/// wave 8 orchestrator, both modes). See [`catchable_failure`]. Read only
/// when a target failed with a `RuntimeError`.
pub(crate) fn combinator_catches_native_throws() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_COMBINATOR_CATCHES_NATIVE_THROWS")
}

/// `MethodHandles.catchException` dispatch (`MH_KIND_CATCH`).
/// An adapter whose own `type` returns a REFERENCE over an inner handle whose
/// `type` returns a primitive answers the inner result BOXED, as `asType`'s
/// return conversion does (`Z` -> `Boolean`). The emulated adapters
/// (`guardWithTest`, `catchException`) passed the raw `Value::Int` through, and
/// Groovy's indy `E.m(true)` -- `catch(guard(.. drop(m)))` typed
/// `(Class, boolean)Object` over `m(boolean)boolean` -- answered `null` instead
/// of `true` (`ThymeleafServletAutoConfigurationTests`).
pub(crate) fn box_adapter_result(
    ctx: &mut dyn NativeContext,
    outer: ObjectRef,
    inner: ObjectRef,
    result: MethodCallResult,
) -> MethodCallResult {
    let Ok(Some(v)) = &result else {
        return result;
    };
    if matches!(v, Value::Object(_)) {
        return result;
    }
    let v = *v;
    let ret_of = |ctx: &mut dyn NativeContext, mh: ObjectRef| {
        mh_type_descriptor(ctx, mh)
            .and_then(|d| split_descriptor_params(&d))
            .map(|(_, ret)| ret)
    };
    let is_primitive = |d: &str| matches!(d, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D");
    let Some(outer_ret) = ret_of(ctx, outer) else {
        return result;
    };
    if !(outer_ret.starts_with('L') || outer_ret.starts_with('[')) {
        return result;
    }
    // The inner handle's own `type` first. An `asType` copy of a leaf is stamped
    // with the widened type (`(Class, boolean)Object`) while its leaf still
    // answers the raw primitive, so the recorded leaf descriptor says what that
    // raw `Value::Int` IS.
    let inner_ret = match ret_of(ctx, inner) {
        Some(r) if is_primitive(&r) => r,
        _ => match mh_read_desc(ctx, inner)
            .and_then(|d| split_descriptor_params(&d))
            .map(|(_, r)| r)
        {
            Some(r) if is_primitive(&r) => r,
            _ => return result,
        },
    };
    Ok(Some(crate::lang_class::box_value_canonical(
        ctx, v, &inner_ret,
    )))
}

pub(crate) fn mh_dispatch_catch(
    ctx: &mut dyn NativeContext,
    outer: ObjectRef,
    bound: Value,
    extra_args: &[Value],
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let catch_type = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(c)) => c,
        _ => return mh_dispatch(ctx, target, extra_args),
    };
    let handler = match ctx.get_field(wrapper, 2) {
        Value::Object(Some(h)) => h,
        _ => return mh_dispatch(ctx, target, extra_args),
    };

    // GC-safety: `mh_dispatch(ctx, target, ...)` below runs the target
    // handle, which can trigger a collection that relocates `catch_type`/
    // `handler` (both captured above and read again in the Err arm below).
    let catch_type_pin = ctx.pin_native_root(catch_type);
    let handler_pin = ctx.pin_native_root(handler);
    // gen r4w3/rooting: the args are forwarded to the handler after the
    // target dispatch (a GC point); pin them, re-read in the Err arm.
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let target_result = mh_dispatch(ctx, target, extra_args);
    let target_result = box_adapter_result(ctx, outer, target, target_result);
    match catchable_failure(ctx, target_result) {
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            let catch_type = ctx.read_native_pin(catch_type_pin, catch_type);
            let handler = ctx.read_native_pin(handler_pin, handler);
            let extra_args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            let extra_args = extra_args.as_slice();
            if !mh_exception_matches(ctx, thrown, catch_type) {
                ctx.unpin_native_roots(catch_type_pin);
                return Err(MethodCallFailed::ExceptionThrown(thrown));
            }
            // `mh_type_descriptor` can itself allocate; pin `thrown`/
            // `handler` across it too and re-read before dispatch.
            let thrown_pin = ctx.pin_native_root(thrown);
            let hdesc = mh_type_descriptor(ctx, handler)
                .or_else(|| mh_read_desc(ctx, handler))
                .unwrap_or_default();
            let hparams = count_descriptor_params(&hdesc);
            let forward_n = hparams.saturating_sub(1).min(extra_args.len());
            let handler = ctx.read_native_pin(handler_pin, handler);
            let thrown = ctx.read_native_pin(thrown_pin, thrown);
            ctx.unpin_native_roots(catch_type_pin);
            let mut hargs = Vec::with_capacity(1 + forward_n);
            hargs.push(Value::Object(Some(thrown)));
            hargs.extend_from_slice(&extra_args[..forward_n]);
            let handled = mh_dispatch(ctx, handler, &hargs);
            box_adapter_result(ctx, outer, handler, handled)
        }
        // The target returned (or failed uncatchably): release this
        // function's pins, the lowest taking the rest. They were left to the
        // funnel, four per call through `invokeWithArguments` (interpreter
        // round i2 wave 62, lane L4; `L4W58MixedWidthArguments` row
        // `iwa-catch`, the `[pin-balance] LEAK` census).
        other => {
            ctx.unpin_native_roots(catch_type_pin);
            other
        }
    }
}

/// `MethodHandles.countedLoop` dispatch (`MH_KIND_LOOP_COUNTED`). `bound` is
/// the 3-field wrapper `(iterations, init, body)`. See that constant's doc
/// comment for the semantics and the measurement that pins the argument
/// order (`body(v, i)`, not `body(i, v)`).
pub(crate) fn mh_dispatch_counted_loop(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(None),
    };
    // gen r4w3/rooting: every dispatch below is a GC point. Pin the wrapper
    // (the three handles are re-read from it after each GC point), the
    // incoming args (re-passed on every call), and the loop-carried `v`
    // (kept as the topmost pin, see `repin_loop_value`). `wrapper_pin` is the
    // lowest handle and releases everything at the end.
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let iterations = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    // Each clause takes the prefix of `A...` it declares (wave 44, see
    // `mh_dispatch_while_loop`); the body its `(v, i)` and then that prefix.
    let iterations_n = match iterations {
        Some(m) => loop_clause_arity(ctx, m, 0, extra_args.len()),
        None => 0,
    };
    let count = match iterations {
        Some(m) => match mh_dispatch(ctx, m, &extra_args[..iterations_n])? {
            Some(Value::Int(n)) => n,
            Some(Value::Object(Some(o))) => match crate::lang_class::unbox_value(ctx, o) {
                Value::Int(n) => n,
                _ => 0,
            },
            _ => 0,
        },
        None => 0,
    };
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    let init = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let body = match ctx.get_field(wrapper, 2) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    // The loop variable `v` (wave 26): a `void` body takes none -- `body(i,
    // A...)` -- and a missing `init` starts it at its type's zero, not `null`
    // (`countedLoop(constant(int, 3), null, (v, i) -> v + i)` is 3 on HotSpot;
    // a `void` body read `i` one slot late). See `loop_variable_shape`.
    let (body_void, v_zero) = loop_variable_shape(ctx, body);
    let init_n = match init {
        Some(m) => loop_clause_arity(ctx, m, 0, extra_args.len()),
        None => 0,
    };
    let body_n = match body {
        Some(m) => loop_clause_arity(ctx, m, if body_void { 1 } else { 2 }, extra_args.len()),
        None => 0,
    };
    // GC-safety: `body_mh` is dispatched repeatedly across a loop whose body
    // (an arbitrary user method) can allocate and trigger a collection on any
    // iteration; pin it once and re-read the forwarded reference each time,
    // the same idiom `MH_KIND_COLLECT`'s per-element loop above uses.
    // gen r4w3/rooting: pinned BELOW `v_pin`, which is truncated and
    // re-pinned on every iteration.
    let body_pin = body.map(|b| ctx.pin_native_root(b));
    let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
    let mut v: Option<Value> = match init {
        Some(m) => mh_dispatch(ctx, m, &args[..init_n.min(args.len())])?,
        None => None,
    };
    if v.is_none() {
        v = v_zero;
    }
    let mut v_pin: Option<usize> = None;
    repin_loop_value(ctx, &mut v_pin, v);
    // The four-handle `countedLoop(start, end, init, body)` (wave 44): slot 3
    // is `start`, evaluated after `init` as the JDK's clause order does
    // (`end`, `init`, then `start`); the three-handle form starts at 0.
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    let start_mh = if ctx.object_num_fields(wrapper) > 3 {
        match ctx.get_field(wrapper, 3) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        }
    } else {
        None
    };
    let start = match start_mh {
        Some(m) => {
            let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            let start_n = loop_clause_arity(ctx, m, 0, args.len());
            match mh_dispatch(ctx, m, &args[..start_n])? {
                Some(Value::Int(n)) => n,
                Some(Value::Object(Some(o))) => match crate::lang_class::unbox_value(ctx, o) {
                    Value::Int(n) => n,
                    _ => 0,
                },
                _ => 0,
            }
        }
        None => 0,
    };
    let (Some(body_mh), Some(body_pin)) = (body, body_pin) else {
        let v = read_loop_value(ctx, v_pin, v);
        ctx.unpin_native_roots(wrapper_pin);
        return Ok(v);
    };
    for i in start..count {
        v = read_loop_value(ctx, v_pin, v);
        let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
        let mut bargs: Vec<Value> = Vec::with_capacity(2 + args.len());
        if !body_void {
            bargs.push(v.unwrap_or(Value::Object(None)));
        }
        bargs.push(Value::Int(i));
        bargs.extend_from_slice(&args[..body_n.min(args.len())]);
        let body_mh = ctx.read_native_pin(body_pin, body_mh);
        let result = mh_dispatch(ctx, body_mh, &bargs)?;
        if result.is_some() {
            v = result;
            repin_loop_value(ctx, &mut v_pin, v);
        }
    }
    let v = read_loop_value(ctx, v_pin, v);
    ctx.unpin_native_roots(wrapper_pin);
    Ok(v)
}

/// gen r4w3/rooting: keep a loop-carried dispatch result rooted as the
/// TOPMOST native pin. Anything pinned above it by the time it is replaced
/// belongs to a nested call that has already returned, so truncating at the
/// old handle and re-pinning cannot release a pin still in use. Bounded: at
/// most one pin per loop, however many iterations run.
pub(crate) fn repin_loop_value(ctx: &mut dyn NativeContext, pin: &mut Option<usize>, v: Option<Value>) {
    if let Some(p) = pin.take() {
        ctx.unpin_native_roots(p);
    }
    if let Some(Value::Object(Some(o))) = v {
        *pin = Some(ctx.pin_native_root(o));
    }
}

/// gen r4w3/rooting: current address of a value pinned by [`repin_loop_value`].
pub(crate) fn read_loop_value(ctx: &dyn NativeContext, pin: Option<usize>, v: Option<Value>) -> Option<Value> {
    match (pin, v) {
        (Some(p), Some(Value::Object(Some(o)))) => {
            Some(Value::Object(Some(ctx.read_native_pin(p, o))))
        }
        _ => v,
    }
}

/// The loop variable a `MethodHandles` loop's `body` handle defines (wave 26):
/// whether it is `void` -- then neither the body nor a predicate takes one
/// (`countedLoop`'s `body(i, A...)`, `whileLoop`'s `pred(A...)` / `body(A...)`,
/// `iteratedLoop`'s `body(t, A...)`) -- and the value it starts at when there
/// is no `init`: the primitive's zero (HotSpot: `countedLoop(constant(int, 3),
/// null, (v, i) -> v + i)` is 3), else `None` (a `null` reference).
///
/// The loop arms pushed `v` unconditionally, and `null` when there was no
/// `init`: a `void` body read `i` (or its first `A`) one slot late, and a
/// primitive body read a `null` as its `int`, which the leaf's argument
/// conversion refuses since wave 26. An unreadable body descriptor answers
/// `(false, None)`, the previous behaviour.
pub(crate) fn loop_variable_shape(
    ctx: &mut dyn NativeContext,
    body: Option<ObjectRef>,
) -> (bool, Option<Value>) {
    let Some(body) = body else {
        return (false, None);
    };
    let Some(desc) = mh_type_descriptor(ctx, body).or_else(|| mh_read_desc(ctx, body)) else {
        return (false, None);
    };
    let ret = return_type_desc(&desc);
    if ret == "V" {
        return (true, None);
    }
    let zero = mh_zero_value(ret);
    (false, (!matches!(zero, Value::Object(_))).then_some(zero))
}

/// `MethodHandles.whileLoop`/`doWhileLoop` dispatch (`MH_KIND_LOOP_WHILE`/
/// `MH_KIND_LOOP_DOWHILE`). `bound` is the 3-field wrapper; for `whileLoop`
/// it is `(init, pred, body)`, for `doWhileLoop` it is `(init, body, pred)` —
/// the factories store them in their OWN call's argument order rather than
/// normalising, so `do_while` picks which slot order this dispatch reads.
/// `whileLoop` tests BEFORE each iteration; `doWhileLoop` runs the body once
/// unconditionally first (see `MH_KIND_LOOP_DOWHILE`'s doc comment) and then
/// the two are the same loop with the test moved.
pub(crate) fn mh_dispatch_while_loop(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
    do_while: bool,
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(None),
    };
    let init = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let (body, pred) = if do_while {
        (
            match ctx.get_field(wrapper, 1) {
                Value::Object(Some(m)) => Some(m),
                _ => None,
            },
            match ctx.get_field(wrapper, 2) {
                Value::Object(Some(m)) => Some(m),
                _ => None,
            },
        )
    } else {
        (
            match ctx.get_field(wrapper, 2) {
                Value::Object(Some(m)) => Some(m),
                _ => None,
            },
            match ctx.get_field(wrapper, 1) {
                Value::Object(Some(m)) => Some(m),
                _ => None,
            },
        )
    };
    let (Some(body_mh), Some(pred_mh)) = (body, pred) else {
        return match init {
            Some(m) => mh_dispatch(ctx, m, extra_args),
            None => Ok(None),
        };
    };
    // A `void` body takes no loop variable, and neither does the predicate:
    // `pred(A...)`, `body(A...)`; a missing `init` starts it at its zero
    // (wave 26, `loop_variable_shape`).
    let (body_void, v_zero) = loop_variable_shape(ctx, Some(body_mh));
    // Each clause takes the prefix of `(v, A...)` it declares (JDK 25
    // `loop`: every clause function is adapted to the whole internal
    // parameter list and ignores what it does not declare). Wave 44, lane L4
    // (`L4W44CombinatorHunt` row `whileLoop-short-clauses`): every clause was
    // handed all of the arguments.
    let lead = usize::from(!body_void);
    let init_n = match init {
        Some(m) => loop_clause_arity(ctx, m, 0, extra_args.len()),
        None => extra_args.len(),
    };
    let body_n = loop_clause_arity(ctx, body_mh, lead, extra_args.len());
    let pred_n = loop_clause_arity(ctx, pred_mh, lead, extra_args.len());
    // GC-safety: both handles are dispatched repeatedly across a loop whose
    // body/predicate (arbitrary user methods) can allocate; pin once and
    // re-read the forwarded reference on every use, as above.
    // gen r4w3/rooting: pinned BEFORE the init dispatch (a GC point, which
    // used to run while they were unrooted), together with the incoming
    // args, which are re-passed on every call. The loop-carried `v` is kept
    // as the topmost pin (`repin_loop_value`); `body_pin` releases all.
    let body_pin = ctx.pin_native_root(body_mh);
    let pred_pin = ctx.pin_native_root(pred_mh);
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let mut v: Option<Value> = match init {
        Some(m) => mh_dispatch(ctx, m, &extra_args[..init_n])?,
        None => None,
    };
    if v.is_none() {
        v = v_zero;
    }
    let mut v_pin: Option<usize> = None;
    repin_loop_value(ctx, &mut v_pin, v);
    loop {
        if do_while {
            v = read_loop_value(ctx, v_pin, v);
            let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            let mut bargs: Vec<Value> = Vec::with_capacity(1 + args.len());
            if !body_void {
                bargs.push(v.unwrap_or(Value::Object(None)));
            }
            bargs.extend_from_slice(&args[..body_n.min(args.len())]);
            let body_mh = ctx.read_native_pin(body_pin, body_mh);
            v = mh_dispatch(ctx, body_mh, &bargs)?;
            repin_loop_value(ctx, &mut v_pin, v);
        }
        v = read_loop_value(ctx, v_pin, v);
        let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
        let mut pargs: Vec<Value> = Vec::with_capacity(1 + args.len());
        if !body_void {
            pargs.push(v.unwrap_or(Value::Object(None)));
        }
        pargs.extend_from_slice(&args[..pred_n.min(args.len())]);
        let pred_mh = ctx.read_native_pin(pred_pin, pred_mh);
        let pred_result = mh_dispatch(ctx, pred_mh, &pargs)?;
        let truthy = mh_guard_truthy(ctx, pred_result);
        if !truthy {
            break;
        }
        if !do_while {
            v = read_loop_value(ctx, v_pin, v);
            let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            let mut bargs: Vec<Value> = Vec::with_capacity(1 + args.len());
            if !body_void {
                bargs.push(v.unwrap_or(Value::Object(None)));
            }
            bargs.extend_from_slice(&args[..body_n.min(args.len())]);
            let body_mh = ctx.read_native_pin(body_pin, body_mh);
            v = mh_dispatch(ctx, body_mh, &bargs)?;
            repin_loop_value(ctx, &mut v_pin, v);
        }
    }
    let v = read_loop_value(ctx, v_pin, v);
    ctx.unpin_native_roots(body_pin);
    Ok(v)
}

/// `MethodHandles.iteratedLoop` dispatch (`MH_KIND_LOOP_ITERATE`), for the
/// `iterator == null` form this VM implements: the adapter's own leading
/// argument IS the `Iterable` to walk (see that constant's doc comment).
/// `bound` is the 2-field wrapper `(init, body)`. Walks the JDK's own
/// `Iterable`/`Iterator` protocol via `NativeContext::invoke_virtual` rather
/// than assuming any particular collection shape, so it works on any real
/// `Iterable`, not just `List`.
pub(crate) fn mh_dispatch_iterated_loop(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(None),
    };
    let init = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let body = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    // Slot 2 (wave 44): a non-null `iterator` handle, which produces the
    // `Iterator` from the loop's arguments; without one the leading argument
    // is the `Iterable` to walk.
    let iterator_mh = if ctx.object_num_fields(wrapper) > 2 {
        match ctx.get_field(wrapper, 2) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        }
    } else {
        None
    };
    let iterable = match (iterator_mh, extra_args.first()) {
        (Some(_), _) => None,
        (None, Some(Value::Object(Some(o)))) => Some(*o),
        (None, _) => {
            // Interpreter round i7 wave 141, lane L4
            // (`CRATONVM_MH_ITERATED_LOOP_CHECKS`): the JDK loop's first
            // clause is `MethodHandleImpl.initIterator(Iterable it)`, whose
            // `it.iterator()` raises this helpful NPE on HotSpot.
            let message = if mh_iterated_loop_checks_enabled() {
                "Cannot invoke \"java.lang.Iterable.iterator()\" because \"it\" is null"
            } else {
                "iteratedLoop: iterable is null"
            };
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(message.to_string()),
            }
            .into());
        }
    };
    // The external parameter list `A...` is every argument, the iterable
    // (the leading `A`) included; `init` takes a prefix of it and `body` a
    // prefix after its `(V T)` (JDK 25 `iteratedLoop`; the loop clauses are
    // adapted to the full list and ignore what they do not declare). Wave 44,
    // lane L4 (`L4W44CombinatorHunt` row `iteratedLoop`): both were handed
    // the arguments AFTER the iterable, so a body declaring `(V T Iterable)`
    // or an `init` declaring `(Iterable)` read the wrong values.
    let trailing: &[Value] = extra_args;
    let Some(body_mh) = body else {
        return match init {
            Some(m) => {
                let n = loop_clause_arity(ctx, m, 0, trailing.len());
                mh_dispatch(ctx, m, &trailing[..n])
            }
            None => Ok(None),
        };
    };
    // A `void` body takes no loop variable -- `body(t, A...)` -- and a missing
    // `init` starts it at its zero (wave 26, `loop_variable_shape`).
    let (body_void, v_zero) = loop_variable_shape(ctx, Some(body_mh));
    let init_n = match init {
        Some(m) => loop_clause_arity(ctx, m, 0, trailing.len()),
        None => 0,
    };
    let body_n = loop_clause_arity(ctx, body_mh, if body_void { 1 } else { 2 }, trailing.len());
    let iterator_n = match iterator_mh {
        Some(m) => loop_clause_arity(ctx, m, 0, trailing.len()),
        None => 0,
    };
    // gen r4w3/rooting: the init dispatch, `iterator()`, `hasNext()`,
    // `next()` and each body dispatch are all GC points. Pin the body handle,
    // the iterable (or the iterator handle) and the trailing args before the
    // first of them; the loop-carried `v` is kept as the topmost pin
    // (`repin_loop_value`), so `iter_pin` (pinned before it) survives each
    // re-pin. `body_pin` (the lowest) releases everything at the end.
    let body_pin = ctx.pin_native_root(body_mh);
    let source_pin = match (iterator_mh, iterable) {
        (Some(m), _) | (None, Some(m)) => Some((ctx.pin_native_root(m), m)),
        (None, None) => None,
    };
    let (_, trailing_handles) = pin_mh_args(ctx, trailing);
    let mut v: Option<Value> = match init {
        Some(m) => mh_dispatch(ctx, m, &trailing[..init_n])?,
        None => None,
    };
    if v.is_none() {
        v = v_zero;
    }
    let mut v_pin: Option<usize> = None;
    repin_loop_value(ctx, &mut v_pin, v);
    let produced = match (source_pin, iterator_mh.is_some()) {
        (Some((pin, m)), true) => {
            let iterator = ctx.read_native_pin(pin, m);
            let args = read_pinned_mh_args(ctx, &trailing_handles, trailing);
            mh_dispatch(ctx, iterator, &args[..iterator_n.min(args.len())])?
        }
        (Some((pin, o)), false) => {
            let iterable = ctx.read_native_pin(pin, o);
            ctx.invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])?
        }
        (None, _) => None,
    };
    let iter_obj = match produced {
        Some(Value::Object(Some(it))) => it,
        _ => {
            let v = read_loop_value(ctx, v_pin, v);
            ctx.unpin_native_roots(body_pin);
            return Ok(v);
        }
    };
    // `iter_obj` must sit below `v_pin`: drop `v_pin`, pin the iterator,
    // then re-pin `v` on top.
    v = read_loop_value(ctx, v_pin, v);
    if let Some(p) = v_pin.take() {
        ctx.unpin_native_roots(p);
    }
    // GC-safety: both handles are re-used across a loop whose bodies
    // (`Iterator.next`, the user's `body` handle) can allocate; pin once and
    // re-read the forwarded reference on every use, as the other loop
    // dispatch functions above do.
    let iter_pin = ctx.pin_native_root(iter_obj);
    repin_loop_value(ctx, &mut v_pin, v);
    loop {
        let iter_obj = ctx.read_native_pin(iter_pin, iter_obj);
        let has_next = match ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[])? {
            Some(Value::Int(b)) => b != 0,
            Some(Value::Object(Some(o))) => {
                matches!(crate::lang_class::unbox_value(ctx, o), Value::Int(b) if b != 0)
            }
            _ => false,
        };
        if !has_next {
            break;
        }
        let iter_obj = ctx.read_native_pin(iter_pin, iter_obj);
        let item = ctx
            .invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        v = read_loop_value(ctx, v_pin, v);
        let trailing = read_pinned_mh_args(ctx, &trailing_handles, trailing);
        let mut bargs: Vec<Value> = Vec::with_capacity(2 + trailing.len());
        if !body_void {
            bargs.push(v.unwrap_or(Value::Object(None)));
        }
        bargs.push(item);
        bargs.extend_from_slice(&trailing[..body_n.min(trailing.len())]);
        let body_mh = ctx.read_native_pin(body_pin, body_mh);
        v = mh_dispatch(ctx, body_mh, &bargs)?;
        repin_loop_value(ctx, &mut v_pin, v);
    }
    let v = read_loop_value(ctx, v_pin, v);
    ctx.unpin_native_roots(body_pin);
    Ok(v)
}

/// How many of `available` loop arguments a loop clause `h` takes after its
/// `lead` loop-variable parameters: its declared parameter count less
/// `lead`, at most `available`; all of them when its type cannot be read.
pub(crate) fn loop_clause_arity(
    ctx: &mut dyn NativeContext,
    h: ObjectRef,
    lead: usize,
    available: usize,
) -> usize {
    match mh_type_descriptor(ctx, h) {
        Some(desc) => count_descriptor_params(&desc)
            .saturating_sub(lead)
            .min(available),
        None => available,
    }
}

/// `MethodHandles.tryFinally` dispatch (`MH_KIND_TRY_FINALLY`). `bound` is
/// the 2-field wrapper `(target, cleanup)`. See that constant's doc comment
/// for the contract; the exceptional arm mirrors `mh_dispatch_catch`'s use of
/// `MethodCallFailed::ExceptionThrown` to recover the actual thrown object so
/// `cleanup` can read it as its leading parameter.
pub(crate) fn mh_dispatch_try_finally(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(None),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(None),
    };
    let cleanup = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(m)) => m,
        _ => return mh_dispatch(ctx, target, extra_args),
    };
    let target_ret = mh_type_descriptor(ctx, target)
        .or_else(|| mh_read_desc(ctx, target))
        .map(|d| return_type_desc(&d).to_string())
        .unwrap_or_default();
    let target_void = target_ret == "V";
    // The result slot's value when the target threw: the primitive's zero, as
    // HotSpot passes it (`tf.invoke(4)` of an `int` target that throws hands
    // `cleanup(Throwable, int, int)` r=0). It was a `null`, which the leaf's
    // argument conversion now refuses (wave 26).
    let zero_result = mh_zero_value(&target_ret);
    // GC-safety: `mh_dispatch(ctx, target, ...)` below can trigger a
    // collection that relocates `cleanup` (captured above, read again in
    // both arms below).
    let cleanup_pin = ctx.pin_native_root(cleanup);
    // gen r4w3/rooting: the args are re-passed to `cleanup` after the target
    // dispatch (a GC point); pin them and re-read in both arms.
    let (_, arg_handles) = pin_mh_args(ctx, extra_args);
    let target_result = mh_dispatch(ctx, target, extra_args);
    let target_result = catchable_failure(ctx, target_result);
    let extra_args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
    let extra_args = extra_args.as_slice();
    // Every pin of this function is read back by now; release them (the
    // lowest, `cleanup_pin`, takes the argument pins with it) before the
    // cleanup runs. They were left to the funnel: each `tryFinally` call
    // leaked one pin per adapter level, which the `[pin-balance] LEAK` census
    // named (interpreter round i2 wave 62, lane L4; `L4W59CombinatorCallSiteTypes`
    // row `tryFinally`).
    let cleanup = ctx.read_native_pin(cleanup_pin, cleanup);
    ctx.unpin_native_roots(cleanup_pin);
    // Interpreter round i7 wave 141, lane L4
    // (`CRATONVM_MH_TRY_FINALLY_CLEANUP_SHAPES`): the cleanup declares
    // `(Throwable[, result])` and a PREFIX of the target's parameters; it takes
    // only that prefix (the JDK's `dropArgumentsToMatch`). Read here, before
    // either arm builds its arguments; `mh_type_descriptor` makes no GC point.
    let lead = if target_void { 1 } else { 2 };
    let cleanup_n = if mh_try_finally_cleanup_shapes_enabled() {
        loop_clause_arity(ctx, cleanup, lead, extra_args.len())
    } else {
        extra_args.len()
    };
    let extra_args = &extra_args[..cleanup_n];
    match target_result {
        Ok(result) => {
            let mut cargs: Vec<Value> = Vec::with_capacity(2 + extra_args.len());
            cargs.push(Value::Object(None));
            if !target_void {
                cargs.push(result.unwrap_or(zero_result));
            }
            cargs.extend_from_slice(extra_args);
            mh_dispatch(ctx, cleanup, &cargs)
        }
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            // GC-safety: `mh_dispatch(ctx, cleanup, ...)` below can trigger a
            // collection that relocates `thrown` (captured above, re-read
            // after cleanup runs and re-thrown below).
            let thrown_pin = ctx.pin_native_root(thrown);
            let mut cargs: Vec<Value> = Vec::with_capacity(2 + extra_args.len());
            cargs.push(Value::Object(Some(thrown)));
            if !target_void {
                cargs.push(zero_result);
            }
            cargs.extend_from_slice(extra_args);
            let cleanup_result = mh_dispatch(ctx, cleanup, &cargs);
            let thrown = ctx.read_native_pin(thrown_pin, thrown);
            ctx.unpin_native_roots(thrown_pin);
            match cleanup_result {
                // Cleanup ran normally: the ORIGINAL exception propagates,
                // exactly as if `finally` had no `throw` in it.
                Ok(_) => Err(MethodCallFailed::ExceptionThrown(thrown)),
                // Cleanup itself threw: that exception replaces the original,
                // exactly as a `throw` inside a real `finally` block would.
                Err(cleanup_err) => Err(cleanup_err),
            }
        }
        other => other,
    }
}

/// `MethodHandles.filterReturnValue` dispatch (`MH_KIND_RETURN_FILTER`).
/// `bound` is the 2-field wrapper (target MH, filter MH). Invokes `target`
/// with the incoming args, then passes its result through the unary `filter`
/// handle, returning the filter's result. A `void`-returning target (mh_
/// dispatch yields `Ok(None)`) is paired only with a zero-arg filter per the
/// JDK contract (the filter's sole parameter type must match the target's
/// return type), so the filter is invoked with no arguments in that case.
pub(crate) fn mh_dispatch_return_filter(
    ctx: &mut dyn NativeContext,
    bound: Value,
    extra_args: &[Value],
) -> MethodCallResult {
    let wrapper = match bound {
        Value::Object(Some(w)) => w,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let filter = match ctx.get_field(wrapper, 1) {
        Value::Object(Some(f)) => f,
        // No filter -> behave like the bare target.
        _ => return mh_dispatch(ctx, target, extra_args),
    };
    // GC-safety: `mh_dispatch(ctx, target, ...)` below can trigger a
    // collection that relocates `filter` (captured above, read again after).
    let filter_pin = ctx.pin_native_root(filter);
    let result = mh_dispatch(ctx, target, extra_args)?;
    let filter = ctx.read_native_pin(filter_pin, filter);
    ctx.unpin_native_roots(filter_pin);
    match result {
        Some(v) => mh_dispatch(ctx, filter, &[v]),
        None => mh_dispatch(ctx, filter, &[]),
    }
}

/// The `NullPointerException` an array accessor handle raises for a null
/// array, worded as HotSpot's helpful message for the JDK's
/// `MethodHandleImpl$ArrayAccessor` bodies (`return a[i]`, `a[i] = x`,
/// `return a.length`, parameter `a`): `Cannot load from int array because
/// "a" is null`, `Cannot store to object array ...`, `Cannot read the array
/// length ...`. `desc` is the handle's descriptor; its first parameter is the
/// array type (interpreter round i1 wave 23, lane L4; probe
/// `tools/probes/interp/L4/L4W23ForcedCombinators.java`).
pub(crate) fn mh_array_accessor_null(desc: &str, action: &str) -> MethodCallFailed {
    let message = if action == "length" {
        "Cannot read the array length because \"a\" is null".to_string()
    } else {
        let component = desc
            .strip_prefix("([")
            .and_then(|rest| rest.bytes().next())
            .unwrap_or(b'L');
        let kind = match component {
            b'I' => "int",
            b'J' => "long",
            b'F' => "float",
            b'D' => "double",
            b'C' => "char",
            b'S' => "short",
            b'B' | b'Z' => "byte/boolean",
            _ => "object",
        };
        format!("Cannot {action} {kind} array because \"a\" is null")
    };
    RuntimeError::NullPointerException {
        message: Some(message),
    }
    .into()
}

/// The `ClassCastException` an array accessor handle owes for an array
/// argument that is not of its array type, or `None` (interpreter round i1
/// wave 24, lane L4).
///
/// HotSpot's `invoke` / `invokeWithArguments` cast the array argument to the
/// handle's declared array type before the access (`Cannot cast [J to [I`,
/// `Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;`, measured by
/// `tools/probes/interp/L4/L4W24ArrayAccessorTypes.java`), so the refusal
/// comes before the index check. The `MH_KIND_ARRAY_GET` / `_SET` arm read
/// whatever array it was handed: a `long[]` through an `int[]` getter
/// answered a truncated element, and an `Integer[]` through a `String[]`
/// getter handed an `Integer` to a call site typed `String` -- a heap type
/// confusion no later check catches (a signature-polymorphic call has no
/// `checkcast` after it).
///
/// Refuses only on a positive reading: a primitive element kind that differs,
/// a reference array where a primitive one is declared (or the reverse), a
/// non-array, or a reference component the class hierarchy PROVES is not
/// assignable. A nested array component, an unnamed class and anything the
/// context cannot tell fail open.
pub(crate) fn mh_array_accessor_type_refusal(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    desc: &str,
) -> Option<MethodCallFailed> {
    let (params, _) = split_descriptor_params(desc)?;
    let want = params.first()?;
    let comp = want.strip_prefix('[')?;
    // The heap's own kind, so a context that cannot tell (a mock whose
    // `object_is_array` is the trait's `false`) is not read as "not an array".
    let is_array = match ctx.heap_kind_of(arr) {
        ObjectKind::Array => ctx.object_is_array(arr),
        ObjectKind::Object => false,
        _ => return None,
    };
    if ctx.heap_kind_of(arr) == ObjectKind::Array && !is_array {
        return None;
    }
    let fits = if !is_array {
        false
    } else {
        let et = ctx.heap_element_type_of(arr);
        match comp {
            "Z" => et == ArrayElementType::Boolean,
            "B" => et == ArrayElementType::Byte,
            "C" => et == ArrayElementType::Char,
            "S" => et == ArrayElementType::Short,
            "I" => et == ArrayElementType::Int,
            "J" => et == ArrayElementType::Long,
            "F" => et == ArrayElementType::Float,
            "D" => et == ArrayElementType::Double,
            _ if et != ArrayElementType::Reference => false,
            "Ljava/lang/Object;" => true,
            // A declared component that is itself an array: covariance over
            // nested arrays needs the receiver's component's own component,
            // which the header does not carry. Fail open.
            _ if comp.starts_with('[') => return None,
            _ => {
                let target = comp.strip_prefix('L')?.strip_suffix(';')?;
                let comp_cid = ctx.class_id_of_object(arr);
                let comp_name = ctx.class_name_of_id(comp_cid)?;
                if comp_name.is_empty() {
                    return None;
                }
                if comp_name.starts_with('[') {
                    // The receiver's elements are arrays: they are instances
                    // of `Object`, `Cloneable` and `Serializable` only.
                    matches!(target, "java/lang/Cloneable" | "java/io/Serializable")
                } else if comp_name == target {
                    true
                } else {
                    ctx.class_assignable_to_name(comp_cid, target)?
                }
            }
        }
    };
    if fits {
        return None;
    }
    let from = if is_array {
        cratonvm_native_api::array_store::aastore_value_external_name(&*ctx, arr)
    } else {
        ctx.class_name_of_id(ctx.class_id_of_object(arr))?
            .class_text(&*ctx)
    };
    Some(
        RuntimeError::ClassCastException {
            message: format!("Cannot cast {from} to {}", want.class_text(&*ctx)),
        }
        .into(),
    )
}

/// [`mh_dispatch`] at the call site `site` (round 15 wave 5, lane mh4, from lane
/// upcall1's `r15w5-upcall1-synthetic-upcall-dispatch-has-no-call-site-patch`):
/// for a caller with no Java call site whose arguments are exactly `site`'s
/// types -- an FFM upcall, whose site is its descriptor's `MethodType` (HotSpot
/// calls the target `invokeExact` with it). The site is armed as the `invoke`
/// door arms it ([`arm_entry_call_site`]): RECEIVER-STRIPPED for an unbound
/// virtual / special handle, whose leaf arms compare it against the
/// receiver-less parameters. With it the leaf knows each raw primitive's own
/// type (`char` / `boolean` / `byte` / `short` share `Value::Int`), which
/// [`leaf_box_raw_for_reference`] boxes by.
pub(crate) fn mh_dispatch_at_site(
    ctx: &mut dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
    site: &str,
    extra_args: &[Value],
) -> MethodCallResult {
    let mh = mh_door_handle(ctx, mh);
    let stripped = mh_receiver_stripped_site(&*ctx, mh, site);
    arm_entry_call_site(stripped.as_deref().unwrap_or(site));
    mh_dispatch(ctx, mh, extra_args)
}

/// `site` without its receiver for an UNBOUND virtual / special handle (the
/// form [`arm_entry_call_site`] wants for it, as `mh_invoke_door` strips it),
/// or `None` for every other handle (the site as is). Reads only.
pub(crate) fn mh_receiver_stripped_site(ctx: &dyn NativeContext, mh: ObjectRef, site: &str) -> Option<String> {
    let unbound_receiver = ctx.object_num_fields(mh) > MH_BOUND
        && matches!(
            ctx.get_field(mh, MH_KIND),
            Value::Int(MH_KIND_VIRTUAL | MH_KIND_SPECIAL)
        )
        && !matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
    if unbound_receiver {
        descriptor_without_first_param(site)
    } else {
        None
    }
}

/// Dispatch one synthetic `MethodHandle` with `extra_args`. A handle marked
/// [`MH_MARK_EXPLICIT_CAST`] opens the explicit-cast scope
/// ([`MH_EXPLICIT_SCOPE`]) for its own dispatch and everything beneath it, and
/// closes it on the way out, whatever the outcome (wave 26). One width-guarded
/// field read per dispatch.
pub(crate) fn mh_dispatch(
    ctx: &mut dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
    extra_args: &[Value],
) -> MethodCallResult {
    let bits = mh_marking_bits(&*ctx, mh);
    // Round 12 wave 7: the casts a stamp over a stamp owes its MIDDLE types
    // (and a bound value's refused cast), judged before the leaf runs and on
    // its result after (`MH_CAST_CHAIN`). Everything read here is read before
    // the body's first GC point; the judge allocates nothing.
    // With `CRATONVM_MH_RETURN_PLAN` (wave 59, lane L4) both are the handle's
    // memoised facts: no text read, no parse, no render on a hit.
    let planned = if bits & MH_MARK_CAST_CHAIN != 0 && crate::nbflags().mh_return_plan {
        mh_return_facts_chain(ctx, mh).map(|chain| (chain, mh_return_facts_declared(ctx, mh)))
    } else {
        None
    };
    let owned = if bits & MH_MARK_CAST_CHAIN != 0 && planned.is_none() {
        mh_cast_chain_text(&*ctx, mh)
            .and_then(|text| parse_cast_chain(&text))
            .map(|chain| (chain, mh_declared_descriptor(ctx, mh)))
    } else {
        None
    };
    let chain: Option<(&CastChain, Option<&str>)> = match (&planned, &owned) {
        (Some((chain, outer)), _) => Some((&**chain, outer.as_deref())),
        (None, Some((chain, outer))) => Some((chain, outer.as_deref())),
        (None, None) => None,
    };
    let mut converted: Option<Vec<Value>> = None;
    let mut mh = mh;
    if let Some((chain, outer)) = chain {
        match mh_cast_chain_judge_args_rebox(ctx, mh, chain, outer, extra_args) {
            Ok((values, reboxes)) => {
                converted = values;
                // Round 13 wave 1 (lane mhffm): a value a primitive middle
                // link re-boxed goes on as the new box (`cast_chain_converts`).
                // Allocates: `mh` and the arguments come back re-read.
                if !reboxes.is_empty() && cast_chain_converts() {
                    let base = converted.take().unwrap_or_else(|| extra_args.to_vec());
                    let (moved, values) = mh_cast_chain_rebox_args(ctx, mh, base, &reboxes);
                    mh = moved;
                    converted = Some(values);
                }
            }
            Err(refusal) => {
                // Nothing dispatches: drop what the door armed for this dispatch.
                let _ = take_entry_call_site();
                let _ = take_mh_tail();
                return Err(refusal);
            }
        }
        // The return is judged after the dispatch, so the door's frame is not
        // transparent to a yield beneath it (`MH_TAIL`; token 0).
        set_mh_tail(0);
    }
    let extra_args = converted.as_deref().unwrap_or(extra_args);
    let result = if !mh_explicit_scope() && bits & MH_MARK_EXPLICIT_CAST != 0 {
        MH_EXPLICIT_SCOPE.with(|f| f.set(true));
        let result = mh_dispatch_body(ctx, mh, extra_args);
        MH_EXPLICIT_SCOPE.with(|f| f.set(false));
        result
    } else {
        mh_dispatch_body(ctx, mh, extra_args)
    };
    match chain {
        Some((chain, outer)) => mh_cast_chain_judge_return(ctx, chain, outer, result),
        None => result,
    }
}

/// A field getter's or setter's handle given a null base raises in
/// `DirectMethodHandle.checkBase`'s `Objects.requireNonNull`, which HotSpot
/// lists under the handle's hidden `LambdaForm` frames (JDK 25.0.3;
/// `tools/probes/interp/L3/L3W54FieldOverrideAccessorFrames.java` under
/// `--jdk-only`, `L3W56FieldHandleNullBase`). The door serves the
/// signature-polymorphic invoker the caller called, so the raise's chain
/// starts with that invoker, unlisted (`ServedRaiseHopSite::Hidden`), then
/// the two frames. Interpreter round i2 wave 56, lane L3 (an error path).
#[cold]
#[inline(never)]
pub(crate) fn note_field_handle_check_base_chain(ctx: &mut dyn NativeContext) {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const REQUIRE_NON_NULL: &str = "requireNonNull";
    const OBJECT_TO_OBJECT: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    ctx.note_served_raise_chain(ServedRaiseChain {
        throwable_class: "java/lang/NullPointerException",
        hops: vec![
            ServedRaiseHop {
                class: "java/lang/invoke/MethodHandle",
                name: "invokeExact",
                descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
                site: ServedRaiseHopSite::Hidden,
            },
            ServedRaiseHop {
                class: "java/lang/invoke/DirectMethodHandle",
                name: "checkBase",
                descriptor: OBJECT_TO_OBJECT,
                site: ServedRaiseHopSite::LastCall {
                    name: REQUIRE_NON_NULL,
                    descriptor: Some(OBJECT_TO_OBJECT),
                },
            },
            ServedRaiseHop {
                class: "java/util/Objects",
                name: REQUIRE_NON_NULL,
                descriptor: OBJECT_TO_OBJECT,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    });
}

/// Which `DirectMethodHandle` barrier HotSpot initializes a handle's class in
/// (JDK 25.0.3, `tools/probes/interp/L4/L4W59HandleClassInitFrames.java`,
/// default flags): each lists its own JDK frames above the caller's
/// `invokeExact` / `invoke`, below the handle's hidden `LambdaForm` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DmhInitBarrier {
    /// A static method's handle: `internalMemberNameEnsureInit(Object)` ->
    /// `ensureInitialized()` -> `checkInitialized(MemberName)` ->
    /// `Unsafe.ensureClassInitialized` -> `ensureClassInitialized0`.
    StaticMethod,
    /// A static field's getter / setter: `ensureInitialized(Object)` ->
    /// `ensureInitialized()` -> `checkInitialized` -> the `Unsafe` pair.
    StaticField,
    /// A constructor's handle: `allocateInstance(Object)` ->
    /// `Unsafe.allocateInstance (Native Method)`, which initializes.
    Constructor,
}

/// The served-raise chain of a method handle's class initialization at
/// `barrier` (see [`DmhInitBarrier`]): the caller's invoker, unlisted
/// (`ServedRaiseHopSite::Hidden`), then the barrier's frames. Interpreter
/// round i2 wave 59, lane L4
/// (`interpreter-L4-a-handle-door-s-class-init-failure-lists-no-jdk-frames-FIXED-20261023`).
#[cold]
#[inline(never)]
pub(crate) fn dmh_class_init_chain(barrier: DmhInitBarrier) -> cratonvm_native_api::ServedRaiseChain {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const DMH: &str = "java/lang/invoke/DirectMethodHandle";
    const OBJECT_TO_OBJECT: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    const VOID: &str = "()V";
    let last = |name: &'static str, descriptor: &'static str| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    let mut hops = vec![ServedRaiseHop {
        class: "java/lang/invoke/MethodHandle",
        name: "invokeExact",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        site: ServedRaiseHopSite::Hidden,
    }];
    let barrier_hop = |name: &'static str, descriptor: &'static str| ServedRaiseHop {
        class: DMH,
        name,
        descriptor,
        site: last("ensureInitialized", VOID),
    };
    match barrier {
        DmhInitBarrier::Constructor => {
            hops.push(ServedRaiseHop {
                class: DMH,
                name: "allocateInstance",
                descriptor: OBJECT_TO_OBJECT,
                site: last("allocateInstance", OBJECT_TO_OBJECT_CLASS),
            });
            hops.push(ServedRaiseHop {
                class: "jdk/internal/misc/Unsafe",
                name: "allocateInstance",
                descriptor: OBJECT_TO_OBJECT_CLASS,
                site: ServedRaiseHopSite::Raise,
            });
        }
        DmhInitBarrier::StaticMethod | DmhInitBarrier::StaticField => {
            hops.push(if barrier == DmhInitBarrier::StaticMethod {
                barrier_hop("internalMemberNameEnsureInit", OBJECT_TO_OBJECT)
            } else {
                barrier_hop("ensureInitialized", "(Ljava/lang/Object;)V")
            });
            hops.push(ServedRaiseHop {
                class: DMH,
                name: "ensureInitialized",
                descriptor: VOID,
                site: last("checkInitialized", "(Ljava/lang/invoke/MemberName;)Z"),
            });
            hops.push(ServedRaiseHop {
                class: DMH,
                name: "checkInitialized",
                descriptor: "(Ljava/lang/invoke/MemberName;)Z",
                site: last("ensureClassInitialized", "(Ljava/lang/Class;)V"),
            });
            hops.extend(crate::lang_class::class_init_chain_tail(
                "jdk/internal/misc/Unsafe",
            ));
        }
    }
    ServedRaiseChain {
        throwable_class: "java/lang/ExceptionInInitializerError",
        hops,
    }
}

/// `Unsafe.allocateInstance(Class)`'s descriptor.
pub(crate) const OBJECT_TO_OBJECT_CLASS: &str = "(Ljava/lang/Class;)Ljava/lang/Object;";

/// Initialize `class_id` for a method handle's barrier `barrier`, listing
/// HotSpot's frames for its failure ([`dmh_class_init_chain`]). An
/// initialized class pays one state read. Interpreter round i2 wave 59, lane
/// L4.
#[inline]
pub(crate) fn dmh_initialize_served(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    barrier: DmhInitBarrier,
) -> Result<(), MethodCallFailed> {
    if ctx.is_class_initialized(class_id) {
        return Ok(());
    }
    dmh_initialize_served_cold(ctx, class_id, barrier)
}

#[cold]
#[inline(never)]
pub(crate) fn dmh_initialize_served_cold(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    barrier: DmhInitBarrier,
) -> Result<(), MethodCallFailed> {
    ctx.initialize_class_served(class_id, dmh_class_init_chain(barrier))
}

/// The STATIC-method arm's barrier ([`DmhInitBarrier::StaticMethod`]): the
/// method's DECLARING class -- `owner` (the class it is named through) or
/// the superclass that declares `name` `desc` -- initialized before the call,
/// with `args` kept valid across the `<clinit>` (a GC point). An initialized
/// owner (whose superclasses are then initialized too) pays one state read; a
/// declaring class this cannot find is left to the call, as before. A handle
/// with no recorded owner (`--compatible`'s, named by `class`) is looked up by
/// name only while it is `unsettled` (its first calls; a handle too narrow to
/// settle is not looked up per call). Interpreter round i2 wave 59, lane L4.
#[inline]
pub(crate) fn mh_static_method_initialize(
    ctx: &mut dyn NativeContext,
    owner: Option<ClassId>,
    class: &str,
    unsettled: bool,
    name: &str,
    desc: &str,
    args: &mut [Value],
) -> Result<(), MethodCallFailed> {
    let owner = match owner {
        Some(owner) => owner,
        None if unsettled => match ctx.class_id_by_name(class) {
            Some(owner) => owner,
            None => return Ok(()),
        },
        None => return Ok(()),
    };
    if ctx.is_class_initialized(owner) {
        return Ok(());
    }
    mh_static_method_initialize_cold(ctx, owner, name, desc, args)
}

/// [`mh_static_method_initialize`] for a caller holding its arguments
/// borrowed: `Some(copy)` -- the arguments as the `<clinit>` (a GC point)
/// left them -- when it initialized, `None` (nothing allocated) when there was
/// nothing to do.
#[inline]
pub(crate) fn mh_static_method_initialize_copy(
    ctx: &mut dyn NativeContext,
    owner: Option<ClassId>,
    class: &str,
    unsettled: bool,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Result<Option<Vec<Value>>, MethodCallFailed> {
    let owner = match owner {
        Some(owner) => owner,
        None if unsettled => match ctx.class_id_by_name(class) {
            Some(owner) => owner,
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    if ctx.is_class_initialized(owner) {
        return Ok(None);
    }
    let mut copy = args.to_vec();
    mh_static_method_initialize_cold(ctx, owner, name, desc, &mut copy)?;
    Ok(Some(copy))
}

#[cold]
#[inline(never)]
pub(crate) fn mh_static_method_initialize_cold(
    ctx: &mut dyn NativeContext,
    owner: ClassId,
    name: &str,
    desc: &str,
    args: &mut [Value],
) -> Result<(), MethodCallFailed> {
    let mut declaring = Some(owner);
    while let Some(c) = declaring {
        if ctx.class_declares_method(c, name, desc) {
            break;
        }
        declaring = ctx.superclass_of(c);
    }
    let Some(declaring) = declaring else {
        return Ok(());
    };
    if ctx.is_class_initialized(declaring) {
        return Ok(());
    }
    let (base, handles) = pin_mh_args(ctx, args);
    let init = dmh_initialize_served_cold(ctx, declaring, DmhInitBarrier::StaticMethod);
    vh_refresh_live(ctx, base, &handles, args);
    init
}

pub(crate) fn mh_dispatch_body(
    ctx: &mut dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
    extra_args: &[Value],
) -> MethodCallResult {
    // Consume the generic-entry flag FIRST, so every recursive dispatch an
    // adapter arm makes below sees `false` — only the outermost handle is
    // adapted to `genericMethodType(n)`. See [`MH_GENERIC_SPREAD`].
    //
    // Combined with the handle's own collector marking here rather than inside
    // [`collect_trailing_varargs`], which sees only `class`/`name`/`desc`: the
    // JDK's rule keys on the HANDLE (`asFixedArity()` on a variable-arity
    // target yields a handle that does NOT collect, and
    // `asVarargsCollector()` on a fixed-arity one yields a handle that does),
    // not on the target's `ACC_VARARGS` flag. `probes/MhVarargsNullProbe.java`
    // rows D03 and D04 are exactly that pair of controls.
    let entry_call_site = take_entry_call_site();
    // Taken with the entry site, for the same reason: only the dispatch the
    // door (or a reshaping arm) armed it for may read it. See [`MH_TAIL`].
    let tail = take_mh_tail();
    // A real JDK `DelegatingMethodHandle` reached as a combinator's target is
    // its own target (wave 45, `mh_door_handle`); nothing below reads a slot
    // of the delegating handle.
    let mh = mh_door_handle(ctx, mh);
    // A real-JDK guard/invoker adapter can ultimately target a foreign
    // downcall. Its downcall state lives above the MethodHandle metadata
    // slots, so dispatch it directly rather than decoding the generic layout.
    //
    // This used to test the receiver's class name against
    // `java/lang/foreign/DowncallHandle` — an invented class no image declares,
    // which `--jdk-only` therefore refused, killing all of FFM (P1-E). The
    // carrier is now a real `java/lang/invoke/MethodHandle`, so the question
    // is no longer "what class is this" but "is this handle's state downcall
    // state", which is what `is_downcall_handle` answers.
    if crate::panama::is_downcall_handle(ctx, mh) {
        if crate::nbflags().dbg_mh_dispatch {
            let arg_slots: Vec<Value> = extra_args
                .iter()
                .map(|value| match value {
                    Value::Object(Some(obj)) => ctx.get_field(*obj, 0),
                    other => *other,
                })
                .collect();
            eprintln!(
                "[MH_DOWNCALL] fn={:?} args={arg_slots:?}",
                ctx.get_field(mh, 0)
            );
        }
        // JIT round 16 wave 11 (lane mhbind): reached through `invoke` /
        // `invokeWithArguments` (an armed entry call site), or an `asType`
        // copy, the arguments are converted to the C function's own types
        // and the result to the handle's (`mh_downcall_adapted_call`).
        if mh_downcall_adapt_enabled() {
            if let Some(answer) =
                mh_downcall_adapted_call(ctx, mh, extra_args, entry_call_site.as_deref())
            {
                return answer;
            }
        }
        let mut args = Vec::with_capacity(extra_args.len() + 1);
        args.push(Value::Object(Some(mh)));
        args.extend_from_slice(extra_args);
        return crate::panama::pe_downcall_invoke(ctx, &args);
    }
    // A handle with none of this model's slots that no arm above took (a real
    // JDK `DirectMethodHandle` over a `LambdaForm`, a `BoundMethodHandle`
    // species): every read below would fall to a default and the call would
    // answer its zero value, silently. Interpreter round i1 wave 52
    // (`interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`, step 1).
    if ctx.object_num_fields(mh) <= MH_BOUND {
        // Step 2 of that page (interpreter round i2 wave 53, lane L4;
        // `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY`): run the handle's own
        // `LambdaForm` entry, as `invokeBasic` does, when the arguments already
        // have the handle's types.
        if crate::nbflags().mh_foreign_handle_runs_vmentry {
            if let Some(answer) =
                mh_run_foreign_vmentry(ctx, mh, extra_args, entry_call_site.as_deref())
            {
                return answer;
            }
        }
        if crate::nbflags().mh_foreign_handle_raises {
            return Err(mh_foreign_handle_refusal(ctx, mh));
        }
    }
    let class = match mh_read_class(ctx, mh) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let name = mh_read_name(ctx, mh).unwrap_or_default();
    let desc = mh_read_desc(ctx, mh).unwrap_or_default();
    // Two readings of the same fact, for the two paths below. `is_collector`
    // is the handle's own marking; `generic_collector` is the permissive
    // path's narrower question — "did the call arrive through a door that
    // could NOT have named the trailing array type", which is what decides a
    // `null` there.
    let is_collector = mh_is_varargs_collector(ctx, mh);
    let generic_collector = entry_call_site.is_some()
        && is_collector
        && !call_site_names_trailing_array(entry_call_site.as_deref(), &desc);
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    let bound = ctx.get_field(mh, MH_BOUND);
    if crate::nbflags().dbg_mh_dispatch {
        let runtime_class = ctx
            .class_name_of_id(ctx.class_id_of_object(mh))
            .unwrap_or_else(|| "<unknown>".to_string());
        eprintln!(
            "[MH_DISPATCH] runtime={runtime_class} class={class} name={name} desc={desc:?} kind={kind} bound={bound:?} argc={}",
            extra_args.len()
        );
        // T2.9.X-dbg: dump each dynamic arg's runtime class (or the raw
        // primitive) so a wrong-value / wrong-position bug in a combinator
        // chain (dropArguments/insertArguments/foldArguments/...) is
        // visible directly, not just the argc.
        let arg_descs: Vec<String> = extra_args
            .iter()
            .map(|v| match v {
                Value::Object(Some(o)) => {
                    let cn = ctx
                        .class_name_of_id(ctx.class_id_of_object(*o))
                        .unwrap_or_else(|| "?".to_string());
                    format!("{:p}:{}", o.as_ptr(), cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        eprintln!("[MH_DISPATCH_ARGS] {arg_descs:?}");
        if kind == MH_KIND_GUARD {
            if let Value::Object(Some(wrapper)) = bound {
                eprintln!(
                    "[MH_GUARD] test={:?} target={:?} fallback={:?}",
                    ctx.get_field(wrapper, 0),
                    ctx.get_field(wrapper, 1),
                    ctx.get_field(wrapper, 2),
                );
            }
        }
    }

    match kind {
        MH_KIND_STATIC => {
            // Static: extra_args are the full argument list. Unbox boxed
            // wrappers against the target's primitive param types. Adapter
            // kinds (insertArguments/asCollector/guardWithTest/permute/drop)
            // re-enter `mh_dispatch` DIRECTLY, bypassing the signature-
            // polymorphic invoke shim's `adapt_invoke_args` — so a bound boxed
            // `Integer`/`Boolean` would otherwise reach a primitive param as a
            // raw pointer. Groovy's `IndyInterface.make` binds `callType` as a
            // boxed `Integer` via `insertArguments` into `selectMethod(...,
            // int callType, ...)`; without unboxing here, `callType` is garbage
            // → `ArrayIndexOutOfBoundsException` on `CALL_TYPE_VALUES[callType]`
            // in `Selector.getSelector`. (`adapt_invoke_args` is a no-op for
            // already-correctly-typed args, so the direct `invoke` path that
            // already adapts is unaffected.)
            //
            // Honor a bound first argument: `staticHandle.bindTo(x)` keeps
            // MH_KIND_STATIC but captures `x` in MH_BOUND (the first param), so
            // prepend it to the incoming args before dispatch.
            let full: Vec<Value> = match bound {
                Value::Object(Some(_)) => {
                    let mut v = Vec::with_capacity(extra_args.len() + 1);
                    v.push(bound);
                    v.extend_from_slice(extra_args);
                    v
                }
                _ => extra_args.to_vec(),
            };
            // On a door whose call-site type is known, run the JDK's
            // `asType(callSiteType)` exactly; otherwise repair the argument
            // list the permissive way. A BOUND receiver disqualifies the
            // strict path: `bindTo` prepends a value the call site never
            // named, so the arity comparison would be off by one — and the JDK
            // agrees that a bound handle "is never a variable-arity method
            // handle, even if the original target method handle was".
            // gen r4w3/rooting: `mh` is an unrooted by-value local; read what
            // the return adaptation needs from it before any GC point below
            // (varargs collection / arg adaptation / the target invoke).
            let boxes = direct_return_boxes(ctx, mh, &desc);
            // The settled owner (`MH_STATIC_SETTLED`), read before any GC
            // point like `boxes`. A handle that has not settled yet is pinned
            // across the call, so the owner can be recorded on it afterwards.
            let settle_slot = mh_has_static_settled_slot(ctx, mh);
            let settled = if settle_slot {
                mh_static_settled(ctx, mh)
            } else {
                None
            };
            let settle_pin = (settle_slot && settled.is_none()).then(|| ctx.pin_native_root(mh));
            // The carrier's door-key slot (wave 66, lane L3,
            // `CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY`), taken while
            // `mh` is current like `settled`.
            let door_key = mh_carrier_door_key(&*ctx, mh, settled);
            // The recorded owner (`MH_BOUND`'s `Int`, or a bound handle's
            // `MH_STATIC_OWNER`; round 15 wave 5, lane mh4), read before any
            // GC point like `settled`.
            let owner = mh_static_owner_in(&*ctx, mh, bound);
            let full = match entry_call_site.as_deref() {
                Some(cs) if !matches!(bound, Value::Object(Some(_))) => {
                    match mh_entry_adapt(ctx, mh, cs, &desc, is_collector, &full) {
                        Ok(full) => full,
                        Err(e) => {
                            if let Some(pin) = settle_pin {
                                ctx.unpin_native_roots(pin);
                            }
                            return Err(e);
                        }
                    }
                }
                _ => collect_trailing_varargs(ctx, &class, &name, &desc, &full, generic_collector),
            };
            // `asType`'s conversion, fallible (wave 26): an argument an adapter
            // bound, spread or passed through, or one an `asType`-retyped
            // handle received, is refused here when it does not convert to its
            // primitive parameter (`adapt_leaf_args`).
            let lead = usize::from(matches!(bound, Value::Object(Some(_))));
            let site = leaf_site_params(entry_call_site.as_deref(), lead, full.len());
            let explicit = mh_explicit_scope();
            let mut adapted = match adapt_leaf_args(ctx, &full, &desc, site.as_deref(), explicit) {
                Ok(adapted) => adapted,
                Err(e) => {
                    if let Some(pin) = settle_pin {
                        ctx.unpin_native_roots(pin);
                    }
                    return Err(e);
                }
            };
            // Interpreter round i2 wave 59, lane L4: the declaring class is
            // initialized here, in `DirectMethodHandle`'s barrier, so its
            // failure lists HotSpot's frames (`mh_static_method_initialize`).
            // A settled handle has nothing left to initialize.
            if settled.is_none() {
                if let Err(e) = mh_static_method_initialize(
                    ctx,
                    owner,
                    &class,
                    settle_slot,
                    &name,
                    &desc,
                    &mut adapted,
                ) {
                    if let Some(pin) = settle_pin {
                        ctx.unpin_native_roots(pin);
                    }
                    return Err(e);
                }
            }
            // The STATIC entries: the first call initializes the method's
            // DECLARING class, as `DirectMethodHandle.ensureInitialized` does
            // (JVMS §5.5) — `findStatic(C.class, "m", ..)` for a static `m`
            // inherited from `P` runs `P.<clinit>` only. `invoke` initialized
            // `C` (wave 16; `lookup_find_static` no longer initializes). Once
            // a call has left nothing to initialize, the handle remembers its
            // owner and later calls skip the check (wave 20,
            // `invoke_static_settling`); a handle too narrow to remember
            // takes the two routes directly. `owner` was read above, before
            // the GC points (a bound handle's is in `MH_STATIC_OWNER`).
            // A result in tail position: the door's native frame is
            // transparent to a yield in the target (`MH_TAIL`; wave 26 for a
            // reference, wave 28 for a result the remounted frame converts).
            // The settle bookkeeping after the call is a cache fill a yield
            // may skip.
            let transparent = transparent_leaf_call(&*ctx, tail, &desc, None);
            let result = if settle_slot {
                let (result, now_settled) = carrier_invoke_static_settling(
                    ctx,
                    door_key.as_deref(),
                    owner,
                    settled,
                    &class,
                    &name,
                    &desc,
                    &adapted,
                );
                if let Some(pin) = settle_pin {
                    if let Some(settled_owner) = now_settled {
                        let mh = ctx.read_native_pin(pin, mh);
                        mh_set_static_settled(ctx, mh, settled_owner);
                    }
                    ctx.unpin_native_roots(pin);
                }
                result
            } else {
                match owner {
                    Some(owner) => {
                        ctx.invoke_static_by_class_id(owner, &class, &name, &desc, &adapted)
                    }
                    None => ctx.invoke_static(&class, &name, &desc, &adapted),
                }
            };
            transparent_leaf_call_done(ctx, transparent, &result);
            box_direct_primitive_return_pre(
                ctx,
                boxes,
                neutralize_missing_serialization_hook(result, &adapted),
                &desc,
            )
        }
        MH_KIND_CONSTRUCTOR => {
            // Constructor: allocate new object then call <init>.
            //
            // "class" is a plain binary-name STRING, which a name-based
            // resolve (ensure_class_initialized/invoke) collapses to "one
            // class per name" globally. findConstructor/unreflectConstructor
            // stash the ALREADY-RESOLVED ClassId of the caller's own Class
            // object in MH_BOUND (see lookup_find_constructor/
            // lookup_unreflect_constructor) -- prefer that identity so a
            // second same-named class minted by a different ClassLoader
            // (e.g. Groovy re-parseClass-ing two textually-similar scripts)
            // is constructed correctly instead of colliding with -- or being
            // refused in favor of -- the first.
            let bound_class_id = match bound {
                Value::Int(raw) => Some(cratonvm_types::ClassId::new(raw as u32)),
                _ => None,
            };
            // gc-common w15-e: the class's first `<clinit>` runs here, and it
            // is a GC point. `mh` and the incoming arguments are read after
            // it (varargs collection, argument adaptation, `<init>`), so both
            // are rooted across it and re-read. `mh_pin` is the lowest pin of
            // this arm: releasing it releases everything pinned below.
            //
            // A failed `<clinit>` throws (`ExceptionInInitializerError`, then
            // `NoClassDefFoundError`), as `new` would; this arm answered
            // `null` until interpreter wave 17.
            let mh_pin = ctx.pin_native_root(mh);
            let (_, arg_handles) = pin_mh_args(ctx, extra_args);
            // Its failure lists `DirectMethodHandle.allocateInstance` and
            // `Unsafe.allocateInstance (Native Method)` (wave 59, lane L4).
            let initialized = match bound_class_id {
                Some(cid) => dmh_initialize_served(ctx, cid, DmhInitBarrier::Constructor)
                    .and_then(|()| ctx.ensure_class_initialized_with_class_id(cid))
                    .map(|()| cid),
                None => ctx.ensure_class_initialized_served(&class, &|| {
                    dmh_class_init_chain(DmhInitBarrier::Constructor)
                }),
            };
            let mh = ctx.read_native_pin(mh_pin, mh);
            let extra_args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            let cid = match initialized {
                Ok(cid) => cid,
                Err(e) => {
                    ctx.unpin_native_roots(mh_pin);
                    return Err(e);
                }
            };
            let new_obj = ctx.alloc_object(cid, 16); // generous field count
                                                     // Interpreter round i2 wave 63, lane L1: the allocation HotSpot's
                                                     // `DirectMethodHandle.allocateInstance` makes through
                                                     // `Unsafe.allocateInstance`, a JVMTI `VMObjectAlloc` (MEASURED: a
                                                     // reflective constructor posts one).
            let new_obj = ctx.jvmti_vm_object_alloc(new_obj);
            // Coerce/unbox args against the <init> descriptor. The constructor
            // params align 1:1 with extra_args (no receiver), so this is exact.
            // invokeExact does NOT pre-adapt, so unboxing here covers both the
            // invoke and invokeExact paths (Jackson 3 uses invokeExact).
            // GC-safety: `adapt_invoke_args`/the `<init>` invocation below
            // can both allocate; pin `new_obj` and re-read the forwarded
            // reference before it's returned.
            let new_obj_pin = ctx.pin_native_root(new_obj);
            // Round 15 wave 6 (lane mh5): `alloc_object` is a GC point as
            // well; the handle and the arguments re-read after `<clinit>` were
            // handed to the collection below at their pre-allocation
            // addresses. Read again through the pins taken above.
            let mh = ctx.read_native_pin(mh_pin, mh);
            let extra_args = read_pinned_mh_args(ctx, &arg_handles, &extra_args);
            // A variable-arity CONSTRUCTOR is a varargs collector exactly like
            // a variable-arity method — `findConstructor(Holder.class,
            // methodType(void.class, String[].class)).isVarargsCollector()` is
            // true on every JDK — but this arm was the one dispatch path that
            // never collected, so `ctor.invokeWithArguments("a", "b")` reached
            // `<init>([Ljava/lang/String;)V` with two flat arguments and built
            // an empty array (`probes/MhVarargsNullProbe.java` B18, measured
            // `H[]` against HotSpot's `H[a, b]`).
            let collected = match entry_call_site.as_deref() {
                Some(cs) => match mh_entry_adapt(ctx, mh, cs, &desc, is_collector, &extra_args) {
                    Ok(collected) => collected,
                    Err(e) => {
                        let abandoned = ctx.read_native_pin(new_obj_pin, new_obj);
                        let epoch = ctx.gc_address_epoch();
                        ctx.withdraw_finalizable_before_constructor(abandoned, epoch);
                        ctx.unpin_native_roots(mh_pin);
                        return Err(e);
                    }
                },
                None => collect_trailing_varargs(
                    ctx,
                    &class,
                    &name,
                    &desc,
                    &extra_args,
                    generic_collector,
                ),
            };
            // `asType`'s conversion, fallible (wave 26; see the STATIC arm).
            let site = leaf_site_params(entry_call_site.as_deref(), 0, collected.len());
            let explicit = mh_explicit_scope();
            let adapted = match adapt_leaf_args(ctx, &collected, &desc, site.as_deref(), explicit) {
                Ok(adapted) => adapted,
                Err(e) => {
                    let abandoned = ctx.read_native_pin(new_obj_pin, new_obj);
                    let epoch = ctx.gc_address_epoch();
                    ctx.withdraw_finalizable_before_constructor(abandoned, epoch);
                    ctx.unpin_native_roots(mh_pin);
                    return Err(e);
                }
            };
            let new_obj = ctx.read_native_pin(new_obj_pin, new_obj);
            let mut init_args = Vec::with_capacity(1 + adapted.len());
            init_args.push(Value::Object(Some(new_obj)));
            init_args.extend_from_slice(&adapted);
            // A throwing `<init>` releases this arm's pins too (it returned
            // with them held). The constructor unwinder note distinguishes a
            // throw before Object.<init> returned from one after construction.
            let ctor_epoch = ctx.gc_address_epoch();
            if let Err(e) = ctx.invoke_by_class_id(cid, &class, "<init>", &desc, &init_args) {
                let current = ctx.read_native_pin(new_obj_pin, new_obj);
                ctx.withdraw_unconstructed_finalizable(current, ctor_epoch);
                ctx.unpin_native_roots(mh_pin);
                return Err(e);
            }
            let new_obj = ctx.read_native_pin(new_obj_pin, new_obj);
            ctx.unpin_native_roots(mh_pin);
            Ok(Some(Value::Object(Some(new_obj))))
        }
        MH_KIND_GETTER => {
            // Field getter: static when the descriptor has no param (e.g.
            // "()J" after `unreflectGetter` on a static field); otherwise
            // receiver-based.  For static the class name identifies the
            // declaring class whose static field we read; for instance,
            // the first extra arg (or `bound`) is the receiver.
            let is_static = desc.starts_with("()");
            if is_static {
                // Resolve the STATIC field's storage slot (the static
                // index space is separate from instance indices — use the
                // dedicated name→index helper).
                // gen r4w3/rooting: read `mh` before `<clinit>` (a GC point)
                // can move it.
                let boxes = direct_return_boxes(ctx, mh, &desc);
                // An inherited static reads and initializes its declaring
                // class, never the class it is named through (wave 16).
                let holder = mh_inherited_static_field_holder(&*ctx, bound, &class, &name);
                // A failed `<clinit>` throws, as `getstatic` would (wave 17;
                // this arm answered `null` / `0`).
                // Its failure lists `DirectMethodHandle.ensureInitialized`'s
                // frames (wave 59, lane L4).
                let class_id = match (holder, mh_static_owner(bound)) {
                    (Some(owner), _) | (None, Some(owner)) => {
                        dmh_initialize_served(ctx, owner, DmhInitBarrier::StaticField)?;
                        ctx.ensure_class_initialized_with_class_id(owner)?;
                        owner
                    }
                    (None, None) => ctx.ensure_class_initialized_served(&class, &|| {
                        dmh_class_init_chain(DmhInitBarrier::StaticField)
                    })?,
                };
                let slot = ctx.static_field_index_by_name(class_id, &name).unwrap_or(0);
                let result = Ok(Some(ctx.get_static_field(class_id, slot)));
                // A volatile load's acquire, whatever the field (wave 79, lane
                // L4; free on x86-64; see the `SETTER` arm).
                field_handle_volatile_load_fence(true);
                box_direct_primitive_return_pre(ctx, boxes, result, &desc)
            } else {
                // Stage 2's slot owner (wave 63, `CRATONVM_MH_OWNER_ID_AUTHORITY`),
                // read while `mh` is current, and the slot carried from mint
                // (wave 64, `CRATONVM_MH_FIELD_SLOT_FROM_MINT`).
                let field_owner = mh_field_slot_owner(&*ctx, mh);
                let minted = mh_field_arm_minted_slot(&*ctx, mh, field_owner);
                let receiver = match bound {
                    Value::Object(Some(r)) => r,
                    _ => match extra_args.first() {
                        // An unbound receiver is cast to the field's class
                        // before its slot is read (wave 27; `checkBase` on
                        // HotSpot): an adapter handed this arm a `String`
                        // and the `Cell` slot index was applied to it. A
                        // bound one was checked by `bindTo`.
                        Some(Value::Object(Some(r))) => {
                            if let Some(e) = leaf_receiver_refusal_owned(
                                ctx,
                                mh,
                                *r,
                                &class,
                                mh_explicit_scope(),
                            ) {
                                return Err(e);
                            }
                            *r
                        }
                        // `checkBase`'s `Objects.requireNonNull`: HotSpot's
                        // message-less NPE (wave 27; this answered `null`),
                        // listed under those two frames (wave 56, lane L3).
                        Some(Value::Object(None)) => {
                            note_field_handle_check_base_chain(ctx);
                            return Err(RuntimeError::NullPointerException { message: None }.into());
                        }
                        _ => return Ok(Some(Value::Object(None))),
                    },
                };
                let slot =
                    minted.or_else(|| mh_instance_field_slot(&*ctx, field_owner, &class, &name));
                let result = match slot {
                    // zt t2/nd: a getter handle over `Reference.referent` keeps
                    // the referent alive (`slot0_read_keep_alive`).
                    Some(idx) => {
                        let value = ctx.get_field(receiver, idx);
                        let value =
                            crate::unsafe_natives::slot0_read_keep_alive(ctx, receiver, idx, value);
                        Ok(Some(value))
                    }
                    // A lambda proxy is no class-store class: its captured `arg$N`
                    // fields are the synthetic ones `getDeclaredFields` reports, and
                    // a getter handle over one (`Field.get` of a capture, which
                    // AssertJ's `extracting("..arg$1..")` reads) answered `null`.
                    None => match crate::lang_class::lambda_capture_slot(&*ctx, receiver, &name) {
                        Some(idx) => Ok(Some(ctx.get_field(receiver, idx))),
                        None => Ok(Some(ctx.get_field_by_name(receiver, &name))),
                    },
                };
                // A volatile load's acquire, whatever the field (wave 79).
                field_handle_volatile_load_fence(true);
                box_direct_primitive_return(ctx, mh, result, &desc)
            }
        }
        MH_KIND_SETTER => {
            // Field setter: static when descriptor has a single param,
            // instance when it has two (owner, value).
            let is_static = !desc_has_two_params(&desc);
            // The field value is the LAST descriptor param; unbox a boxed
            // wrapper to the field's primitive type (Jackson 3 POJO field
            // injection passes boxed values via invokeExact). Without this a
            // boxed Integer was stored into an `int` field as its pointer.
            //
            // Wave 26: the conversion is `asType`'s, fallible
            // (`adapt_leaf_arg`). A setter behind an adapter
            // (`insertArguments(setter, 0, box).invoke((Object) "s")`) stored
            // the String's address into the `int` field.
            let value_type: Option<String> =
                split_descriptor_params(&desc).and_then(|(params, _)| params.last().cloned());
            if is_static {
                let mut value = extra_args.first().copied().unwrap_or(Value::Object(None));
                if let Some(vt) = &value_type {
                    let explicit = mh_explicit_scope();
                    value = adapt_leaf_arg(ctx, value, vt, None, explicit)?;
                    // A reference field is given only a value of its type
                    // (wave 27): the cast `asType` makes.
                    if let Some(e) = leaf_reference_refusal(ctx, value, vt, explicit) {
                        return Err(e);
                    }
                }
                // gen r4w3/rooting: `value` must survive the owner's
                // `<clinit>` (a GC point) before it is stored.
                let (value_pin, value_handles) = pin_mh_args(ctx, std::slice::from_ref(&value));
                // The getter arm's declaring-class rule (wave 16).
                let holder = mh_inherited_static_field_holder(&*ctx, bound, &class, &name);
                // Its failure lists `DirectMethodHandle.ensureInitialized`'s
                // frames (wave 59, lane L4).
                let init = match (holder, mh_static_owner(bound)) {
                    (Some(owner), _) | (None, Some(owner)) => {
                        dmh_initialize_served(ctx, owner, DmhInitBarrier::StaticField)
                            .and_then(|()| ctx.ensure_class_initialized_with_class_id(owner))
                            .map(|()| owner)
                    }
                    (None, None) => ctx.ensure_class_initialized_served(&class, &|| {
                        dmh_class_init_chain(DmhInitBarrier::StaticField)
                    }),
                };
                let value = read_pinned_mh_arg(ctx, value_handles[0], value);
                if value_pin != usize::MAX {
                    ctx.unpin_native_roots(value_pin);
                }
                // A failed `<clinit>` throws, as `putstatic` would (wave 17;
                // this arm dropped the write and answered normally).
                let class_id = init?;
                if let Some(slot) = ctx.static_field_index_by_name(class_id, &name) {
                    // Fenced as a volatile store whatever the field (wave 79,
                    // lane L4; [`field_handle_volatile_store`]): the door does
                    // not know the field's flags without a class-store walk per
                    // call, and a stronger order is always a legal one. Its
                    // cost (two fences) is small beside the door's own.
                    field_handle_volatile_store(true, || {
                        ctx.set_static_field(class_id, slot, value)
                    });
                }
                Ok(None)
            } else {
                // Stage 2's slot owner (wave 63), read while `mh` is current,
                // and the slot carried from mint (wave 64).
                let field_owner = mh_field_slot_owner(&*ctx, mh);
                let minted = mh_field_arm_minted_slot(&*ctx, mh, field_owner);
                let (receiver, mut value) = match bound {
                    Value::Object(Some(r)) => {
                        let v = extra_args.first().copied().unwrap_or(Value::Object(None));
                        (r, v)
                    }
                    _ => {
                        let r = match extra_args.first() {
                            Some(Value::Object(Some(r))) => *r,
                            // `checkBase`'s message-less NPE (wave 27; this
                            // dropped the write and answered normally),
                            // listed under its frames (wave 56, lane L3).
                            Some(Value::Object(None)) => {
                                note_field_handle_check_base_chain(ctx);
                                return Err(
                                    RuntimeError::NullPointerException { message: None }.into()
                                );
                            }
                            _ => return Ok(None),
                        };
                        // The unbound receiver is cast to the field's class
                        // before anything is written through it (wave 27; see
                        // the GETTER arm): `insertArguments(setter, 1, 5)
                        // .invoke((Object) "str")` wrote `5` into the `String`.
                        if let Some(e) =
                            leaf_receiver_refusal_owned(ctx, mh, r, &class, mh_explicit_scope())
                        {
                            return Err(e);
                        }
                        let v = extra_args.get(1).copied().unwrap_or(Value::Object(None));
                        (r, v)
                    }
                };
                if let Some(vt) = &value_type {
                    let explicit = mh_explicit_scope();
                    value = adapt_leaf_arg(ctx, value, vt, None, explicit)?;
                    // A reference field is given only a value of its type
                    // (wave 27): a `String` field received an `Integer`.
                    if let Some(e) = leaf_reference_refusal(ctx, value, vt, explicit) {
                        return Err(e);
                    }
                }
                // Fenced as a volatile store (wave 79, lane L4; see the static
                // arm above).
                let slot =
                    minted.or_else(|| mh_instance_field_slot(&*ctx, field_owner, &class, &name));
                field_handle_volatile_store(true, || match slot {
                    Some(idx) => ctx.set_field(receiver, idx, value),
                    None => ctx.set_field_by_name(receiver, &name, value),
                });
                Ok(None)
            }
        }
        MH_KIND_PERMUTE => {
            // Permute adapter: bound (field 4) holds a wrapper synthetic with:
            //   field 0 = target MH, field 1 = int[] reorder array
            let wrapper: ObjectRef = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target_mh = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let reorder_arr: ObjectRef = match ctx.get_field(wrapper, 1) {
                Value::Object(Some(a)) => a,
                _ => {
                    // No reorder array — pass through directly
                    set_mh_tail(tail);
                    return mh_dispatch(ctx, target_mh, extra_args);
                }
            };
            let reorder_len = ctx.array_length(reorder_arr);
            let mut permuted_args = Vec::with_capacity(reorder_len);
            let mut reorder_vals: Vec<i32> = Vec::with_capacity(reorder_len);
            for i in 0..reorder_len {
                let idx = match ctx.get_array_element(reorder_arr, i) {
                    Value::Int(v) => v as usize,
                    _ => i,
                };
                reorder_vals.push(idx as i32);
                let val = extra_args.get(idx).copied().unwrap_or(Value::Object(None));
                permuted_args.push(val);
            }
            if crate::nbflags().dbg_mh_dispatch {
                let target_desc = mh_read_desc(ctx, target_mh).unwrap_or_default();
                eprintln!(
                    "[MH_PERMUTE] reorder={reorder_vals:?} extra_args_len={} target_desc={target_desc:?}",
                    extra_args.len()
                );
            }
            set_mh_tail(tail);
            mh_dispatch(ctx, target_mh, &permuted_args)
        }
        MH_KIND_DROP => {
            // C26: dropArgumentsTrusted wrapper. Unwrap to inner MH (in
            // MH_BOUND) and forward only the inner MH's expected args.
            //
            // The drop position AND count are encoded in MH_CLASS as
            // "pos:count" (set by `make_drop_arguments_adapter` at
            // construction time -- see its own comment). This USED to
            // re-derive `drop_n` at dispatch time as
            // `extra_args.len() - inner_expected` (inner_expected computed
            // by re-parsing the INNER handle's reported descriptor/kind).
            // That heuristic silently produces the wrong `drop_n` (and then
            // a wrong, silently-clamped `pos`) whenever this DROP adapter is
            // itself nested inside further combinators (JRuby's
            // `org.jruby.ir.targets.indy.InvokeSite` composes SIX
            // `dropArguments` + SIX `insertArguments` calls per call site) --
            // any drift in what the inner handle's descriptor reports as its
            // effective arity (e.g. a receiver-detection edge case, or an
            // inner adapter whose own widened `type` field doesn't exactly
            // match its TRUE effective arity) throws off the subtraction,
            // which then throws off every downstream drop in the chain.
            // Confirmed via `CRATONVM_DBG_INDY_GENERIC` tracing on
            // `rubygems/version.rb`'s `@version.sub(regex, "")` call
            // (reached through exactly this `InvokeSite` machinery): the
            // receiver slot held `self` instead of `@version`'s string, and
            // a stray `Regexp` literal landed in the replacement-string/
            // block slots -- a classic "wrong args kept as the `pos` prefix"
            // symptom of a silently-mis-clamped `pos`/`drop_n` pair.
            let inner: ObjectRef = match bound {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let inner_desc = mh_type_descriptor(ctx, inner)
                .or_else(|| mh_read_desc(ctx, inner))
                .unwrap_or_default();
            let inner_params = count_descriptor_params(&inner_desc);
            let inner_kind = match ctx.get_field(inner, MH_KIND) {
                Value::Int(k) => k,
                _ => MH_KIND_STATIC,
            };
            let inner_needs_recv = inner_kind == MH_KIND_VIRTUAL || inner_kind == MH_KIND_SPECIAL;
            let inner_bound = matches!(ctx.get_field(inner, MH_BOUND), Value::Object(Some(_)));
            let inner_expected = inner_params
                + if inner_needs_recv && !inner_bound {
                    1
                } else {
                    0
                };
            let class_str = mh_read_class(ctx, mh);
            let (pos, drop_n): (usize, usize) = class_str
                .as_deref()
                .and_then(|s| {
                    let mut parts = s.splitn(2, ':');
                    let p = parts.next()?.parse::<usize>().ok()?;
                    let n = parts.next()?.parse::<usize>().ok()?;
                    Some((p, n))
                })
                // Defensive fallback for a DROP handle whose MH_CLASS wasn't
                // encoded in the "pos:count" format (shouldn't happen via
                // `make_drop_arguments_adapter`, but avoid a hard failure on
                // an unexpected encoding): fall back to the old
                // arg-count-difference heuristic.
                .unwrap_or_else(|| {
                    let p = class_str
                        .as_deref()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let n = extra_args.len().saturating_sub(inner_expected);
                    (p, n)
                });
            let drop_n = drop_n.min(extra_args.len());
            let pos = pos.min(extra_args.len().saturating_sub(drop_n));
            let mut trimmed: Vec<Value> = Vec::with_capacity(inner_expected);
            trimmed.extend_from_slice(&extra_args[..pos]);
            if pos + drop_n < extra_args.len() {
                trimmed.extend_from_slice(&extra_args[pos + drop_n..]);
            }
            set_mh_tail(tail);
            mh_dispatch(ctx, inner, &trimmed)
        }
        MH_KIND_GUARD => {
            // Guard adapter: bound (field 4) holds a wrapper synthetic with:
            //   field 0 = test MH, field 1 = target MH, field 2 = fallback MH
            let wrapper: ObjectRef = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let test_mh = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target_mh = match ctx.get_field(wrapper, 1) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let fallback_mh = match ctx.get_field(wrapper, 2) {
                Value::Object(Some(f)) => f,
                _ => return Ok(Some(Value::Object(None))),
            };
            // guardWithTest calls the test with the prefix of the target
            // arguments matching the test handle's own parameter list. Passing
            // all target args makes shorter Groovy guard predicates
            // (sameClasses/isSameMetaClass after dropArguments) miss forever.
            let test_desc = mh_type_descriptor(ctx, test_mh)
                .or_else(|| mh_read_desc(ctx, test_mh))
                .unwrap_or_default();
            let test_argc = count_descriptor_params(&test_desc).min(extra_args.len());
            // GC-safety: this recursive `mh_dispatch` (running the test
            // handle) can trigger a collection that relocates
            // `target_mh`/`fallback_mh` (both captured above, one of which
            // is dispatched again below depending on the test result).
            let target_pin = ctx.pin_native_root(target_mh);
            let fallback_pin = ctx.pin_native_root(fallback_mh);
            // gen r4w3/rooting: the incoming args are re-passed to the target
            // or fallback after the test dispatch (a GC point); pin them too.
            let (_args_pin, arg_handles) = pin_mh_args(ctx, extra_args);
            // The guard hands its own arguments to the test and to the chosen
            // branch unchanged, so a collector inside them boxes a raw `Z` by the
            // GUARD's type (see `MH_ADAPTER_SITE`).
            let guard_site = mh_type_descriptor(ctx, mh);
            let outer_site = swap_adapter_site(guard_site.clone());
            let test_result = mh_dispatch(ctx, test_mh, &extra_args[..test_argc]);
            let _ = swap_adapter_site(outer_site.clone());
            let target_mh = ctx.read_native_pin(target_pin, target_mh);
            let fallback_mh = ctx.read_native_pin(fallback_pin, fallback_mh);
            let args = read_pinned_mh_args(ctx, &arg_handles, extra_args);
            ctx.unpin_native_roots(target_pin);
            let test_result = test_result?;
            let is_true = mh_guard_truthy(ctx, test_result);
            // The chosen branch is the guard's tail call (the test above was
            // not: its answer is read afterwards).
            set_mh_tail(tail);
            if is_true {
                let _ = swap_adapter_site(guard_site);
                let r = mh_dispatch(ctx, target_mh, &args);
                let r = box_adapter_result(ctx, mh, target_mh, r);
                let _ = swap_adapter_site(outer_site);
                r
            } else {
                let _ = swap_adapter_site(guard_site);
                let r = mh_dispatch(ctx, fallback_mh, &args);
                let r = box_adapter_result(ctx, mh, fallback_mh, r);
                let _ = swap_adapter_site(outer_site);
                r
            }
        }
        MH_KIND_STRING_CONCAT => {
            // Round-9: real StringConcatFactory target. Recipe is in
            // MH_CLASS, constants Object[] is field 0 of MH_BOUND. Walk
            // the recipe char-by-char, interpolating dynamic args
            // (`\u{0001}`) and pre-baked constants (`\u{0002}`).
            let recipe = mh_read_class(ctx, mh).unwrap_or_default();
            let constants_arr: Option<cratonvm_types::ObjectRef> = match bound {
                Value::Object(Some(holder)) => match ctx.get_field(holder, 0) {
                    Value::Object(Some(a)) => Some(a),
                    _ => None,
                },
                _ => None,
            };
            // GC-safety: `string_concat_render_value` below can trigger a
            // collection (it may call a dynamic arg's real `toString()`); a
            // GC during ANY loop iteration can relocate `constants_arr`'s
            // object, which is then read again on a LATER iteration. Pin it
            // once for the whole recipe walk and re-read the forwarded
            // reference before each use.
            let constants_pin = constants_arr.map(|a| ctx.pin_native_root(a));
            // gen r4w3/rooting: an earlier arg's `toString()` is a GC point;
            // pin every dynamic arg and re-read it at its turn. Pinned after
            // `constants_pin`, so the unpin below releases these too.
            let (args_pin, arg_handles) = pin_mh_args(ctx, extra_args);
            let mut out = String::with_capacity(recipe.len() + 16);
            let mut arg_idx: usize = 0;
            let mut const_idx: usize = 0;
            for ch in recipe.chars() {
                match ch {
                    '\u{0001}' => {
                        let v = match extra_args.get(arg_idx) {
                            Some(a) => read_pinned_mh_arg(ctx, arg_handles[arg_idx], *a),
                            None => Value::Object(None),
                        };
                        out.push_str(&string_concat_render_value(ctx, v));
                        arg_idx += 1;
                    }
                    '\u{0002}' => {
                        if let (Some(arr), Some(pin)) = (constants_arr, constants_pin) {
                            let arr = ctx.read_native_pin(pin, arr);
                            if const_idx < ctx.array_length(arr) {
                                let v = ctx.get_array_element(arr, const_idx);
                                out.push_str(&string_concat_render_value(ctx, v));
                            }
                        }
                        const_idx += 1;
                    }
                    c => out.push(c),
                }
            }
            if let Some(pin) = constants_pin {
                ctx.unpin_native_roots(pin);
            } else if args_pin != usize::MAX {
                ctx.unpin_native_roots(args_pin);
            }
            let s = ctx.create_string(&out);
            Ok(Some(Value::Object(Some(s))))
        }
        MH_KIND_LAMBDA_FACTORY => {
            // Reflective lambda factory (see `build_reflective_lambda_callsite`).
            // MH_CLASS = synthetic proxy ClassId (decimal); MH_DESC = factory
            // signature `(captures...)FI`. Gather the captures — those bound via
            // `bindTo` (MH_BOUND Object[]) followed by any direct invoke args —
            // allocate a proxy instance, and return it. The interpreter's SAM
            // dispatch then routes the proxy's abstract method to the impl.
            let proxy_raw: u32 = mh_read_class(ctx, mh)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if proxy_raw == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let proxy_cid = cratonvm_types::ClassId::new(proxy_raw);
            let num_captures = count_descriptor_params(&desc);

            let mut captures: Vec<Value> = Vec::with_capacity(num_captures);
            if let Value::Object(Some(arr)) = bound {
                let blen = ctx.array_length(arr);
                for i in 0..blen {
                    captures.push(ctx.get_array_element(arr, i));
                }
            }
            captures.extend_from_slice(extra_args);
            // Defensive: a well-formed factory yields exactly `num_captures`
            // values, but tolerate over/under supply rather than corrupting the
            // proxy layout.
            captures.truncate(num_captures);
            while captures.len() < num_captures {
                captures.push(Value::Object(None));
            }

            // gc-common w15-e: the captures are stored AFTER the proxy's
            // allocation; root them across it and store the re-read copies.
            let (captures_pin, capture_handles) = pin_mh_args(ctx, &captures);
            let proxy = ctx.alloc_object(proxy_cid, num_captures);
            let captures = read_pinned_mh_args(ctx, &capture_handles, &captures);
            if captures_pin != usize::MAX {
                ctx.unpin_native_roots(captures_pin);
            }
            for (i, v) in captures.iter().enumerate() {
                ctx.set_field(proxy, i, *v);
            }
            Ok(Some(Value::Object(Some(proxy))))
        }
        MH_KIND_SPECIAL => {
            // WP2.9 — invokespecial semantics: dispatch *exactly* on the
            // resolved class, no virtual lookup, no iface retarget. Used
            // for private-to-private calls and `I.super.m()` default-method
            // super-call patterns.
            //
            // The MH carries the resolved class name in MH_CLASS. We build
            // an args vector with the receiver at index 0 and forward via
            // `invoke_special` (which the Vm impl routes through the
            // no-retarget path). For bound MHs, the receiver is pre-captured.
            let mut full_args: Vec<Value>;
            let class_for_dispatch = class.clone();
            match bound {
                Value::Object(Some(r)) => {
                    full_args = Vec::with_capacity(1 + extra_args.len());
                    full_args.push(Value::Object(Some(r)));
                    full_args.extend_from_slice(extra_args);
                }
                _ => {
                    if extra_args.is_empty() {
                        return Ok(Some(Value::Object(None)));
                    }
                    // The unbound receiver is cast (wave 27; see the VIRTUAL
                    // arm) to `findSpecial`'s `specialCaller`, as HotSpot
                    // casts it, since wave 28: round 12 records it on the
                    // handle (`MH_SPECIAL_CALLER`, read by
                    // `mh_leaf_receiver_class`) and the `invoke` /
                    // `invokeExact` doors already cast to it, but this arm --
                    // reached through an adapter -- cast to `MH_CLASS`, the
                    // dispatch class, so
                    // `dropArguments(explicitCastArguments(findSpecial(Mid,
                    // m, ()int, Sub), (Object)int), 0, int).invoke(1, (Object)
                    // new Mid())` ran `Mid.m` on a `Mid` where HotSpot 25.0.3
                    // throws `Cannot cast Mid to Sub`. Without a recorded
                    // caller (`CRATONVM_MH_SPECIAL_CALLER_CAST=0`) the cast is
                    // to `MH_CLASS` and the refusal names the handle's own
                    // receiver type, as before.
                    // Wave 64 (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`): the recorded
                    // `specialCaller` id judges first, read while `mh` is current.
                    let caller_verdict = match extra_args[0] {
                        Value::Object(Some(r)) => {
                            match mh_special_caller_with(
                                &*ctx,
                                mh,
                                mh_special_by_owner_id_enabled(),
                            ) {
                                Some(caller) => special_caller_receiver_verdict(ctx, r, caller),
                                None => None,
                            }
                        }
                        _ => None,
                    };
                    match caller_verdict {
                        Some(Some(e)) => return Err(e),
                        Some(None) => {}
                        None => {
                            if let Value::Object(Some(r)) = extra_args[0] {
                                let receiver_class =
                                    mh_leaf_receiver_class(&*ctx, mh, class.clone());
                                if let Some(e) = leaf_receiver_refusal(
                                    ctx,
                                    r,
                                    &receiver_class,
                                    mh_explicit_scope(),
                                ) {
                                    if receiver_class != class {
                                        return Err(e);
                                    }
                                    return Err(special_receiver_refusal_named(ctx, mh, r, e));
                                }
                            }
                        }
                    }
                    full_args = extra_args.to_vec();
                }
            }
            // gen r4w3/rooting: read `mh` before the invoke (a GC point).
            let boxes = direct_return_boxes(ctx, mh, &desc);
            // The dispatch class by id (wave 64, `CRATONVM_MH_SPECIAL_BY_OWNER_ID`),
            // read while `mh` is current.
            let special_owner = mh_special_owner_with(&*ctx, mh, mh_special_by_owner_id_enabled());
            // `asType`'s conversion of the arguments after the receiver
            // (`MH_DESC` omits it), which this arm never ran (wave 26):
            // `findSpecial(..).invokeWithArguments(recv, 21)` handed `(int)`
            // the boxed `Integer`, and an adapter's reference reached the
            // primitive parameter as is. The receiver is rooted across it.
            // Round 15 wave 5 (lane mh4): a receiver alone with parameters
            // owed is `adapt_leaf_args`' short-arity refusal, not a call.
            if full_args.len() > 1 || count_descriptor_params(&desc) > 0 {
                let recv_pin = pin_mh_value(ctx, full_args[0]);
                let site = leaf_site_params(entry_call_site.as_deref(), 0, full_args.len() - 1);
                let explicit = mh_explicit_scope();
                let rest = adapt_leaf_args(ctx, &full_args[1..], &desc, site.as_deref(), explicit);
                full_args[0] = read_pinned_mh_arg(ctx, recv_pin, full_args[0]);
                if recv_pin != usize::MAX {
                    ctx.unpin_native_roots(recv_pin);
                }
                let rest = rest?;
                full_args.truncate(1);
                full_args.extend_from_slice(&rest);
            }
            // Transparent to a yield in a reference-returning tail call
            // (`MH_TAIL`, wave 26).
            let transparent = transparent_leaf_call(&*ctx, tail, &desc, None);
            // Round 15 wave 1 (lane mh): the receiver rooted across the call
            // and re-read after it, so the neutral hook answers where the call
            // left it (`r15w1-mh-neutral-serialization-hook-answers-a-stale-receiver`).
            let hook_pin =
                mh_neutral_hook_current_receiver_enabled().then(|| pin_mh_value(ctx, full_args[0]));
            let result = match special_owner {
                Some(owner) => {
                    note_special_route("door-special-arm", &class_for_dispatch, &name, &desc, true);
                    ctx.invoke_special_by_class_id(
                        owner,
                        &class_for_dispatch,
                        &name,
                        &desc,
                        &full_args,
                    )
                }
                None => {
                    note_special_route(
                        "door-special-arm",
                        &class_for_dispatch,
                        &name,
                        &desc,
                        false,
                    );
                    ctx.invoke_special(&class_for_dispatch, &name, &desc, &full_args)
                }
            };
            transparent_leaf_call_done(ctx, transparent, &result);
            if let Some(pin) = hook_pin {
                full_args[0] = read_pinned_mh_arg(ctx, pin, full_args[0]);
                if pin != usize::MAX {
                    ctx.unpin_native_roots(pin);
                }
            }
            box_direct_primitive_return_pre(
                ctx,
                boxes,
                neutralize_missing_serialization_hook(result, &full_args),
                &desc,
            )
        }
        MH_KIND_RECORD_DESER => {
            // Record deserialization constructor. extra_args =
            // [primValues:byte[], objValues:Object[]] as passed by
            // `ObjectInputStream.readRecord`. Reflectively rebuild the record.
            record_deser_dispatch(ctx, mh, extra_args)
        }
        // `CallSite.makeUninitializedCallSite`'s placeholder (the target of a
        // `new MutableCallSite(type)` before its first `setTarget`): HotSpot's
        // is `CallSite.uninitializedCallSite`, which throws
        // `IllegalStateException: uninitialized call site` (measured on JDK
        // 25). It answered `null` / zero here (interpreter round i1 wave 47,
        // lane L4, probe `L4W47UninitializedCallSite`).
        MH_KIND_CONSTANT if class.is_empty() && name == UNINITIALIZED_CALL_SITE_NAME => {
            Err(RuntimeError::IllegalStateException {
                message: "uninitialized call site".to_string(),
            }
            .into())
        }
        // `throwException(rtype, exType)` (wave 48): throw the bound exception
        // (after `bindTo`) or the first argument. A `null` one is the NPE of
        // `MethodHandleImpl.throwException(T t)`'s `athrow` (measured, HotSpot
        // 25, `L4W48ThrowException` row `null-arg`).
        MH_KIND_THROW => {
            let thrown = match bound {
                Value::Object(Some(t)) => Some(t),
                _ => match extra_args.first() {
                    Some(Value::Object(Some(t))) => Some(*t),
                    _ => None,
                },
            };
            // An unbound handle's argument is cast to `exType` first, as the
            // JDK's `invoke` adapter casts it (measured: row `wrong-class`,
            // `ClassCastException: Cannot cast java.lang.RuntimeException to
            // java.lang.IllegalStateException`). `bindTo` cast its value.
            if let (Some(t), Value::Object(None)) = (thrown, bound) {
                if let Some((params, _)) = split_descriptor_params(&desc) {
                    if let Some(want) = params.first() {
                        let v = Value::Object(Some(t));
                        if reference_arg_admitted(ctx, v, want) == Some(false) {
                            return Err(reference_cast_failure(ctx, v, want));
                        }
                    }
                }
            }
            match thrown {
                Some(t) => Err(MethodCallFailed::ExceptionThrown(t)),
                None => Err(RuntimeError::NullPointerException {
                    message: Some("Cannot throw exception because \"t\" is null".to_string()),
                }
                .into()),
            }
        }
        MH_KIND_CONSTANT => {
            // constant(type, value): nullary handle that ignores its arguments
            // and returns the captured value held in MH_BOUND. When the handle's
            // return type is a primitive, unbox the captured wrapper so an
            // `invokeExact()Z`/`()I`/… observes the raw value (and so the GUARD
            // arm's `Int(v) => v != 0` boolean test stays correct). For a
            // reference return type the captured object passes through unchanged;
            // the signature-polymorphic return adapter re-boxes if the call site
            // wants `Object`.
            let ret = return_type_desc(&desc);
            let v = match ret {
                "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" => match bound {
                    Value::Object(Some(obj)) => crate::lang_class::unbox_value(ctx, obj),
                    other => other,
                },
                _ => bound,
            };
            Ok(Some(v))
        }
        MH_KIND_IDENTITY => {
            // identity(type): return the single incoming argument unchanged.
            // After `bindTo(x)` the argument is pre-captured in MH_BOUND.
            let v = match bound {
                Value::Object(Some(_)) => bound,
                // The argument reaches `(T)T` through the handle's own
                // conversion (wave 48): an `explicitCastArguments` copy
                // narrows and unboxes (`ConstantBootstraps.explicitCast(byte,
                // 300)` is `44` on HotSpot, it answered `300`) and casts a
                // reference (`explicitCast(String, 3)` is a
                // `ClassCastException`, it answered `3`); an `asType` copy
                // converts as `asType` does. Unadapted, a no-op.
                _ if extra_args.len() == 1 && !desc.is_empty() => {
                    let site = leaf_site_params(entry_call_site.as_deref(), 0, 1);
                    let explicit = mh_explicit_scope();
                    adapt_leaf_args(ctx, extra_args, &desc, site.as_deref(), explicit)?
                        .first()
                        .copied()
                        .unwrap_or(Value::Object(None))
                }
                _ => extra_args.first().copied().unwrap_or(Value::Object(None)),
            };
            Ok(Some(v))
        }
        MH_KIND_DYNAMIC_INVOKER => {
            // CallSite.dynamicInvoker(): delegate to the call site's CURRENT
            // target. MH_BOUND holds the call site; read its `target` field
            // (real CallSite layout) — falling back to the synthetic CallSite
            // model's slot 0 — and dispatch with the incoming args. Avoids the
            // real `makeDynamicInvoker` → `bindArgumentL` → BoundMethodHandle
            // species path (which hangs).
            let callsite: ObjectRef = match bound {
                Value::Object(Some(cs)) => cs,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target = match ctx.get_field_by_name(callsite, "target") {
                Value::Object(Some(t)) => t,
                _ => match ctx.get_field(callsite, 0) {
                    Value::Object(Some(t)) => t,
                    _ => return Ok(Some(Value::Object(None))),
                },
            };
            set_mh_tail(tail);
            mh_dispatch(ctx, target, extra_args)
        }
        MH_KIND_INSERT => {
            // insertArguments(target, pos, values): splice the pre-bound
            // `values` into the incoming args at `pos`, then dispatch target.
            let wrapper: ObjectRef = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let values = ctx.get_field(wrapper, 1);
            let pos = match ctx.get_field(wrapper, 2) {
                Value::Int(p) => (p as usize).min(extra_args.len()),
                _ => 0,
            };
            let mut full: Vec<Value> = Vec::with_capacity(extra_args.len() + 4);
            full.extend_from_slice(&extra_args[..pos]);
            if let Value::Object(Some(arr)) = values {
                let n = ctx.array_length(arr);
                for i in 0..n {
                    full.push(ctx.get_array_element(arr, i));
                }
            }
            full.extend_from_slice(&extra_args[pos..]);
            set_mh_tail(tail);
            mh_dispatch(ctx, target, &full)
        }
        MH_KIND_COLLECT => {
            // asCollector(arrayType, count): collect the trailing `count`
            // incoming args into a fresh array OF THE COLLECTOR'S OWN ARRAY
            // TYPE and append to the leading args, then dispatch target (whose
            // last param is that array).
            //
            // W7-19: "of the collector's own array type" is the fix. This arm
            // built an `Object[]` unconditionally, so every primitive-array
            // collector handed its target a reference array where the target's
            // bytecode expects `int[]`/`long[]`/… — `iaload` then read an oop
            // as an int. Measured on the shipped `dev` binary under
            // `--real-jdk`: `int[]`→0, `byte[]`/`short[]`/`char[]`/
            // `boolean[]`→0, `float[]`→0.0, `double[]`→NaN, and `long[]`→
            // -2527743864898872 (a raw heap pointer read as a `long`), against
            // HotSpot 25's 6/6/60/131/2/3.75/7.0/6. `Object[]` and `String[]`
            // were already right, which is why the one probe that reached this
            // combinator did not see it — the same four-of-nine shape as the
            // FFM carrier defect: one reachable carrier is not the surface.
            let wrapper: ObjectRef = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let count = match ctx.get_field(wrapper, 1) {
                Value::Int(c) => (c as usize).min(extra_args.len()),
                _ => 0,
            };
            let leading = extra_args.len() - count;
            // Slot 2 is the `arrayType` Class mirror the `asCollector` native
            // captured. Absent (a carrier written before this field existed, or
            // an `asCollector` call whose Class argument was not an object) the
            // component descriptor stays `Ljava/lang/Object;` and this arm
            // behaves exactly as it did before — the pre-existing `Object[]`
            // path is the fallback, never a new failure mode.
            let arr_mirror = match ctx.get_field(wrapper, 2) {
                Value::Object(Some(m)) => Some(m),
                _ => None,
            };
            let comp = match arr_mirror {
                Some(m) => {
                    let ad = mirror_to_descriptor(ctx, m);
                    match ad.strip_prefix('[') {
                        Some(rest) => rest.to_string(),
                        // Not an array mirror at all. `asCollector` on a
                        // non-array type is an `IllegalArgumentException` on
                        // HotSpot and never reaches dispatch there; here it
                        // keeps the old container rather than inventing a new
                        // refusal at invoke time, which would be the wrong
                        // place for it.
                        None => DESC_OBJECT.to_string(),
                    }
                }
                None => DESC_OBJECT.to_string(),
            };
            let elem_type = array_element_type_of_descriptor(&comp);
            // gc-common w15-e: `target` and the incoming arguments are rooted
            // BEFORE the collector array's allocation (they used to be pinned
            // after it, and the arguments not at all although every element is
            // read after the previous element's box). `target_pin` is the
            // lowest pin of this arm.
            // No call site armed for THIS dispatch: fall back on the type of the
            // enclosing adapter that handed its arguments over unchanged (a
            // `guardWithTest` around Groovy's `sameClasses` collector).
            let site_hint: Option<String> = entry_call_site.clone().or_else(|| {
                current_adapter_site().filter(|site| {
                    crate::lang_class::parse_descriptor_param_and_return(site)
                        .0
                        .len()
                        >= extra_args.len()
                })
            });
            let target_pin = ctx.pin_native_root(target);
            let (_, arg_handles) = pin_mh_args(ctx, extra_args);
            let arr = if elem_type == cratonvm_types::ArrayElementType::Reference {
                // A typed reference array where the component class resolves
                // (`String[]`, not `Object[]`), because the target's parameter
                // is `String[]` and an `aastore`-checked or reflective consumer
                // can tell the difference. `Object[]` remains the fallback.
                let arr_cid = match arr_mirror {
                    Some(m) => ctx.class_id_from_mirror(m),
                    None => None,
                };
                let comp_cid = match arr_cid {
                    Some(acid) => ctx.array_component_class_id(acid),
                    None => None,
                };
                match comp_cid {
                    Some(cid) => ctx.new_ref_array(cid, count),
                    None => ctx.new_array(cratonvm_types::ArrayElementType::Reference, count),
                }
            } else {
                ctx.new_array(elem_type, count)
            };
            // GC-safety: `box_value` inside the loop below can trigger a
            // collection that relocates `arr` (created once, before the
            // loop, then written into on every iteration) and `target`
            // (captured earlier, dispatched only after the loop finishes).
            // Pin both and re-read the forwarded references before each use.
            // The primitive branch allocates nothing per element, but it shares
            // the loop and the pin costs a forwarding read, not a collection.
            let arr_pin = ctx.pin_native_root(arr);
            for i in 0..count {
                let v = read_pinned_mh_arg(ctx, arg_handles[leading + i], extra_args[leading + i]);
                let elem = if elem_type == cratonvm_types::ArrayElementType::Reference {
                    // Box primitive values into their wrappers — a reference
                    // collector gathers into an `Object[]`. The indy call site
                    // passes raw primitives (Groovy's `3 * 2` is
                    // `invoke(II)Object`), and the real JDK boxes them via the
                    // trailing `asType`; our `asType` shim is a passthrough, so
                    // box here. Without this, `selectMethod` receives raw
                    // `int`s in its `Object[] args` and Groovy's
                    // `args[0].getClass()` (Selector.setGuards) dereferences a
                    // raw int as an object → NPE.
                    //
                    // CANONICAL for `I`/`J` — measured `mhcoll.int` /
                    // `mhcoll.long` / `mhvar.int` = true. Out of bound is
                    // measured and needs no arm of its own:
                    // `mhcolloob.int1000` = false, which is what the `valueOf`
                    // native's own uncached path already produces.
                    //
                    // The wrapper CLASS comes from `comp` when `comp` settles
                    // it (`Character[]` -> `Character`, MEASURED, and HotSpot
                    // refuses every other static argument type for that
                    // collector); otherwise from the CALL SITE the caller
                    // wrote (F29-1 NOMINATION 2, closed via `entry_call_site`)
                    // when one is armed; otherwise from the `Value` variant,
                    // which is a KNOWN wrong answer for `char`/`boolean`/
                    // `byte`/`short` in the ordinary `Object[]` case. See
                    // `collector_element_box_desc` and
                    // `collector_call_site_prim`.
                    let elem = box_collector_element(
                        ctx,
                        v,
                        &comp,
                        collector_call_site_prim(site_hint.as_deref(), leading + i),
                    );
                    // The element is cast to the component (wave 27, the
                    // reference half of wave 26's element conversion): an
                    // `Integer` was stored into the `String[]` the target
                    // then read as `String`s.
                    if let Some(e) = leaf_reference_refusal(ctx, elem, &comp, mh_explicit_scope()) {
                        ctx.unpin_native_roots(target_pin);
                        return Err(e);
                    }
                    elem
                } else {
                    // The mirror image, for the same reason: a primitive
                    // collector's element slot is raw, and an argument that
                    // arrived boxed (`invokeWithArguments`, or an adapter chain
                    // that spread an `Object[]`) must be unwrapped or
                    // `write_prim_element` would see a `Value::Object` and
                    // store the type's zero. Then apply the widening the JDK's
                    // trailing `asType` would have applied — `asCollector
                    // (long[], 3).invoke(1, 2, 3)` passes `int`s and HotSpot
                    // widens them; our `asType` is a passthrough, so this is
                    // the only place it can happen.
                    //
                    // Wave 26: the element conversion is `asType`'s, fallible
                    // (`adapt_leaf_arg`): a `String`, a non-widening wrapper or
                    // a `null` element was stored as the component's zero (an
                    // array reinterpreted as a wrapper read its header). Under
                    // an `explicitCastArguments` stamp it casts instead.
                    match adapt_leaf_arg(ctx, v, &comp, None, mh_explicit_scope()) {
                        Ok(raw) => raw,
                        Err(e) => {
                            ctx.unpin_native_roots(target_pin);
                            return Err(e);
                        }
                    }
                };
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, elem);
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            let target = ctx.read_native_pin(target_pin, target);
            let leading_args =
                read_pinned_mh_args(ctx, &arg_handles[..leading], &extra_args[..leading]);
            ctx.unpin_native_roots(target_pin);
            let mut full: Vec<Value> = Vec::with_capacity(leading + 1);
            full.extend_from_slice(&leading_args);
            full.push(Value::Object(Some(arr)));
            set_mh_tail(tail);
            mh_dispatch(ctx, target, &full)
        }
        MH_KIND_ARRAY_LENGTH => {
            // `MethodHandles.arrayLength`: (T[])int, reads `array.length`.
            // After `bindTo(array)` the array is pre-captured in MH_BOUND,
            // exactly as MH_KIND_ARRAY_GET/_SET.
            let arr = match bound {
                Value::Object(Some(a)) => Some(a),
                _ => match extra_args.first() {
                    Some(Value::Object(Some(a))) => Some(*a),
                    _ => None,
                },
            };
            match arr {
                Some(a) => {
                    // Cast to the handle's array type first (wave 24).
                    if let Some(refusal) = mh_array_accessor_type_refusal(ctx, a, &desc) {
                        return Err(refusal);
                    }
                    Ok(Some(Value::Int(ctx.array_length(a) as i32)))
                }
                None => Err(mh_array_accessor_null(&desc, "length")),
            }
        }
        MH_KIND_ARRAY_GET | MH_KIND_ARRAY_SET => {
            // `MethodHandles.arrayElementGetter` / `arrayElementSetter`:
            //   getter  (T[],int)    -> T
            //   setter  (T[],int,T)  -> void
            // After `bindTo(array)` the array is pre-captured in MH_BOUND, so
            // prepend it exactly as the MH_KIND_STATIC arm does.
            let full: Vec<Value> = match bound {
                Value::Object(Some(_)) => {
                    let mut v = Vec::with_capacity(extra_args.len() + 1);
                    v.push(bound);
                    v.extend_from_slice(extra_args);
                    v
                }
                _ => extra_args.to_vec(),
            };
            let arr = match full.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Err(mh_array_accessor_null(
                        &desc,
                        if kind == MH_KIND_ARRAY_GET {
                            "load from"
                        } else {
                            "store to"
                        },
                    ));
                }
            };
            // The array argument is cast to the handle's array type first
            // (wave 24; see `mh_array_accessor_type_refusal`).
            if let Some(refusal) = mh_array_accessor_type_refusal(ctx, arr, &desc) {
                return Err(refusal);
            }
            // The call site's static parameter types, aligned with `full`
            // (whose first slot is the bound array when there is one), when
            // the door published a site that fits `extra_args`. They decide
            // only the wording of a null's NPE (`mh_unbox_for_primitive`), and
            // a site that typed an argument as a primitive passed it raw, so a
            // reference there is ours and is unboxed as before.
            let site_params: Option<Vec<String>> = entry_call_site.as_deref().and_then(|site| {
                let (params, _) = split_descriptor_params(site)?;
                (params.len() == extra_args.len()).then_some(params)
            });
            let shift = full.len() - extra_args.len();
            let site_param_at = |i: usize| -> Option<String> {
                let j = i.checked_sub(shift)?;
                site_params.as_ref()?.get(j).cloned()
            };
            let site_is_primitive = |p: &Option<String>| {
                p.as_deref()
                    .is_some_and(|p| wrapper_desc_for_primitive(p).is_some())
            };
            // The index may arrive raw (direct `invokeExact` from bytecode) or
            // boxed (`invokeWithArguments` / an adapter chain that spreads an
            // `Object[]`). A boxed one takes `asType`'s reference -> `int`
            // conversion (wave 25): a `String` index is the `(Number)` cast's
            // `ClassCastException`, a `Long` one `Cannot cast java.lang.Long
            // to java.lang.Integer`, a `null` the NPE -- it read element 0.
            let idx = match full.get(1).copied() {
                Some(Value::Int(i)) => i,
                Some(Value::Long(l)) => l as i32,
                Some(v @ Value::Object(_)) => {
                    let site = site_param_at(1);
                    let unboxed = match v {
                        Value::Object(Some(o)) if site_is_primitive(&site) => {
                            crate::lang_class::unbox_value(ctx, o)
                        }
                        _ => mh_unbox_for_primitive(ctx, DESC_INT, v, site.as_deref())?,
                    };
                    match unboxed {
                        Value::Int(i) => i,
                        Value::Long(l) => l as i32,
                        _ => 0,
                    }
                }
                _ => 0,
            };
            // A setter's value into a PRIMITIVE array is converted with the
            // index, before the access (`asType` runs before the accessor):
            // a `String` value with an out-of-range index is the cast's
            // `ClassCastException`, not the index's. Store it converted.
            let comp = split_descriptor_params(&desc)
                .and_then(|(params, _)| params.last().cloned())
                .unwrap_or_else(|| DESC_OBJECT.to_string());
            let comp_is_primitive =
                matches!(comp.as_str(), "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D");
            let mut value = full.get(2).copied().unwrap_or(Value::Object(None));
            if kind == MH_KIND_ARRAY_SET && comp_is_primitive {
                // `asType`'s reference -> primitive conversion (wave 25): a
                // `String`, a non-widening wrapper or a `null` is refused the
                // way HotSpot refuses it, and a widening wrapper (or a raw
                // `int` into a `long[]`) is widened -- `write_prim_element`'s
                // `Long`/`Float`/`Double` arms store 0 for any other `Value`
                // variant.
                let site = site_param_at(2);
                value = match value {
                    Value::Object(Some(o)) if site_is_primitive(&site) => {
                        widen_primitive_to_descriptor(crate::lang_class::unbox_value(ctx, o), &comp)
                    }
                    _ => mh_unbox_for_primitive(ctx, &comp, value, site.as_deref())?,
                };
            }
            // Round 16 (lane mh): and a setter's REFERENCE value is `asType`'s
            // cast to the handle's own component, before the access: HotSpot
            // 25 refuses `arrayElementSetter(String[].class).invoke((Object[])
            // strs, 0, (Object) 1)` with `ClassCastException: Cannot cast
            // java.lang.Integer to java.lang.String` (`R16MhErrors`
            // `aset-store`), where this arm answered the store's
            // `ArrayStoreException: java.lang.Integer`. Positive mismatches
            // only (`reference_arg_admitted`). An ARRAY component is cast too
            // since wave 60 (lane L4; it was left to the store check, whose
            // `ArrayStoreException` HotSpot does not raise there).
            if kind == MH_KIND_ARRAY_SET
                && (comp.starts_with('L') || comp.starts_with('['))
                && matches!(value, Value::Object(Some(_)))
                && mh_array_setter_component_cast_enabled()
                && reference_arg_admitted(ctx, value, &comp) == Some(false)
            {
                return Err(reference_cast_failure(ctx, value, &comp));
            }
            let len = ctx.array_length(arr);
            if idx < 0 || idx as usize >= len {
                // HotSpot's accessor handle runs an `aaload`/`iaload`/...
                // (`MethodHandleImpl.ArrayAccessor.getElementI` etc.), so the
                // refusal is the array-access wording, "Index 2 out of bounds
                // for length 2" (wave 24; it was a null message,
                // `tools/probes/interp/L4/L4W23ForcedCombinators.java`).
                return Err(RuntimeError::aioobe(idx, len as i32).into());
            }
            if kind == MH_KIND_ARRAY_GET {
                // Raw element value; `auto_box_return` re-boxes it against
                // MH_DESC's return token for the Object-erased invoke shim.
                Ok(Some(ctx.get_array_element(arr, idx as usize)))
            } else {
                // Store: a primitive component's value was converted with the
                // index, above; a reference one is checked against the ARRAY's
                // component.
                let refusal = if comp_is_primitive {
                    None
                } else {
                    cratonvm_native_api::array_store::reject_unstorable(
                        ctx,
                        arr,
                        value,
                        cratonvm_native_api::array_store::StoreRoute::Aastore,
                    )
                };
                if let Some(refusal) = refusal {
                    // HEAP TYPE SAFETY (wave 24). The JDK's accessor stores
                    // through `aastore` (`ArrayAccessor.setElementL`), so a
                    // `String[]` seen through an `Object[]` setter refuses an
                    // `Integer` with `ArrayStoreException: java.lang.Integer`.
                    // This arm stored it: `L4W23ForcedCombinators`'s
                    // `arrayElementSetter store` row printed `[1]`, a
                    // `String[]` holding an `Integer`. The handle's declared
                    // component (`comp`) cannot decide it; the ARRAY's own
                    // component can, through the predicate the `aastore`
                    // opcode uses (it fails open, never a false refusal).
                    return Err(refusal);
                }
                ctx.set_array_element(arr, idx as usize, value);
                Ok(None)
            }
        }
        MH_KIND_FILTER => mh_dispatch_filter(ctx, bound, extra_args, tail),
        MH_KIND_FOLD => mh_dispatch_fold(ctx, bound, extra_args, tail),
        MH_KIND_COLLECT_ARGS => mh_dispatch_collect_args(ctx, bound, extra_args, tail),
        MH_KIND_CATCH => mh_dispatch_catch(ctx, mh, bound, extra_args),
        MH_KIND_RETURN_FILTER => mh_dispatch_return_filter(ctx, bound, extra_args),
        MH_KIND_INVOKER => {
            // `MethodHandles.exactInvoker`/`invoker`/`spreadInvoker`: the
            // target handle is the FIRST incoming argument (not captured at
            // creation time — see MH_KIND_INVOKER's doc comment), the rest
            // are its arguments.
            //
            // `spreadInvoker(type, leadingArgCount)` (the count is stashed in
            // MH_NAME as a decimal string by the factory; absent/unparseable =>
            // plain invoker) is `invoker(type).asSpreader(Object[].class,
            // type.parameterCount() - leadingArgCount)`: the caller passes
            // `leadingArgCount` values and ONE trailing `Object[]`, whose
            // elements become the target's remaining arguments (wave 26, lane
            // L4). This arm used to do the inverse -- PACK the trailing
            // `leadingArgCount` arguments into a new `Object[]` -- so
            // `spreadInvoker(concat.type(), 1).invoke(concat, "a", new
            // Object[] {"b"})` called `"a".concat(Object[] {Object[] {"b"}})`
            // where HotSpot answers `ab`. The array's checks are
            // `asSpreader`'s (`MethodHandleImpl.checkSpreadArgument`).
            //
            // A null target is refused after the spread, as HotSpot's
            // spreader runs before the invoker (wave 28).
            let target = match extra_args.first() {
                Some(Value::Object(t)) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let rest = &extra_args[1..];
            let spread_lead: Option<usize> = mh_read_name(ctx, mh).and_then(|s| s.parse().ok());
            // The target type's arity, from `MH_DESC`, where the factory
            // records the TARGET descriptor since wave 27 (`mh_alloc_invoker`;
            // the handle's `type` is its own, `(MethodHandle, ..)R`). A
            // handle minted without a readable target type keeps the caller's
            // type in `type` and an empty `MH_DESC`, which is read then; an
            // unreadable one leaves the arguments as passed. Read before any
            // GC point.
            let target_arity = spread_lead
                .and_then(|_| {
                    mh_read_desc(ctx, mh)
                        .filter(|d| d.starts_with('('))
                        .or_else(|| mh_type_descriptor(ctx, mh))
                })
                .filter(|d| d.starts_with('('))
                .map(|d| count_descriptor_params(&d));
            let full: Vec<Value> = match (spread_lead, target_arity) {
                (Some(lead), Some(arity)) if rest.len() == lead + 1 && lead <= arity => {
                    let count = arity - lead;
                    let mut v = Vec::with_capacity(arity);
                    v.extend_from_slice(&rest[..lead]);
                    match rest[lead] {
                        Value::Object(Some(arr)) => {
                            let n = ctx.array_length(arr);
                            if n != count {
                                return Err(RuntimeError::IllegalArgumentException {
                                    message: format!("array is not of length {count}"),
                                }
                                .into());
                            }
                            for i in 0..n {
                                v.push(ctx.get_array_element(arr, i));
                            }
                        }
                        _ if count == 0 => {}
                        _ => {
                            return Err(RuntimeError::NullPointerException {
                                message: Some("null array reference".to_string()),
                            }
                            .into());
                        }
                    }
                    v
                }
                _ => rest.to_vec(),
            };
            let exact = mh_is_exact_invoker(&*ctx, mh);
            let Some(target) = target else {
                return Err(invoker_null_target(exact));
            };
            // The invoker's own `asType` casts its leading argument to
            // `MethodHandle` (HotSpot 25.0.3: `invoker(t).invoke((Object)
            // "x", ..)` is `Cannot cast java.lang.String to
            // java.lang.invoke.MethodHandle`); dispatching a non-handle read
            // its fields as a handle's (wave 28). The target type is read
            // first; the check reads class metadata only.
            let t = mh_read_desc(ctx, mh).filter(|d| d.starts_with('('));
            if let Some(e) = leaf_reference_refusal(
                ctx,
                Value::Object(Some(target)),
                "Ljava/lang/invoke/MethodHandle;",
                false,
            ) {
                return Err(e);
            }
            // The TARGET type `t` (wave 28): `exactInvoker(t)` is
            // `target.invokeExact(args)` at the call-site type `t`, and
            // `invoker(t)` / `spreadInvoker(t, n)` is `target.invoke(args)`
            // there -- HotSpot's `Invokers.checkExactType` /
            // `checkGenericType`. This arm used to dispatch the target
            // whatever its type, so an exact invoker ran a target of another
            // type and a plain one skipped the `asType(t)` conversions (an
            // `int` leaf behind an `Object` return came back raw). A handle
            // minted without a readable `t` keeps the bare dispatch.
            match t {
                Some(t) => invoker_call_target(ctx, target, &full, t, exact, tail),
                None => {
                    set_mh_tail(tail);
                    mh_dispatch(ctx, target, &full)
                }
            }
        }
        MH_KIND_SPREAD => {
            // asSpreader(arrayType, count): the trailing argument is an array
            // whose length MUST equal `count`; spread its elements into
            // positional args, then dispatch target.
            //
            // G-MHCOMB1: this arm used to spread WHATEVER length array showed
            // up (or, for a null array, spread nothing) instead of checking it
            // against `count` — measured `c.asSpreaderWrongLen` (a 2-element
            // array into a 3-parameter spreader) fabricating `a|b|null` where
            // HotSpot 25 throws `IllegalArgumentException`, and
            // `c.asSpreaderNullArray` fabricating `null|null|null` where
            // HotSpot throws `NullPointerException`. Both are silent-wrong-
            // answer defects: a caller cannot tell a fabricated argument from
            // a real one without an oracle.
            let wrapper: ObjectRef = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let count = match ctx.get_field(wrapper, 1) {
                Value::Int(c) => c.max(0) as usize,
                _ => 0,
            };
            if count == 0 || extra_args.is_empty() {
                // No trailing array parameter to spread at all (or nothing to
                // spread it from) — dispatch unchanged, exactly as before.
                set_mh_tail(tail);
                return mh_dispatch(ctx, target, extra_args);
            }
            let last = extra_args.len() - 1;
            // HotSpot's two messages (`MethodHandleImpl.checkSpreadArgument`,
            // wave 23; probe `L4/L4W23ForcedCombinators`).
            let arr = match extra_args[last] {
                Value::Object(Some(a)) => a,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("null array reference".to_string()),
                    }
                    .into());
                }
            };
            let n = ctx.array_length(arr);
            if n != count {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!("array is not of length {count}"),
                    }
                    .into(),
                );
            }
            let mut elems: Vec<Value> = Vec::with_capacity(n);
            for i in 0..n {
                elems.push(ctx.get_array_element(arr, i));
            }
            // SBR2 W9 MHFIX: HotSpot's spreader converts each element to the
            // target's parameter type (`asType`'s rule: a cast for a
            // reference, an unbox for a primitive) BEFORE the target runs,
            // so a failed conversion is thrown by the spreader itself, OUTSIDE
            // any `catchException` wrapped around the target
            // (`Proxy.defaultMethodHandle` = `catchException(dmh, ..)` then
            // `asSpreader`: `invokeDefault` relies on it to wrap the
            // ClassCastException / NullPointerException in an
            // IllegalArgumentException). The leaf used to convert INSIDE the
            // catch adapter, which captured the failure.
            let mut target = target;
            let mut prefix: Vec<Value> = extra_args[..last].to_vec();
            if n > 0 {
                if let Some(tdesc) = mh_type_descriptor(ctx, target) {
                    if let Some((params, _)) = split_descriptor_params(&tdesc) {
                        if params.len() >= n {
                            let tail_params = &params[params.len() - n..];
                            let mut sub = String::from("(");
                            for p in tail_params {
                                sub.push_str(p);
                            }
                            sub.push_str(")V");
                            // gen r4w3/rooting: the conversion allocates;
                            // `target` and the leading arguments are used
                            // after it. Pin (lowest first) and re-read.
                            let target_pin = ctx.pin_native_root(target);
                            let (_, prefix_handles) = pin_mh_args(ctx, &prefix);
                            let converted = adapt_leaf_args(ctx, &elems, &sub, None, false);
                            target = ctx.read_native_pin(target_pin, target);
                            prefix = read_pinned_mh_args(ctx, &prefix_handles, &prefix);
                            ctx.unpin_native_roots(target_pin);
                            elems = converted?;
                        }
                    }
                }
            }
            let mut full: Vec<Value> = Vec::with_capacity(last + n);
            full.extend_from_slice(&prefix);
            full.extend_from_slice(&elems);
            set_mh_tail(tail);
            mh_dispatch(ctx, target, &full)
        }
        MH_KIND_LOOP_COUNTED => mh_dispatch_counted_loop(ctx, bound, extra_args),
        MH_KIND_LOOP_WHILE => mh_dispatch_while_loop(ctx, bound, extra_args, false),
        MH_KIND_LOOP_DOWHILE => mh_dispatch_while_loop(ctx, bound, extra_args, true),
        MH_KIND_LOOP_ITERATE => mh_dispatch_iterated_loop(ctx, bound, extra_args),
        MH_KIND_TRY_FINALLY => mh_dispatch_try_finally(ctx, bound, extra_args),
        MH_KIND_TABLE_SWITCH => {
            let Value::Object(Some(wrapper)) = bound else {
                return Ok(Some(Value::Object(None)));
            };
            let selector = match extra_args.first() {
                Some(Value::Int(i)) => Some(*i),
                Some(Value::Object(Some(o))) => match crate::lang_class::unbox_value(ctx, *o) {
                    Value::Int(i) => Some(i),
                    _ => None,
                },
                _ => None,
            };
            // Slot 0 is the case count (one carrier class serves every
            // width, so the object's own width is not the count).
            let cases = match ctx.get_field(wrapper, 0) {
                Value::Int(n) => n,
                _ => 0,
            };
            let slot = match selector {
                Some(i) if i >= 0 && i < cases => i as usize + 2,
                _ => 1,
            };
            let Value::Object(Some(chosen)) = ctx.get_field(wrapper, slot) else {
                return Err(mh_factory_null_argument(None));
            };
            // The chosen case is the switch's tail call.
            set_mh_tail(tail);
            mh_dispatch(ctx, chosen, extra_args)
        }
        _ => {
            // Virtual: first extra_arg is receiver (unless bound). Unbox boxed
            // primitive args against the (receiver-less) descriptor, exactly as
            // the MH_KIND_STATIC arm does via `adapt_invoke_args`. `invokeWithArguments`
            // (and any `Object[]`-spreading caller) hands every primitive param a
            // boxed wrapper; without unboxing here a `(...,int)` target reads each
            // boxed Integer as 0. Canonical victim: Gradle's `LookupClassDefiner`
            // does `ClassLoader.defineClass(name, bytes, 0, bytes.length)` via a
            // bound virtual MH + `invokeWithArguments` — `len` arrived as 0, so the
            // class was defined from 0 bytes ("Could not inject synthetic classes").
            // `adapt_invoke_args` is a no-op for already-raw args (the direct
            // invoke/invokeExact path), so this is safe for every caller.
            //
            // gen r4w3/rooting: read `mh` (unrooted) before the varargs
            // collection / arg adaptation / invoke below, all GC points.
            let boxes = direct_return_boxes(ctx, mh, &desc);
            match bound {
                Value::Object(Some(r)) => {
                    // Bound method handle — receiver was pre-captured
                    let recv_pin = ctx.pin_native_root(r);
                    let collected = collect_trailing_varargs(
                        ctx,
                        &class,
                        &name,
                        &desc,
                        extra_args,
                        generic_collector,
                    );
                    // `asType`'s conversion, fallible (wave 26; see the
                    // STATIC arm).
                    let site = leaf_site_params(entry_call_site.as_deref(), 0, collected.len());
                    let explicit = mh_explicit_scope();
                    let adapted =
                        match adapt_leaf_args(ctx, &collected, &desc, site.as_deref(), explicit) {
                            Ok(adapted) => adapted,
                            Err(e) => {
                                ctx.unpin_native_roots(recv_pin);
                                return Err(e);
                            }
                        };
                    let (adapted_pin_base, adapted_handles) = pin_mh_args(ctx, &adapted);
                    let adapted: Vec<Value> = adapted
                        .iter()
                        .enumerate()
                        .map(|(i, arg)| read_pinned_mh_arg(ctx, adapted_handles[i], *arg))
                        .collect();
                    let r = ctx.read_native_pin(recv_pin, r);
                    let mut full_args = Vec::with_capacity(1 + adapted.len());
                    full_args.push(Value::Object(Some(r)));
                    full_args.extend_from_slice(&adapted);
                    // Transparent to a yield in a reference-returning tail
                    // call (`MH_TAIL`, wave 26).
                    let transparent = transparent_leaf_call(&*ctx, tail, &desc, Some(r));
                    let result = ctx.invoke_virtual_declared(&class, r, &name, &desc, &adapted);
                    transparent_leaf_call_done(ctx, transparent, &result);
                    // Round 15 wave 1 (lane mh): the neutral hook answers the
                    // receiver where the call left it.
                    if mh_neutral_hook_current_receiver_enabled() {
                        full_args[0] = Value::Object(Some(ctx.read_native_pin(recv_pin, r)));
                    }
                    if adapted_pin_base != usize::MAX {
                        ctx.unpin_native_roots(adapted_pin_base);
                    }
                    ctx.unpin_native_roots(recv_pin);
                    box_direct_primitive_return_pre(
                        ctx,
                        boxes,
                        neutralize_missing_serialization_hook(result, &full_args),
                        &desc,
                    )
                }
                _ => match extra_args.first() {
                    Some(Value::Object(Some(receiver))) => {
                        let receiver = *receiver;
                        // Wave 118 (`CRATONVM_MH_VIRTUAL_BY_OWNER_ID`): the
                        // owner id, read while `mh` is current.
                        let virtual_owner = if kind == MH_KIND_VIRTUAL {
                            mh_instance_owner(&*ctx, mh)
                        } else {
                            None
                        };
                        // The unbound receiver is cast to `MH_CLASS` (wave 27;
                        // `checkReceiver` on HotSpot): `invoke_virtual_declared`
                        // dispatches by name, so a receiver of another class
                        // ran ITS same-named method or failed with
                        // `NoSuchMethodError`. Only a `VIRTUAL` handle: this
                        // arm is also the fallback for an unknown kind.
                        if kind == MH_KIND_VIRTUAL {
                            // JIT round 19 wave 1 (lane mhiface;
                            // `CRATONVM_MH_INTERFACE_RECEIVER_CHECK`): an
                            // interface member's `Interface.checkReceiver`.
                            if let Some(e) =
                                mh_interface_receiver_refusal_named(ctx, receiver, &class)
                            {
                                return Err(e);
                            }
                            if let Some(e) = leaf_receiver_refusal_owned(
                                ctx,
                                mh,
                                receiver,
                                &class,
                                mh_explicit_scope(),
                            ) {
                                return Err(e);
                            }
                            // A `VarHandle` access mode's invoker
                            // (`varHandleInvoker` / `varHandleExactInvoker`,
                            // a direct handle's mode invoker): the access is a
                            // call site of type `desc` on the receiver.
                            if class == VAR_HANDLE_CLASS {
                                if let Some(e) =
                                    vh_invoker_call_refusal(ctx, mh, receiver, &name, &desc)
                                {
                                    return Err(e);
                                }
                            }
                        }
                        let recv_pin = ctx.pin_native_root(receiver);
                        let collected = collect_trailing_varargs(
                            ctx,
                            &class,
                            &name,
                            &desc,
                            &extra_args[1..],
                            generic_collector,
                        );
                        // `asType`'s conversion, fallible (wave 26; see the
                        // STATIC arm).
                        let site = leaf_site_params(entry_call_site.as_deref(), 0, collected.len());
                        let explicit = mh_explicit_scope();
                        let adapted = match adapt_leaf_args(
                            ctx,
                            &collected,
                            &desc,
                            site.as_deref(),
                            explicit,
                        ) {
                            Ok(adapted) => adapted,
                            Err(e) => {
                                ctx.unpin_native_roots(recv_pin);
                                return Err(e);
                            }
                        };
                        let (adapted_pin_base, adapted_handles) = pin_mh_args(ctx, &adapted);
                        let adapted: Vec<Value> = adapted
                            .iter()
                            .enumerate()
                            .map(|(i, arg)| read_pinned_mh_arg(ctx, adapted_handles[i], *arg))
                            .collect();
                        let receiver = ctx.read_native_pin(recv_pin, receiver);
                        if crate::nbflags().dbg_mh_dispatch {
                            let last_desc = match adapted.last() {
                                Some(Value::Object(Some(o))) if ctx.object_is_array(*o) => {
                                    format!("array[len={}]", ctx.array_length(*o))
                                }
                                Some(Value::Object(Some(_))) => "obj".to_string(),
                                Some(Value::Object(None)) => "null".to_string(),
                                Some(other) => format!("{other:?}"),
                                None => "<none>".to_string(),
                            };
                            eprintln!(
                                "[MH_VIRTUAL_ADAPTED] class={class} name={name} desc={desc:?} collected_len={} adapted_len={} last={last_desc}",
                                collected.len(),
                                adapted.len()
                            );
                        }
                        let mut full_args = Vec::with_capacity(1 + adapted.len());
                        full_args.push(Value::Object(Some(receiver)));
                        full_args.extend_from_slice(&adapted);
                        // Transparent to a yield in a reference-returning
                        // tail call (`MH_TAIL`, wave 26).
                        let transparent = transparent_leaf_call(&*ctx, tail, &desc, Some(receiver));
                        let result = mh_invoke_virtual_by_owner(
                            ctx,
                            virtual_owner,
                            &class,
                            receiver,
                            &name,
                            &desc,
                            &adapted,
                        );
                        transparent_leaf_call_done(ctx, transparent, &result);
                        // Round 15 wave 1 (lane mh): as in the bound arm.
                        if mh_neutral_hook_current_receiver_enabled() {
                            full_args[0] =
                                Value::Object(Some(ctx.read_native_pin(recv_pin, receiver)));
                        }
                        if adapted_pin_base != usize::MAX {
                            ctx.unpin_native_roots(adapted_pin_base);
                        }
                        ctx.unpin_native_roots(recv_pin);
                        box_direct_primitive_return_pre(
                            ctx,
                            boxes,
                            neutralize_missing_serialization_hook(result, &full_args),
                            &desc,
                        )
                    }
                    // A null receiver of a VIRTUAL handle is HotSpot's
                    // message-less NPE (`DirectMethodHandle.checkReceiver`'s
                    // `Objects.requireNonNull`), wherever the call came from
                    // (wave 27): an adapter's or `invokeWithArguments`'
                    // null answered `null` here -- or the primitive zero --
                    // with no exception.
                    Some(Value::Object(None)) if kind == MH_KIND_VIRTUAL => {
                        Err(vh_invoker_null_handle(&*ctx, mh).unwrap_or_else(|| {
                            RuntimeError::NullPointerException { message: None }.into()
                        }))
                    }
                    _ => Ok(Some(Value::Object(None))),
                },
            }
        }
    }
}

