// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5, lane MHASTYPE: the compiled constant bind of a REAL `asType`
//! result (MH6-1 / MH7-4 remainder; page
//! `docs/feature-designs/native-dispatch-architecture.md`).
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

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING` -- perf, default ON, `=0`
/// off (j27 wave 5, lane MHASTYPE; flipped on 2026-11-01): the dispatch
/// helper's cold fill also asks [`mh_real_converting_compiled_bind`] about a
/// REAL handle every other admission refused. It reads the JDK-private
/// `MethodHandle.asTypeCache` / `asTypeSoftCache` fields and boxes a `long`
/// return through the compiled `Long.valueOf` helper. Evidence for the flip:
/// `J27MhRows` facts equal HotSpot 25 in `--real-jdk` and `--compatible`, and
/// its `conv-*` rows fall from 9-18 us/op to 0.6-0.8 us/op (medians of 5
/// interleaved reps). Read on the cold fill only.
pub fn mh_constant_bind_real_converting_enabled() -> bool {
    true
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
    /// Per member argument word: `0` when no reference cast applies; otherwise
    /// the target `ClassId` (`Class.cast`) the caller's reference must satisfy
    /// (null passes).
    pub cast: [u32; MH_BIND_CONVERSION_MAX_WORDS],
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
pub fn mh_real_arg_unbox(from: &str, to: &str) -> Option<u8> {
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
pub fn mh_real_return_box(site_ret: &str, member_ret: &str) -> Option<MhRealBox> {
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
pub fn mh_real_arg_cast<'a>(from: &str, to: &'a str) -> Option<&'a str> {
    if !mh_real_is_reference(from)
        || !mh_real_is_reference(to)
        || from == to
        || to == "Ljava/lang/Object;"
    {
        return None;
    }
    if to.starts_with('L') && to.ends_with(';') && to.len() > 2 {
        Some(&to[1..to.len() - 1])
    } else if to.starts_with('[') {
        Some(to)
    } else {
        None
    }
}

pub fn mh_real_conversion_plan(
    site: &str,
    member: &str,
) -> Option<([u8; MH_BIND_CONVERSION_MAX_WORDS], MhRealBox)> {
    let (unbox, casts, ret) = mh_real_conversion_plan_with_casts(site, member)?;
    if casts.iter().any(|c| c.is_some()) {
        return None;
    }
    Some((unbox, ret))
}

pub fn mh_real_conversion_plan_with_casts(
    site: &str,
    member: &str,
) -> Option<(
    [u8; MH_BIND_CONVERSION_MAX_WORDS],
    [Option<String>; MH_BIND_CONVERSION_MAX_WORDS],
    MhRealBox,
)> {
    let (from_params, from_ret) = split_descriptor_params(site)?;
    let (to_params, to_ret) = split_descriptor_params(member)?;
    if from_params.len() != to_params.len() || to_params.len() > MH_BIND_CONVERSION_MAX_WORDS {
        return None;
    }
    let mut unbox = [0u8; MH_BIND_CONVERSION_MAX_WORDS];
    let mut casts: [Option<String>; MH_BIND_CONVERSION_MAX_WORDS] = Default::default();
    for (i, (f, t)) in from_params.iter().zip(&to_params).enumerate() {
        if let Some(u) = mh_real_arg_unbox(f, t) {
            unbox[i] = u;
        } else if let Some(c) = mh_real_arg_cast(f, t) {
            casts[i] = Some(c.to_string());
        } else {
            return None;
        }
    }
    Some((unbox, casts, mh_real_return_box(&from_ret, &to_ret)?))
}

fn mh_real_is_direct_source_class(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("java/lang/invoke/DirectMethodHandle" | "java/lang/invoke/DirectMethodHandle$Interface")
    )
}

fn mh_real_direct_target_candidate(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    let species = ctx.class_name_arc_of_id(ctx.class_id_of_object(handle))?;
    if !species.starts_with("java/lang/invoke/BoundMethodHandle$Species_") {
        return None;
    }
    let target = mh_real_obj(ctx.get_field_by_name(handle, "argL0"))?;
    mh_real_is_direct_source_class(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(target))
            .as_deref(),
    )
    .then_some(target)
}

fn mh_real_as_type_cache_matches(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    handle: ObjectRef,
) -> bool {
    if mh_real_obj(ctx.get_field_by_name(target, "asTypeCache")) == Some(handle) {
        return true;
    }
    mh_real_obj(ctx.get_field_by_name(target, "asTypeSoftCache"))
        .and_then(|soft| mh_real_obj(ctx.get_field_by_name(soft, "referent")))
        == Some(handle)
}

const LFE_KIND_FILTER_ARG: u8 = 5;
const LFE_KIND_FILTER_RETURN: u8 = 6;
const LFE_SCAN_MAX: usize = 64;

fn mh_real_uncustomize_form(ctx: &dyn NativeContext, form: ObjectRef) -> Option<ObjectRef> {
    match mh_real_obj(ctx.get_field_by_name(form, "customized")) {
        None => Some(form),
        Some(_) => mh_real_obj(ctx.get_field_by_name(form, "transformCache")),
    }
}

fn mh_real_transform_children(
    ctx: &dyn NativeContext,
    form: ObjectRef,
    out: &mut Vec<(u8, usize, ObjectRef)>,
) {
    let Some(cache) = mh_real_obj(ctx.get_field_by_name(form, "transformCache")) else {
        return;
    };
    let mut push_transform = |t: ObjectRef| {
        if mh_real_obj(ctx.get_field_by_name(t, "fullBytes")).is_some() {
            return;
        }
        let Value::Long(packed) = ctx.get_field_by_name(t, "packedBytes") else {
            return;
        };
        let kind = (packed & 0x0F) as u8;
        if !matches!(kind, LFE_KIND_FILTER_ARG | LFE_KIND_FILTER_RETURN) {
            return;
        }
        let pos = ((packed >> 4) & 0xFF) as usize;
        if let Some(next) = mh_real_obj(ctx.get_field_by_name(t, "referent")) {
            out.push((kind, pos, next));
        }
    };
    if ctx.object_is_array(cache) {
        let n = ctx.array_length(cache).min(LFE_SCAN_MAX);
        for i in 0..n {
            match mh_real_obj(ctx.get_array_element(cache, i)) {
                Some(t) => push_transform(t),
                None => break,
            }
        }
    } else if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(cache))
        .is_some_and(|n| &*n == "java/lang/invoke/LambdaFormEditor$Transform")
    {
        push_transform(cache);
    }
}

fn mh_real_dmh_member_matches(
    ctx: &mut dyn NativeContext,
    dmh: ObjectRef,
    class: &str,
    name: &str,
    desc_prefix: Option<&str>,
) -> bool {
    let Some(cls) = ctx.class_name_arc_of_id(ctx.class_id_of_object(dmh)) else {
        return false;
    };
    if &*cls != "java/lang/invoke/DirectMethodHandle" {
        return false;
    }
    let Some(rec) = real_handle_record(ctx, dmh) else {
        return false;
    };
    let Some(d) = rec.direct.as_ref() else {
        return false;
    };
    d.class == class && d.name == name && desc_prefix.is_none_or(|p| d.desc.starts_with(p))
}

fn mh_real_step_filter_matches(
    ctx: &mut dyn NativeContext,
    filter: ObjectRef,
    kind: u8,
    pos: usize,
    from_params: &[String],
    unbox: &[u8; MH_BIND_CONVERSION_MAX_WORDS],
    cast_ids: &[u32; MH_BIND_CONVERSION_MAX_WORDS],
    box_return: MhRealBox,
    seen_args: &mut u32,
    seen_ret: &mut bool,
) -> bool {
    match kind {
        LFE_KIND_FILTER_ARG => {
            let Some(p) = pos.checked_sub(1) else {
                return false;
            };
            if p >= from_params.len() || p >= 32 || (*seen_args & (1u32 << p)) != 0 {
                return false;
            }
            *seen_args |= 1u32 << p;
            if unbox[p] != 0 {
                let (fn_name, exact_tok) = if (unbox[p] & !(1 << MH_REAL_UNBOX_LONG)) == 0 {
                    ("unboxLong", "Ljava/lang/Long;")
                } else {
                    ("unboxInteger", "Ljava/lang/Integer;")
                };
                if from_params[p] == exact_tok {
                    return mh_real_dmh_member_matches(
                        ctx,
                        filter,
                        "sun/invoke/util/ValueConversions",
                        fn_name,
                        None,
                    );
                }
                let Some(arg0) = mh_real_obj(ctx.get_field_by_name(filter, "argL0")) else {
                    return false;
                };
                return mh_real_dmh_member_matches(
                    ctx,
                    arg0,
                    "sun/invoke/util/ValueConversions",
                    fn_name,
                    Some("(Ljava/lang/Object;Z)"),
                );
            }
            if cast_ids[p] != 0 {
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(filter))
                    .as_deref()
                    != Some("java/lang/invoke/BoundMethodHandle$Species_LL")
                {
                    return false;
                }
                let Some(arg0) = mh_real_obj(ctx.get_field_by_name(filter, "argL0")) else {
                    return false;
                };
                let Some(arg1) = mh_real_obj(ctx.get_field_by_name(filter, "argL1")) else {
                    return false;
                };
                return mh_real_dmh_member_matches(
                    ctx,
                    arg0,
                    "java/lang/Class",
                    "cast",
                    Some("(Ljava/lang/Object;)Ljava/lang/Object;"),
                ) && ctx.class_id_from_mirror(arg1).map(|c| c.as_u32()) == Some(cast_ids[p]);
            }
            false
        }
        LFE_KIND_FILTER_RETURN => {
            if *seen_ret {
                return false;
            }
            *seen_ret = true;
            let fn_name = match box_return {
                MhRealBox::Int => "boxInteger",
                MhRealBox::Long => "boxLong",
                MhRealBox::Keep => return false,
            };
            mh_real_dmh_member_matches(
                ctx,
                filter,
                "sun/invoke/util/ValueConversions",
                fn_name,
                None,
            )
        }
        _ => false,
    }
}

/// Structural proof for adapters whose `DirectMethodHandle.asTypeCache` was
/// overwritten by another `asType` of the same direct handle.
fn mh_real_as_type_by_forms(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    target: ObjectRef,
    from_params: &[String],
    unbox: &[u8; MH_BIND_CONVERSION_MAX_WORDS],
    cast_ids: &[u32; MH_BIND_CONVERSION_MAX_WORDS],
    box_return: MhRealBox,
) -> bool {
    let expected_steps = unbox[..from_params.len()]
        .iter()
        .zip(&cast_ids[..from_params.len()])
        .filter(|(&u, &c)| u != 0 || c != 0)
        .count()
        + usize::from(box_return != MhRealBox::Keep);
    if expected_steps == 0 || expected_steps > 16 {
        return false;
    }
    let Some(handle_form) = mh_real_obj(ctx.get_field_by_name(handle, "form"))
        .and_then(|f| mh_real_uncustomize_form(ctx, f))
    else {
        return false;
    };
    let Some(reinvoker) = mh_real_rebind_form(ctx, target) else {
        return false;
    };
    let mut filters = Vec::with_capacity(expected_steps);
    for k in 1..=expected_steps {
        let Some(f) = mh_real_obj(ctx.get_field_by_name(handle, &format!("argL{k}"))) else {
            return false;
        };
        filters.push(f);
    }
    let mut path = Vec::with_capacity(expected_steps);
    fn dfs(
        ctx: &mut dyn NativeContext,
        cur: ObjectRef,
        goal: ObjectRef,
        remaining: usize,
        path: &mut Vec<(u8, usize)>,
        filters: &[ObjectRef],
        from_params: &[String],
        unbox: &[u8; MH_BIND_CONVERSION_MAX_WORDS],
        cast_ids: &[u32; MH_BIND_CONVERSION_MAX_WORDS],
        box_return: MhRealBox,
    ) -> bool {
        if remaining == 0 {
            if cur != goal {
                return false;
            }
            let mut seen_args = 0u32;
            let mut seen_ret = false;
            return path.iter().zip(filters).all(|(&(kind, pos), &filter)| {
                mh_real_step_filter_matches(
                    ctx,
                    filter,
                    kind,
                    pos,
                    from_params,
                    unbox,
                    cast_ids,
                    box_return,
                    &mut seen_args,
                    &mut seen_ret,
                )
            });
        }
        let mut children = Vec::new();
        mh_real_transform_children(ctx, cur, &mut children);
        for (kind, pos, next) in children {
            path.push((kind, pos));
            if dfs(
                ctx,
                next,
                goal,
                remaining - 1,
                path,
                filters,
                from_params,
                unbox,
                cast_ids,
                box_return,
            ) {
                return true;
            }
            path.pop();
        }
        false
    }
    dfs(
        ctx,
        reinvoker,
        handle_form,
        expected_steps,
        &mut path,
        &filters,
        from_params,
        unbox,
        cast_ids,
        box_return,
    )
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
pub fn mh_real_unbox_positions_survive(
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
    let target = mh_real_direct_target_candidate(&*ctx, handle)?;
    if real_handle_record(ctx, handle)?.declared != site {
        return None;
    }
    let record = real_handle_record(ctx, target)?;
    let direct = record.direct.as_ref()?;
    let (kind, own_type) = match direct.ref_kind {
        9 => {
            if !mh_constant_bind_interface_enabled() {
                return None;
            }
            (
                MH_KIND_VIRTUAL,
                format!("(L{};{}", direct.class, direct.desc.get(1..)?),
            )
        }
        _ => mh_real_direct_kind(direct)?,
    };
    if record.declared != own_type {
        return None;
    }
    let (planned, cast_names, box_return) = mh_real_conversion_plan_with_casts(site, &own_type)?;
    let mut cast = [0u32; MH_BIND_CONVERSION_MAX_WORDS];
    for (i, name) in cast_names.iter().enumerate() {
        if let Some(name) = name {
            let id = ctx.class_id_by_name(name).map(|c| c.as_u32()).unwrap_or(0);
            if id == 0 {
                return None;
            }
            cast[i] = id;
        }
    }
    let (from_params, _) = split_descriptor_params(site)?;
    if !mh_real_as_type_cache_matches(&*ctx, target, handle)
        && !mh_real_as_type_by_forms(
            ctx,
            handle,
            target,
            &from_params,
            &planned,
            &cast,
            box_return,
        )
    {
        return None;
    }
    let (unbox, wrapper_class_ids) = mh_real_resolve_wrappers(&*ctx, planned);
    if !mh_real_unbox_positions_survive(&planned, &unbox) {
        return None;
    }
    let lane = mh_real_lane_record(direct, own_type);
    let bind = if direct.ref_kind == 9 {
        mh_compiled_bind_interface_member(ctx, &lane)?
    } else {
        mh_compiled_bind_member(ctx, kind, Value::Object(None), None, &lane, direct.owner)?
    };
    // No bound receiver went in, so `bind.bound` is `false`.
    Some((
        bind,
        MhRealConversion {
            unbox,
            cast,
            box_return,
            wrapper_class_ids,
        },
    ))
}

#[cfg(test)]
mod j27_mh_real_astype_plan_tests;
