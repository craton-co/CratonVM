// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5, lane MHASTYPE: the compiled constant bind of a REAL `asType`
//! result (MH6-1 / MH7-4 remainder; page
//! `docs/known-issues/jit/j27-mh-real-astype-converting-bind-proposal-20261028.md`).
//!
//! Under the default-ON `jdk-invoke` alignment row every handle is a real JDK
//! handle, so the carrier's converting bind
//! ([`mh_compiled_bind_converting_target`]) never sees one: a compiled
//! `invokeExact` of `twiceJ.asType((Object)Object)` walked the whole
//! signature-polymorphic link chain on every call.
//!
//! **Provenance, not shape.** `MethodHandle.asType(T)` answers
//! `setAsTypeCache(asTypeUncached(T))`, and `asTypeUncached` of a plain
//! `DirectMethodHandle` is `MethodHandleImpl.makePairwiseConvert(this, T,
//! strict = true)`, whose species keeps the direct handle as `argL0` (its
//! `rebind()` reinvoker). So a `BoundMethodHandle` species `h` whose `argL0` is
//! a plain `DirectMethodHandle` `d` with `d.asTypeCache == h` (or the referent
//! of `d.asTypeSoftCache`) IS `d.asType(h.type())`: its behaviour is the
//! documented `asType` conversion of `d.type()` into `h.type()`, and no
//! private form or transform-key layout has to be recognised. Every field the
//! answer depends on (`argL0`, `type`, the member) is `final`; the cache field
//! is read only to prove where `h` came from, once, on the cold fill.
//!
//! **The conversions served** (strict `asType`, `ValueConversions`): per
//! position, the same token, or any reference into `Object` (a null
//! conversion, no filter); a reference into `long` / `int`, unboxed from an
//! exact wrapper the JDK's unboxer accepts without raising
//! (`unboxExact` when the source type IS the wrapper, else `unboxWiden`:
//! `Wrapper.isConvertibleFrom` -- Long, Integer, Short, Byte, Character into
//! `long`; Integer, Short, Byte, Character into `int`); and an `int` / `long`
//! return boxed through `valueOf` into its wrapper, `Number`, `Comparable`,
//! `Serializable` or `Object`. Every other value -- `null` (the JDK's NPE), a
//! wrapper the unboxer refuses (its `ClassCastException`), anything else --
//! is sent by the helper to the door, which runs the JDK's own adapter; every
//! other type is refused here. The unboxers are pure, so declining after
//! reading some argument words skips no JDK side effect.

use super::*;

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING` -- perf, **OPT-IN** (`=1`
/// on; j27 wave 5, lane MHASTYPE): the dispatch helper's cold fill also asks
/// [`mh_real_converting_compiled_bind`] about a REAL handle every other
/// admission refused. Opt-in because it was written without a build: it reads
/// the JDK-private `MethodHandle.asTypeCache` / `asTypeSoftCache` fields and
/// boxes a `long` return through the compiled `Long.valueOf` helper, neither
/// of which was run here. Read on the cold fill only.
pub fn mh_constant_bind_real_converting_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING")
}

/// The wrapper classes a real converting bind unboxes, in the bit order of
/// [`MhRealConversion::unbox`] (bit `k` = `MH_REAL_UNBOX_WRAPPERS[k]`).
pub const MH_REAL_UNBOX_WRAPPERS: [&str; 5] = [
    "java/lang/Long",
    "java/lang/Integer",
    "java/lang/Short",
    "java/lang/Byte",
    "java/lang/Character",
];
/// Index of `java/lang/Long` in [`MH_REAL_UNBOX_WRAPPERS`].
pub const MH_REAL_UNBOX_LONG: usize = 0;
/// Index of `java/lang/Integer`.
pub const MH_REAL_UNBOX_INT: usize = 1;
/// Index of `java/lang/Short`.
pub const MH_REAL_UNBOX_SHORT: usize = 2;
/// Index of `java/lang/Byte`.
pub const MH_REAL_UNBOX_BYTE: usize = 3;
/// Index of `java/lang/Character`.
pub const MH_REAL_UNBOX_CHAR: usize = 4;

/// The wrappers `Wrapper.INT.isConvertibleFrom` accepts (no `Long`).
const MH_REAL_INT_SOURCES: u8 = (1 << MH_REAL_UNBOX_INT)
    | (1 << MH_REAL_UNBOX_SHORT)
    | (1 << MH_REAL_UNBOX_BYTE)
    | (1 << MH_REAL_UNBOX_CHAR);
/// The wrappers `Wrapper.LONG.isConvertibleFrom` accepts.
const MH_REAL_LONG_SOURCES: u8 = MH_REAL_INT_SOURCES | (1 << MH_REAL_UNBOX_LONG);

/// How a real converting bind hands back the member's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MhRealBox {
    /// As the member returns it (the same type, or a null conversion).
    Keep,
    /// An `int` boxed through `Integer.valueOf`.
    Int,
    /// A `long` boxed through `Long.valueOf`.
    Long,
}

/// What a real converting bound site does around its member's call (j27
/// wave 5, lane MHASTYPE; [`mh_real_converting_compiled_bind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhRealConversion {
    /// Per member argument word (an unbound virtual member's receiver is word
    /// 0): `0` passes the caller's word as is; otherwise the bit mask, over
    /// [`MH_REAL_UNBOX_WRAPPERS`], of the exact wrapper classes whose value
    /// the word is replaced by (widened to the member's `int` / `long`).
    pub unbox: [u8; MH_BIND_CONVERSION_MAX_WORDS],
    pub box_return: MhRealBox,
    /// The class id of each [`MH_REAL_UNBOX_WRAPPERS`] entry, `0` when that
    /// class is not loaded (its bit is then clear in every `unbox` mask).
    pub wrapper_class_ids: [u32; 5],
}

/// A reference type token (`L..;` or `[..`)?
fn mh_real_is_reference(token: &str) -> bool {
    token.starts_with('L') || token.starts_with('[')
}

/// One argument position of strict `asType` from the site's token `from`
/// into the member's `to`: `Some(0)` -- no filter (the same token, or a
/// reference into `Object`); `Some(mask)` -- unbox from the wrappers in
/// `mask`; `None` -- a conversion this bind does not serve (a box, a cast, a
/// primitive conversion, an unbox into anything but `int` / `long`). Pure.
pub(crate) fn mh_real_arg_unbox(from: &str, to: &str) -> Option<u8> {
    if from == to || (to == "Ljava/lang/Object;" && mh_real_is_reference(from)) {
        return Some(0);
    }
    if !mh_real_is_reference(from) {
        return None;
    }
    // `valueConversion`: the wrapper's own type takes `unboxExact` (only that
    // class can arrive), any other reference `unboxWiden`.
    match (to, from) {
        ("J", "Ljava/lang/Long;") => Some(1 << MH_REAL_UNBOX_LONG),
        ("J", _) => Some(MH_REAL_LONG_SOURCES),
        ("I", "Ljava/lang/Integer;") => Some(1 << MH_REAL_UNBOX_INT),
        ("I", _) => Some(MH_REAL_INT_SOURCES),
        _ => None,
    }
}

/// The return of strict `asType` from the member's `member_ret` into the
/// site's `site_ret` (`valueConversion(member_ret, site_ret)`): `Keep` for
/// the same token or a reference into `Object`; a box for an `int` / `long`
/// into a type its wrapper is assignable to; `None` otherwise (a `void`
/// drop, a cast, any other box). Pure.
pub(crate) fn mh_real_return_box(site_ret: &str, member_ret: &str) -> Option<MhRealBox> {
    if site_ret == member_ret
        || (site_ret == "Ljava/lang/Object;" && mh_real_is_reference(member_ret))
    {
        return Some(MhRealBox::Keep);
    }
    let (wrapper, boxed) = match member_ret {
        "I" => ("Ljava/lang/Integer;", MhRealBox::Int),
        "J" => ("Ljava/lang/Long;", MhRealBox::Long),
        _ => return None,
    };
    let assignable = site_ret == wrapper
        || matches!(
            site_ret,
            "Ljava/lang/Object;"
                | "Ljava/lang/Number;"
                | "Ljava/lang/Comparable;"
                | "Ljava/io/Serializable;"
        );
    assignable.then_some(boxed)
}

/// The conversions of strict `asType` from the call's type `site` into the
/// member's own type `member`: the per-word unbox masks and the return box,
/// or `None` when any position is outside [`mh_real_arg_unbox`] /
/// [`mh_real_return_box`]. Pure.
pub(crate) fn mh_real_conversion_plan(
    site: &str,
    member: &str,
) -> Option<([u8; MH_BIND_CONVERSION_MAX_WORDS], MhRealBox)> {
    let (from_params, from_ret) = split_descriptor_params(site)?;
    let (to_params, to_ret) = split_descriptor_params(member)?;
    if from_params.len() != to_params.len() || to_params.len() > MH_BIND_CONVERSION_MAX_WORDS {
        return None;
    }
    let mut unbox = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
    for (i, (f, t)) in from_params.iter().zip(&to_params).enumerate() {
        unbox[i] = mh_real_arg_unbox(f, t)?;
    }
    Some((unbox, mh_real_return_box(&from_ret, &to_ret)?))
}

/// The direct handle `handle` is the `asType` result of, or `None`: `handle`
/// is a `BoundMethodHandle` species whose `argL0` is a plain
/// `java/lang/invoke/DirectMethodHandle` that names `handle` as its
/// `asTypeCache` (or `asTypeSoftCache` referent). See the module comment for
/// why that proves `handle == argL0.asType(handle.type())`. A layout this
/// does not read answers `None` (the chain, as before). Pure reads.
fn mh_real_as_type_source(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    let species = ctx.class_name_arc_of_id(ctx.class_id_of_object(handle))?;
    if !species.starts_with("java/lang/invoke/BoundMethodHandle$Species_") {
        return None;
    }
    let target = mh_real_obj(ctx.get_field_by_name(handle, "argL0"))?;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(target))
        .as_deref()
        != Some("java/lang/invoke/DirectMethodHandle")
    {
        return None;
    }
    if mh_real_obj(ctx.get_field_by_name(target, "asTypeCache")) == Some(handle) {
        return Some(target);
    }
    let soft = mh_real_obj(ctx.get_field_by_name(target, "asTypeSoftCache"))?;
    (mh_real_obj(ctx.get_field_by_name(soft, "referent")) == Some(handle)).then_some(target)
}

/// The class id of every [`MH_REAL_UNBOX_WRAPPERS`] entry, and `unbox` with
/// the bit of each wrapper that is not loaded cleared (no value of it can
/// arrive, and the helper must never match class id `0`). Pure reads.
fn mh_real_resolve_wrappers(
    ctx: &dyn NativeContext,
    mut unbox: [u8; MH_BIND_CONVERSION_MAX_WORDS],
) -> ([u8; MH_BIND_CONVERSION_MAX_WORDS], [u32; 5]) {
    let mut ids = [0u32; 5];
    for (k, name) in MH_REAL_UNBOX_WRAPPERS.iter().enumerate() {
        match ctx.class_id_by_name(name) {
            Some(id) if id.as_u32() != 0 => ids[k] = id.as_u32(),
            _ => unbox.iter_mut().for_each(|m| *m &= !(1u8 << k)),
        }
    }
    (unbox, ids)
}

/// Does every position the plan unboxes still unbox after
/// [`mh_real_resolve_wrappers`] cleared the bits of unloaded wrappers (j27
/// wave 8, lane W8CLI)? A mask of `0` means "pass the word as is", so a
/// cleared unbox position would hand the member the REFERENCE word as its
/// `int` / `long`: refuse the handle instead (the door then runs the JDK's
/// adapter). Pure.
pub(crate) fn mh_real_unbox_positions_survive(
    planned: &[u8; MH_BIND_CONVERSION_MAX_WORDS],
    resolved: &[u8; MH_BIND_CONVERSION_MAX_WORDS],
) -> bool {
    planned.iter().zip(resolved).all(|(p, r)| *p == 0 || *r != 0)
}

/// May a compiled `invokeExact` / `invoke` site of descriptor `site` call
/// `handle`'s member directly, with the [`MhRealConversion`] around the call
/// (j27 wave 5, lane MHASTYPE; the module comment)? `handle` must be a REAL
/// `asType` result ([`mh_real_as_type_source`]) called at its own type (any
/// other `invokeExact` type is the door's `WrongMethodTypeException`), of a
/// plain `REF_invokeStatic` / `REF_invokeVirtual` direct handle at its
/// member's own type; the member questions are the exact bind's
/// ([`mh_compiled_bind_member`]). Asked only after every other admission
/// refused. The answer is never `bound`, and no compile door may record it
/// (the compile doors would call the member without the conversions).
///
/// Pure reads; no allocation, no GC point.
pub fn mh_real_converting_compiled_bind(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> Option<(MhCompiledBind, MhRealConversion)> {
    if crate::nbflags().dbg_mh_dispatch || !mh_is_real_jdk_handle(&*ctx, handle) {
        return None;
    }
    let target = mh_real_as_type_source(&*ctx, handle)?;
    if real_handle_record(ctx, handle)?.declared != site {
        return None;
    }
    let record = real_handle_record(ctx, target)?;
    let direct = record.direct.as_ref()?;
    let (kind, own_type) = mh_real_direct_kind(direct)?;
    if record.declared != own_type {
        return None;
    }
    let (planned, box_return) = mh_real_conversion_plan(site, &own_type)?;
    let (unbox, wrapper_class_ids) = mh_real_resolve_wrappers(&*ctx, planned);
    if !mh_real_unbox_positions_survive(&planned, &unbox) {
        return None;
    }
    let lane = mh_real_lane_record(direct, own_type);
    let bind = mh_compiled_bind_member(ctx, kind, Value::Object(None), None, &lane, direct.owner)?;
    // No bound receiver went in, so `bind.bound` is `false`.
    Some((
        bind,
        MhRealConversion {
            unbox,
            box_return,
            wrapper_class_ids,
        },
    ))
}

#[cfg(test)]
mod j27_mh_real_astype_plan_tests {
    use super::*;

    const LONG: u8 = 1 << MH_REAL_UNBOX_LONG;
    const INT: u8 = 1 << MH_REAL_UNBOX_INT;
    const SHORT: u8 = 1 << MH_REAL_UNBOX_SHORT;
    const BYTE: u8 = 1 << MH_REAL_UNBOX_BYTE;
    const CHAR: u8 = 1 << MH_REAL_UNBOX_CHAR;

    /// `twiceJ(long)long.asType((Object)Object)` (`J27MhRows` `conv-long-*`):
    /// `unboxWiden(LONG)` accepts Long, Integer, Short, Byte and Character
    /// (Boolean, Float, Double and null are the door's CCE / NPE), and the
    /// result is boxed through `Long.valueOf`.
    #[test]
    fn j27_object_into_long_widens_every_convertible_wrapper() {
        let (unbox, ret) = mh_real_conversion_plan(
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "(J)J",
        )
        .expect("served");
        assert_eq!(unbox[0], LONG | INT | SHORT | BYTE | CHAR);
        assert!(unbox[1..].iter().all(|m| *m == 0));
        assert_eq!(ret, MhRealBox::Long);
    }

    /// `int` takes no `Long` (`Wrapper.INT.isConvertibleFrom(LONG)` is false:
    /// the JDK raises `Cannot cast java.lang.Long to java.lang.Integer`), and
    /// the wrapper's own type is `unboxExact`: that class only.
    #[test]
    fn j27_int_refuses_long_and_the_exact_wrapper_takes_only_itself() {
        assert_eq!(
            mh_real_arg_unbox("Ljava/lang/Number;", "I"),
            Some(INT | SHORT | BYTE | CHAR)
        );
        assert_eq!(mh_real_arg_unbox("Ljava/lang/Integer;", "I"), Some(INT));
        assert_eq!(mh_real_arg_unbox("Ljava/lang/Long;", "J"), Some(LONG));
        assert_eq!(mh_real_arg_unbox("Ljava/lang/Integer;", "J"), Some(LONG_SOURCES));
    }

    const LONG_SOURCES: u8 = LONG | INT | SHORT | BYTE | CHAR;

    /// Positions this bind does not serve refuse the whole handle: an unbox
    /// into `short` / `char` / `boolean` / `double`, a box of an argument, a
    /// primitive conversion, a reference cast.
    #[test]
    fn j27_unserved_positions_refuse() {
        for to in ["S", "C", "Z", "B", "F", "D"] {
            assert_eq!(mh_real_arg_unbox("Ljava/lang/Object;", to), None, "{to}");
        }
        assert_eq!(mh_real_arg_unbox("I", "Ljava/lang/Object;"), None, "argument box");
        assert_eq!(mh_real_arg_unbox("I", "J"), None, "primitive widening");
        assert_eq!(mh_real_arg_unbox("Ljava/lang/Object;", "Ljava/lang/String;"), None);
        // Null conversions pass the word through.
        assert_eq!(mh_real_arg_unbox("Ljava/lang/String;", "Ljava/lang/Object;"), Some(0));
        assert_eq!(mh_real_arg_unbox("[I", "Ljava/lang/Object;"), Some(0));
        assert_eq!(mh_real_arg_unbox("Lp/Acc;", "Lp/Acc;"), Some(0));
        assert!(mh_real_conversion_plan("(Ljava/lang/Object;)Ljava/lang/Object;", "(S)S").is_none());
        assert!(mh_real_conversion_plan("(Ljava/lang/Object;)V", "(J)J").is_none(), "void drop");
        assert!(mh_real_conversion_plan("(Ljava/lang/Object;)J", "(JJ)J").is_none(), "arity");
    }

    /// The return box: into the wrapper or a supertype of it; never into
    /// another wrapper (`int` into `Long` is a CCE in the JDK's adapter).
    #[test]
    fn j27_return_boxes_only_into_assignable_types() {
        assert_eq!(mh_real_return_box("Ljava/lang/Integer;", "I"), Some(MhRealBox::Int));
        assert_eq!(mh_real_return_box("Ljava/lang/Number;", "J"), Some(MhRealBox::Long));
        assert_eq!(mh_real_return_box("Ljava/lang/Comparable;", "I"), Some(MhRealBox::Int));
        assert_eq!(mh_real_return_box("Ljava/lang/Long;", "I"), None);
        assert_eq!(mh_real_return_box("Ljava/lang/Object;", "S"), None, "short box");
        assert_eq!(mh_real_return_box("J", "J"), Some(MhRealBox::Keep));
        assert_eq!(mh_real_return_box("Ljava/lang/Object;", "Ljava/lang/String;"), Some(MhRealBox::Keep));
        assert_eq!(mh_real_return_box("Ljava/lang/String;", "Ljava/lang/Object;"), None, "cast");
    }

    /// An unbound virtual member's receiver is word 0 and keeps its token;
    /// the `int` parameter after it unboxes (`J27MhRows` `conv-virtual-*`).
    #[test]
    fn j27_virtual_receiver_passes_and_the_next_word_unboxes() {
        let (unbox, ret) = mh_real_conversion_plan(
            "(Lp/Acc;Ljava/lang/Object;)Ljava/lang/Object;",
            "(Lp/Acc;I)I",
        )
        .expect("served");
        assert_eq!(&unbox[..2], &[0, INT | SHORT | BYTE | CHAR]);
        assert_eq!(ret, MhRealBox::Int);
    }

    /// j27 wave 8 (lane W8CLI): a position whose every wrapper is unloaded
    /// would read as "pass the word" (mask 0) and hand the member a reference
    /// word as its `long`: the handle is refused instead. Positions the plan
    /// never unboxed, and positions that keep a bit, are fine.
    #[test]
    fn j27_w8cli_an_unbox_position_emptied_by_unloaded_wrappers_refuses() {
        let (planned, _) = mh_real_conversion_plan(
            "(Ljava/lang/Long;Lp/Acc;)J",
            "(JLp/Acc;)J",
        )
        .expect("served");
        assert_eq!(&planned[..2], &[LONG, 0]);
        let mut emptied = planned;
        emptied[0] &= !LONG;
        assert!(!mh_real_unbox_positions_survive(&planned, &emptied));
        assert!(mh_real_unbox_positions_survive(&planned, &planned));
        let mut narrowed = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
        narrowed[0] = LONG_SOURCES & !CHAR;
        let mut wide = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
        wide[0] = LONG_SOURCES;
        assert!(mh_real_unbox_positions_survive(&wide, &narrowed));
    }

    /// The switch is opt-in: unset is off, `1` on.
    #[test]
    fn j27_real_converting_switch_is_opt_in() {
        const KEY: &str = "CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING";
        let unset = cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            mh_constant_bind_real_converting_enabled()
        });
        assert!(!unset);
        let on = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            mh_constant_bind_real_converting_enabled()
        });
        assert!(on);
    }
}
