#[allow(unused_imports)]
use super::*;

/// [`invoke_reference_cast_refusal_with`] once `MH_DESC` is decoded and split
/// (`params`), and, when the caller already holds it, `MH_CLASS` decoded
/// (`class`; `None` reads it here, as before). Round 15 wave 1 (lane mh): the
/// direct lane hands in its [`DirectLaneRecord`]'s, decoded from the same
/// objects, so the `invoke` door's refusals decode nothing per call.
#[allow(clippy::too_many_arguments)]
pub fn invoke_reference_cast_refusal_decoded(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    needs_receiver: bool,
    has_bound: bool,
    extra: &[Value],
    call_site: Option<&str>,
    declared_type: Option<&str>,
    params: &[String],
    class: Option<&str>,
) -> Option<MethodCallFailed> {
    // See `invoke_reference_cast_refusal_with`: direct kinds only.
    if !kind_has_authoritative_type(kind) {
        return None;
    }
    // The OUTER cast first -- `asType(callSiteType)` casts to the handle's
    // declared `type`, receiver included -- then the leaf's own below.
    let declared = match invoke_declared_type_refusal(ctx, mh, declared_type, extra, call_site) {
        Ok(declared) => declared,
        Err(refusal) => return Some(refusal),
    };
    let receiver_taken = needs_receiver && !has_bound && !extra.is_empty();
    if receiver_taken {
        let class = match class {
            Some(class) => Some(class.to_string()),
            None => mh_read_class(ctx, mh),
        };
        if let Some(class) = class {
            let class = mh_leaf_receiver_class(ctx, mh, class);
            // `findVirtual(int[].class, "clone", ..)` names an ARRAY class,
            // whose descriptor is its name: `L[I;` read as the class `[I`,
            // which no array is an instance of, and refused the call.
            let target = if class.starts_with('[') {
                class
            } else {
                format!("L{class};")
            };
            let judged = declared_position_judged(declared.as_deref(), 0, &target);
            if !judged && reference_arg_admitted(ctx, extra[0], &target) == Some(false) {
                let refusal = reference_cast_failure(ctx, extra[0], &target);
                // A `findSpecial` handle's receiver type is its
                // `specialCaller`, which HotSpot's message names (wave 27).
                // `CRATONVM_MH_SPECIAL_CALLER_CAST=0` restores the leaf-only
                // cast (round 12 wave 5), message included.
                if let (MH_KIND_SPECIAL, Value::Object(Some(r)), true) =
                    (kind, extra[0], special_caller_cast_enabled())
                {
                    return Some(special_receiver_refusal_named(ctx, mh, r, refusal));
                }
                return Some(refusal);
            }
        }
    }
    let rest = if receiver_taken { &extra[1..] } else { extra };
    // Only a 1:1 alignment is judged. A varargs collector, a spread/collect
    // adapter or a partially bound handle can legitimately present a different
    // arity here, and guessing the alignment is how a cast check starts refusing
    // correct calls.
    if rest.len() != params.len() {
        return None;
    }
    // The call site's static parameter types, aligned with `extra` (the
    // `invoke` descriptor names every argument after the handle, the receiver
    // included), when the door published one that fits.
    let site_params = call_site
        .and_then(split_descriptor_params)
        .map(|(p, _)| p)
        .filter(|p| p.len() == extra.len());
    let offset = usize::from(receiver_taken);
    // A handle `explicitCastArguments` stamped reaches a primitive `MH_DESC`
    // parameter through a JLS 5.5 cast (a `null` is zero, a `Long` narrows),
    // not `asType`'s conversion: its primitive positions are not judged here
    // (wave 26; `MH_MARK_EXPLICIT_CAST`).
    let explicit = mh_is_explicit_cast(&*ctx, mh);
    for (i, (v, pdesc)) in rest.iter().zip(params.iter()).enumerate() {
        if wrapper_desc_for_primitive(pdesc).is_some() {
            if explicit {
                continue;
            }
            // A REFERENCE reaching a primitive parameter is `asType`'s
            // reference -> primitive conversion (wave 25): a `String`, a
            // non-widening wrapper or a `null` is refused, where
            // `adapt_single_arg` used to pass it through to the callee
            // (`findStatic(C, "m", (int)void).invoke((Object) "s")` ran `m`
            // on the String's address). A site that typed the argument as a
            // primitive passed it raw; a reference there is ours, not the
            // caller's, and is left to `adapt_single_arg` as before.
            if matches!(v, Value::Object(_)) {
                let site_param = site_params
                    .as_ref()
                    .and_then(|p| p.get(i + offset))
                    .map(String::as_str);
                if site_param.is_some_and(|p| wrapper_desc_for_primitive(p).is_some()) {
                    continue;
                }
                if let Err(refusal) = mh_unbox_for_primitive(ctx, pdesc, *v, site_param) {
                    return Some(refusal);
                }
            }
            continue;
        }
        // Already judged by the declared-type pass above (round 12 wave 5).
        if declared_position_judged(declared.as_deref(), offset + i, pdesc) {
            continue;
        }
        // `explicitCastArguments` passes a reference to an INTERFACE parameter
        // without a cast (`VerifyType.isNullConversion(.., keepInterfaces =
        // false)`; the leaf arms skip it under the explicit scope since wave
        // 27), so this door must not refuse it either (round 12 wave 7, lane
        // mh5; hunter3's residual 1): `explicitCastArguments(takesRunnable,
        // (Object)int).invoke("x")` runs the target on HotSpot.
        if explicit
            && descriptor_names_an_interface(&*ctx, pdesc)
            && explicit_interface_uncast_enabled()
        {
            continue;
        }
        if reference_arg_admitted(ctx, *v, pdesc) == Some(false) {
            if i + 1 == rest.len() && varargs_collects_last(&*ctx, mh, call_site, declared_type) {
                continue;
            }
            return Some(reference_cast_failure(ctx, *v, pdesc));        }    }
    None
}

/// Does `mh`, a varargs collector, COLLECT its trailing argument at this
/// `invoke` site rather than take it as the array? `AsVarargsCollector.asType`
/// passes the trailing argument through as the array only when the site's
/// type for it is assignable to the array type
/// ([`call_site_names_trailing_array`]); otherwise the argument is an ELEMENT,
/// cast to the component by the collection (`mh_entry_adapt`), so the array
/// cast must not judge it (interpreter round i2 wave 60, lane L4: once array
/// arguments were judged, `varargs.invoke((Object) new Integer[1])` refused
/// with `Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;` where
/// HotSpot collects and refuses the element, `... to java.lang.String`).
/// Asked only on a refusal's path. No site, or no declared type: `true`
/// (never refuse on a guess).
pub fn varargs_collects_last(
    ctx: &dyn NativeContext,
    mh: ObjectRef,
    call_site: Option<&str>,
    declared: Option<&str>,
) -> bool {
    if !mh_is_varargs_collector(ctx, mh) {
        return false;
    }
    match (call_site, declared) {
        (Some(site), Some(declared)) => !call_site_names_trailing_array(Some(site), declared),
        _ => true,
    }
}

/// `CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST` -- default ON (round 12 wave 7,
/// lane mh5); `0`/`false`/`off`/`no` makes the `invoke` /
/// `invokeWithArguments` doors cast a reference to an interface parameter of
/// an `explicitCastArguments` copy again (a `ClassCastException` HotSpot does
/// not raise). Read only for an explicit copy's interface parameter.
pub fn explicit_interface_uncast_enabled() -> bool {
    true
}

/// The cast `asType(callSiteType)` makes to the handle's DECLARED `type`, for
/// `invoke` and `invokeWithArguments`: `Err` is the `ClassCastException`,
/// `Ok(Some(params))` the declared parameters every argument was judged
/// against, `Ok(None)` "not judged" (no readable `type`, or an arity that is
/// not the argument count -- a collector, a fabricated `()V`).
///
/// The leaf check in [`invoke_reference_cast_refusal`] reads `MH_CLASS` and
/// `MH_DESC`, and for a `findSpecial` / `unreflectSpecial` handle those are
/// NOT the declared type: the JDK narrows the receiver to `specialCaller`
/// (`Lookup.restrictReceiver`), while `MH_CLASS` stays the class dispatch
/// resolves in. Measured on round 12 wave 4 (`R12Hunt2FindSpecial`):
/// `findSpecial(Mid, "m", (int)int, Sub).invoke((Object) new Mid(), 3)` ran
/// `Mid.m` on a `Mid` and answered 9; HotSpot 25 throws
/// `ClassCastException: Cannot cast Mid to Sub`. The same pass also covers an
/// `asType` copy of a direct handle, whose `type` is the outer type the call
/// site is cast to while `MH_DESC` keeps the leaf's.
///
/// Same predicate, same fail-open rule as every other cast in this file:
/// only a positive `Some(false)` from [`reference_arg_admitted`] refuses.
/// `CRATONVM_MH_INVOKE_DECLARED_CAST=0` restores the leaf-only check; it is
/// read only when a refusal is about to be raised, so the common path pays no
/// flag read. Spec conformance, identical in both modes.
pub fn invoke_declared_type_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    declared: Option<&str>,
    extra: &[Value],
    call_site: Option<&str>,
) -> Result<Option<Vec<String>>, MethodCallFailed> {
    // `declared` is the handle's `mh_declared_descriptor` (round 12 wave 7:
    // read once per call by the door).
    let Some(declared_desc) = declared else {
        return Ok(None);
    };
    let Some(params) = declared_cast_params(declared_desc, extra.len()) else {
        return Ok(None);
    };
    for (pos, (v, pdesc)) in extra.iter().zip(params.iter()).enumerate() {
        if reference_arg_admitted(ctx, *v, pdesc) == Some(false) {
            if !invoke_declared_cast_enabled() {
                return Ok(None);
            }
            if call_site_names_declared_param(call_site, extra.len(), pos, pdesc) {
                continue;
            }
            if pos + 1 == extra.len() && varargs_collects_last(&*ctx, mh, call_site, declared) {
                continue;
            }
            return Err(reference_cast_failure(ctx, *v, pdesc));
        }
    }
    Ok(Some(params))
}

/// Round 13 wave 1 (lane mhffm; `r12w5-hunter3-methodhandle-cast-model-residuals`
/// item 2): does the call site already type argument `pos` exactly as the
/// declared parameter `pdesc`? `asType(callSiteType)` casts only where the two
/// types differ, so HotSpot does not judge such a position (only non-javac
/// bytecode can put a non-implementer in an interface-typed slot there).
/// `CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED=0` judges it anyway, as
/// before; read only when a refusal is about to be raised.
pub fn call_site_names_declared_param(
    call_site: Option<&str>,
    argc: usize,
    pos: usize,
    pdesc: &str,
) -> bool {
    let Some((site, _)) = call_site.and_then(split_descriptor_params) else {
        return false;
    };
    site.len() == argc
        && site.get(pos).is_some_and(|t| t == pdesc)
}

/// `CRATONVM_MH_INVOKE_DECLARED_CAST` -- default ON (round 12 wave 5, lane
/// hunter3); `0`/`false`/`off`/`no` restores the leaf-only cast
/// (`MH_CLASS`/`MH_DESC`) that `invoke` performed before.
pub fn invoke_declared_cast_enabled() -> bool {
    true
}

/// The declared parameters of `declared` when there is one per argument, or
/// `None`. `argc == 0` has nothing to cast.
pub fn declared_cast_params(declared: &str, argc: usize) -> Option<Vec<String>> {
    if argc == 0 {
        return None;
    }
    let (params, _) = split_descriptor_params(declared)?;
    (params.len() == argc).then_some(params)
}

/// Did the declared pass already judge argument `pos` against this very
/// `target`? Then the leaf pass would ask the same question of the same value.
pub fn declared_position_judged(declared: Option<&[String]>, pos: usize, target: &str) -> bool {
    declared
        .and_then(|d| d.get(pos))
        .is_some_and(|t| t == target)
}

/// The INNER cast of an `asType` copy of a direct handle, which
/// `invokeExact` owes as much as `invoke` does, or `None`.
///
/// `h.asType(t)` is a new handle whose adapter casts each argument from `t`'s
/// parameter to the target's; `invokeExact` on it performs that cast. This VM
/// stamps `t` on a copy of the direct handle (`mh_with_stamped_type`), so the
/// adapter is gone and `invokeExact` dispatched the uncast argument straight
/// to the leaf. Measured shapes (HotSpot 25: `ClassCastException` for each):
///
/// ```text
/// findVirtual(Mid, m, (int)int).asType((Object,int)int).invokeExact((Object) grand, 3)
///     ran Grand.m on the Grand
/// findStatic(C, takesSub, (Sub)int).asType((Object)int).invokeExact((Object) mid)
///     ran takesSub with a Mid in its Sub parameter
/// ```
///
/// The second is not only a wrong answer: the callee's bytecode, and its
/// compiled code, trust a verified `Sub` parameter, so a field of `Sub` read
/// through it lands outside a `Mid`.
///
/// Judged exactly where the declared parameter and the leaf's differ, which is
/// exactly where the JDK's adapter casts; an unadapted handle (declared ==
/// leaf at every position) is never judged. Same predicate and fail-open rule
/// as [`invoke_reference_cast_refusal`]'s leaf pass, and the same leaf
/// placement: [`mh_leaf_receiver_class`] (`specialCaller`, else `MH_CLASS`)
/// for an unbound virtual/special receiver, `MH_DESC` for the rest. So
/// `findSpecial(Mid, m, (int)int, Sub).asType((Object,int)int)
/// .invokeExact((Object) mid, 3)` is refused as on HotSpot, although the copy's
/// `type` no longer names `Sub`.
/// `CRATONVM_MH_INVOKEEXACT_INNER_CAST=0` restores the uncast dispatch; it is
/// read only when a refusal is about to be raised.
pub fn invoke_exact_inner_cast_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    declared: Option<&str>,
    extra: &[Value],
) -> Option<MethodCallFailed> {
    // A real JDK handle has no `MH_BOUND` / `MH_DESC` to judge (it would end
    // at `mh_read_desc`'s `None`; the reads were heap OOB warnings; wave 56,
    // lane L4).
    if !kind_has_authoritative_type(kind)
        || extra.is_empty()
        || ctx.object_num_fields(mh) <= MH_BOUND
    {
        return None;
    }
    let (declared_params, _) = split_descriptor_params(declared?)?;
    if declared_params.len() != extra.len() {
        return None;
    }
    let bound = matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)));
    let mut leaf: Vec<String> = Vec::with_capacity(extra.len());
    if matches!(kind, MH_KIND_VIRTUAL | MH_KIND_SPECIAL) && !bound {
        let class = mh_read_class(ctx, mh)?;
        let class = mh_leaf_receiver_class(ctx, mh, class);
        leaf.push(if class.starts_with('[') {
            class
        } else {
            format!("L{class};")
        });
    }
    let desc = mh_read_desc(ctx, mh)?;
    let (leaf_params, _) = split_descriptor_params(&desc)?;
    // A bound STATIC handle's `MH_DESC` still names the captured leading
    // parameter, which the caller does not pass.
    let skip = usize::from(kind == MH_KIND_STATIC && bound);
    leaf.extend(leaf_params.into_iter().skip(skip));
    if leaf.len() != extra.len() {
        return None;
    }
    // An `explicitCastArguments` stamp passes a reference to an INTERFACE
    // uncast (JLS 5.5; `leaf_reference_refusal`'s rule, which the leaf arms
    // apply): `explicitCastArguments(takesRunnable, (Object)String)
    // .invokeExact((Object) "s")` answers `s` on HotSpot 25.0.3, and this
    // refused it with `Cannot cast java.lang.String to java.lang.Runnable`
    // (interpreter round i1 wave 28, lane L4, reviewing round 12's cast).
    let explicit = mh_is_explicit_cast(&*ctx, mh);
    for ((v, have), want) in extra.iter().zip(declared_params.iter()).zip(leaf.iter()) {
        if have == want {
            continue;
        }
        if explicit && descriptor_names_an_interface(&*ctx, want) {
            continue;
        }
        if reference_arg_admitted(ctx, *v, want) == Some(false) {
            // An `explicitCastArguments` copy passes a reference to an
            // INTERFACE parameter uncast (round 12 wave 7, lane mh5; the
            // `invoke` door's leaf pass skips it for the same reason).
            if mh_is_explicit_cast(&*ctx, mh)
                && descriptor_names_an_interface(&*ctx, want)
                && explicit_interface_uncast_enabled()
            {
                continue;
            }
            if !invoke_exact_inner_cast_enabled() {
                return None;
            }
            return Some(reference_cast_failure(ctx, *v, want));        }    }
    None
}

/// `CRATONVM_MH_INVOKEEXACT_INNER_CAST` -- default ON (round 12 wave 5, lane
/// hunter3); `0`/`false`/`off`/`no` restores `invokeExact`'s uncast dispatch
/// of an `asType` copy of a direct handle.
pub fn invoke_exact_inner_cast_enabled() -> bool {
    true
}

/// The reference RETURN cast an `asType` adapter owes, or `None`.
///
/// The argument casts above have a return-side twin that nothing performed:
/// `(String) mh.invoke()` on a handle declared `()Object`, or `invokeExact` on
/// `findStatic(C, "obj", ()Object).asType(()String)`, handed whatever object
/// the leaf returned straight to a call site typed `String` -- javac emits no
/// `checkcast` after a signature-polymorphic call (the cast is in the call
/// site's descriptor), and `vm_exec::unbox_poly_return` passes references
/// through, so the caller held, say, an `Integer` in a `String` variable.
/// HotSpot 25 throws `ClassCastException` from the adapter.
///
/// Two casts, exactly where the JDK's adapters make them:
/// * INNER -- an `asType` copy of a direct handle (`kind_has_authoritative_type`)
///   whose declared return differs from the leaf's reference return;
/// * OUTER -- `invoke`'s `asType(callSiteType)`, when the call site's return
///   differs from the declared (else leaf) reference return.
///
/// Class, interface and (since wave 60) array targets are judged. Only the
/// INNER cast of an `explicitCastArguments` stamp (`explicit`, read by the
/// door before its dispatch) skips an interface: that conversion is JLS 5.5's,
/// which passes a reference to an interface uncast. `asType` casts to one --
/// HotSpot 25.0.3: `(Runnable) h.invoke()` on a `()Object` handle answering a
/// `String`, and `h.asType(()Runnable).invokeExact()`, throw `Cannot cast
/// java.lang.String to java.lang.Runnable` -- and round 12 skipped every
/// interface, so the caller held the `String` in a `Runnable` variable
/// (interpreter round i1 wave 28, lane L4). A primitive leaf return is not
/// this function's (boxing to an unrelated class is `asType`'s
/// `WrongMethodTypeException`). Same predicate and fail-open rule as the
/// argument casts. `CRATONVM_MH_RETURN_CAST=0` restores the uncast return; it
/// is read only when a refusal is about to be raised.
pub fn return_cast_refusal(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    site: Option<&str>,
    explicit: bool,
) -> Option<MethodCallFailed> {
    let value = match result {
        Ok(Some(v @ Value::Object(Some(_)))) => *v,
        _ => return None,
    };
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let leaf_ret = return_type_desc(leaf_desc);
    let declared_ret = declared.map(return_type_desc);
    let inner = declared_ret
        .filter(|d| kind_has_authoritative_type(kind) && is_reference(leaf_ret) && *d != leaf_ret);
    let from = declared_ret.unwrap_or(leaf_ret);
    let outer = site
        .map(return_type_desc)
        .filter(|s| is_reference(from) && *s != from);
    let targets = [inner.map(|t| (t, true)), outer.map(|t| (t, false))];
    for (target, is_inner) in targets.into_iter().flatten() {
        // An array target is cast too, component included (wave 60, lane
        // L4: `asType(()String[])` over a leaf returning an `Integer[]` as
        // `Object` handed the `Integer[]` to a `String[]` call site).
        if !is_reference(target) {
            continue;
        }
        if is_inner && explicit && descriptor_names_an_interface(ctx, target) {
            continue;
        }
        if reference_arg_admitted(ctx, value, target) == Some(false) {
            if !return_cast_enabled() {
                return None;
            }
            return Some(reference_cast_failure(ctx, value, target));        }    }
    None
}

/// Could [`return_cast_refusal`] judge a cast for these types? A door that owes
/// one does work after its dispatch, so it must not arm [`MH_TAIL`]: a yield
/// that unwound a transparent door frame would skip the cast. The same inner /
/// outer targets as [`return_cast_refusal`], without its interface screen, so
/// the answer errs towards an opaque door.
pub fn return_cast_may_apply(
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    site: Option<&str>,
) -> bool {
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let leaf_ret = return_type_desc(leaf_desc);
    let declared_ret = declared.map(return_type_desc);
    let inner = declared_ret
        .filter(|d| kind_has_authoritative_type(kind) && is_reference(leaf_ret) && *d != leaf_ret);
    let from = declared_ret.unwrap_or(leaf_ret);
    let outer = site
        .map(return_type_desc)
        .filter(|s| is_reference(from) && *s != from);
    [inner, outer].into_iter().flatten().any(is_reference)
}

/// Does the class descriptor `desc` (`Lp/C;`) name a loaded INTERFACE? `false`
/// when it names no loaded class.
pub fn descriptor_names_an_interface(ctx: &dyn NativeContext, desc: &str) -> bool {
    desc.strip_prefix('L')
        .and_then(|d| d.strip_suffix(';'))
        .and_then(|name| ctx.class_id_by_name(name))
        .is_some_and(|cid| ctx.is_interface_class(cid))
}

/// `CRATONVM_MH_RETURN_CAST` -- default ON (round 12 wave 5, lane hunter3);
/// `0`/`false`/`off`/`no` restores the uncast reference return of `invoke` /
/// `invokeExact`.
pub fn return_cast_enabled() -> bool {
    true
}

/// The reference -> primitive RETURN conversion an `asType` adapter owes, or
/// `None` (the door's ordinary boxing then runs, as before). The primitive
/// twin of [`return_cast_refusal`] (interpreter round i2 wave 58, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// `target` is the type the door's caller receives: the call site for
/// `invoke`, the declared type for `invokeExact`. Two conversions, exactly
/// where the JDK's adapters make them:
/// * OUTER -- `invoke`'s `asType(callSiteType)` from a handle whose DECLARED
///   return is a reference to a primitive call-site return (`(int)
///   objHandle.invoke()`), for a kind whose `type` is authoritative;
/// * INNER -- an `asType` copy of a direct handle
///   (`kind_has_authoritative_type`) from its leaf's reference return to a
///   primitive declared one (`objHandle.asType(()int).invokeExact()`), then
///   widened to `target`'s.
///
/// Measured on HotSpot 25.0.3 (`L4W58MethodHandleReturnUnboxing.java`):
/// `null` is `primitiveConversion`'s NPE, a wrapper that does not widen (or a
/// `Number` that is no wrapper) `Cannot cast java.lang.Long to
/// java.lang.Integer`, anything else the `(Number)` cast's CCE: exactly
/// [`mh_unbox_for_primitive`]'s answers for an argument. The coercion after
/// the native (`vm_exec::coerce_value_against_ret_char`) fabricated `0` for
/// each. A `void` declared return is never judged (`asType` makes it `0`),
/// nor a raw primitive the dispatch did not box, nor a handle with no `type`.
///
/// One predictable branch on the hot path: `target` returns a primitive.
#[inline]
pub fn invoke_return_unbox(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    target: Option<&str>,
) -> Option<MethodCallResult> {
    let target = target?;
    let bytes = target.as_bytes();
    // `...)I`: a one-byte return after the parameter list; `)[I` and `;)` do
    // not match, `V` is `asType`'s zero.
    if bytes.len() < 2 || bytes[bytes.len() - 2] != b')' || bytes[bytes.len() - 1] == b'V' {
        return None;
    }
    invoke_return_unbox_cold(ctx, result, marks, kind, leaf_desc, declared?, target)
}

/// Could [`invoke_return_unbox`]'s INNER conversion apply to a door whose
/// caller receives a REFERENCE (the `invokeWithArguments` doors)? Such a door
/// then does work after its dispatch and must not arm [`MH_TAIL`]'s `L`
/// token (a yield under a transparent door would skip the unboxing). The
/// same shape test, without the value; string compares only.
pub fn inner_return_unbox_may_apply(
    kind: i32,
    leaf_desc: &str,
    declared: Option<&str>,
    marks: i32,
) -> bool {
    let Some(declared) = declared else {
        return false;
    };
    let declared_ret = return_type_desc(declared);
    let leaf_ret = return_type_desc(leaf_desc);
    declared_ret.len() == 1
        && declared_ret != "V"
        && (leaf_ret.starts_with('L') || leaf_ret.starts_with('['))
        && kind_has_authoritative_type(kind)
        && marks & MH_MARK_CAST_CHAIN == 0
        && crate::nbflags().mh_invoke_return_unbox
}

/// The shape test and conversion of [`invoke_return_unbox`], for a `target`
/// with a primitive return.
#[inline(never)]
pub fn invoke_return_unbox_cold(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: &str,
    target: &str,
) -> Option<MethodCallResult> {
    let enabled = crate::nbflags().mh_invoke_return_unbox;
    let converted = invoke_return_unbox_judged(
        ctx, result, marks, kind, leaf_desc, declared, target, enabled,
    )?;
    // Boxed as the call site's own wrapper: every route that reaches these
    // natives unboxes it again (`vm_exec::unbox_poly_return_checked`).
    Some(converted.and_then(|v| auto_box_return(ctx, Ok(Some(v)), target)))
}

/// [`invoke_return_unbox_cold`]'s conversion, UNBOXED (the primitive of
/// `target`'s return, or the refusal), with `CRATONVM_MH_INVOKE_RETURN_UNBOX`
/// as a parameter (the unit tests ask both answers).
#[allow(clippy::too_many_arguments)]
pub fn invoke_return_unbox_judged(
    ctx: &mut dyn NativeContext,
    result: &MethodCallResult,
    marks: i32,
    kind: i32,
    leaf_desc: &str,
    declared: &str,
    target: &str,
    enabled: bool,
) -> Option<Result<Value, MethodCallFailed>> {
    if !enabled {
        return None;
    }
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    let is_value_primitive = |t: &str| t.len() == 1 && t != "V";
    let target_ret = return_type_desc(target);
    if !is_value_primitive(target_ret) {
        return None;
    }
    let declared_ret = return_type_desc(declared);
    let (from, prim) = if is_reference(declared_ret) && kind_has_authoritative_adapted_type(kind) {
        (declared_ret, target_ret)
    } else if is_value_primitive(declared_ret)
        && kind_has_authoritative_type(kind)
        && is_reference(return_type_desc(leaf_desc))
        // A cast chain judges its own return (`mh_cast_chain_judge_return`).
        && marks & MH_MARK_CAST_CHAIN == 0
    {
        (return_type_desc(leaf_desc), declared_ret)    } else {
        return None;
    };
    let value = match result {
        Ok(Some(v @ Value::Object(_))) => *v,
        _ => return None,
    };
    // An `explicitCastArguments` stamp's own conversion is JLS 5.5's: a
    // `null` is zero, a wrapper is unboxed and then widened or narrowed
    // (HotSpot 25.0.3: `explicitCastArguments(objLong, ()int).invokeExact()`
    // is `6`; this door's caller refused it with the strict `invokeExact`
    // return check). `invoke`'s own `asType` is never explicit.
    let explicit = from != declared_ret && marks & MH_MARK_EXPLICIT_CAST != 0;
    let converted = if explicit {
        adapt_leaf_arg(ctx, value, prim, Some(from), true)
    } else {
        mh_unbox_for_primitive(ctx, prim, value, Some(from))
    };
    let unboxed = match converted {
        Ok(v) => v,
        Err(refusal) => {
            if crate::nbflags().dbg_mh_dispatch {
                eprintln!("[MH_RETURN_UNBOX] refused {from} -> {prim} at {target}");
            }
            return Some(Err(refusal));
        }
    };
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_RETURN_UNBOX] {from} -> {prim} at {target}");
    }
    let (Some(&p), Some(&t)) = (prim.as_bytes().first(), target_ret.as_bytes().first()) else {
        return None;
    };
    Some(Ok(widen_return_value(unboxed, p, t).unwrap_or(unboxed)))}

/// `MethodType.genericMethodType(count)`'s `checkSlotCount` for the two
/// `invokeWithArguments` doors: more than 255 arguments are
/// `IllegalArgumentException: bad parameter count <count>` before any
/// conversion, unless the handle is a variable-arity collector, whose own
/// `invokeWithArguments` (`MethodHandleImpl.AsVarargsCollector`) takes a
/// jumbo list without that type. Measured on HotSpot 25
/// (`L4W43InvokeWithArgumentsEdges.java` row
/// `fixed-300`); CratonVM answered the arity's `WrongMethodTypeException`.
/// Interpreter round i1 wave 43, lane L4.
pub fn invoke_with_arguments_count_refusal(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    count: usize,
) -> Option<MethodCallFailed> {
    if count <= 255 || mh_is_varargs_collector(ctx, this) {
        return None;
    }
    Some(
        RuntimeError::IllegalArgumentException {
            message: format!("bad parameter count {count}"),
        }
        .into(),
    )
}

/// [`invoke_reference_cast_refusal`] for the `invokeWithArguments` doors, which
/// receive their arguments already unpacked and compute `needs_receiver` /
/// `has_bound` for themselves.
///
/// Two registrations spell `invokeWithArguments` -- `([Ljava/lang/Object;)` and
/// `(Ljava/util/List;)` -- and both bypass the `invoke` native entirely. Fixing
/// `invoke` alone left `d.invokeWithArguments` differing, which is the
/// several-independent-doors shape this file meets often enough to have a name
/// for it.
pub fn invoke_with_arguments_cast_refusal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    unpacked: &[Value],
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    // A handle with none of this model's slots (a real JDK handle) has no
    // `MH_KIND` / `MH_BOUND` / `MH_DESC` to judge: its casts are its own
    // `asType`'s (`mh_run_foreign_vmentry` converts its arguments). Reading
    // them was a heap OOB warning per call and a decision taken on defaults
    // (interpreter round i2 wave 56, lane L4;
    // `interpreter-L4-a-door-reads-this-models-slots-of-a-real-handle-FIXED-20261020`).
    if ctx.object_num_fields(this) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(this, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    let needs_receiver = kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL;
    let has_bound = matches!(ctx.get_field(this, MH_BOUND), Value::Object(Some(_)));
    // `invokeWithArguments` adapts to `genericMethodType(n)`: every static
    // type is `Object`, which is what `None` means to the primitive check.
    // `declared` is the handle's `mh_declared_descriptor`, read once by the
    // door (round 12 wave 7).
    invoke_reference_cast_refusal_with(
        ctx,
        this,
        kind,
        needs_receiver,
        has_bound,
        unpacked,
        None,
        declared,
    )
}

/// Refuse a `VarHandle` INSTANCE access whose receiver is not an instance of the
/// handle's coordinate class, or whose value does not fit the declared field.
///
/// # What this VM did instead
///
/// `varhandle_set`'s instance arm resolved `field_idx` from the VarHandle's OWN
/// class and then applied it to whatever object arrived, with no arm between
/// the two lines. Measured, `probes/ReflectArgTypeSweep.java`, BOTH modes:
///
/// ```text
/// row                 HotSpot 25.0.3+9           CratonVM
/// v.wrongRef          ClassCastException         3        <- Integer STORED in a String field
/// v.wrongReceiver     ClassCastException         no-throw <- wrote through a String receiver
/// v.primWrongRef      WrongMethodTypeException   no-throw <- "nine" STORED in an int field
/// v.getWrongReceiver  ClassCastException         y        <- READ through a String receiver
/// v.casWrongRef       ClassCastException         true     <- CAS succeeded
/// ```
///
/// Two of those are worse than a wrong exception type. `v.wrongRef` leaves a
/// `String`-declared field holding an `Integer` with nothing failing at the
/// store, so the next ordinary read of that field is where it surfaces -- at a
/// site that did nothing wrong. `v.getWrongReceiver` applied a `Box` field index
/// to a `String` and returned what it found there.
///
/// # Why the ORDER of the checks is the whole design
///
/// These are the CAS-dominated paths this file has been tuned for twice: a
/// thread-local plan memo took the global lock off the JIT's fast paths (a
/// scaling probe went 0.07x -> 0.68x at 24 threads), and `vh_meta_get` returns
/// an `Arc` precisely so a hot op pays a refcount bump instead of three `String`
/// clones. A per-operation `class_id_by_name` + `is_subclass` would take a
/// class-manager read lock on every `CompletableFuture` composition step and
/// undo both.
///
/// So the receiver check is an INTEGER COMPARE against `meta.class_id`, which is
/// already in hand. Only a mismatch -- a subclass receiver, or a genuinely wrong
/// one -- pays `reference_arg_admitted`, and that predicate refuses only on a
/// positive reading. A handle with no meta, or whose meta carries no class name,
/// is not judged at all.
pub fn vh_instance_refusal(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    receiver: ObjectRef,
    value: Option<Value>,
) -> Option<MethodCallFailed> {
    let meta = meta?;
    if meta.class_name.is_empty() {
        return None;
    }
    // FAST PATH: the overwhelmingly common case is the exact class, and this
    // arm costs two header reads and one integer compare. The array test is
    // part of it because an array's header carries its COMPONENT's class id:
    // an `H[]` receiver matches `meta.class_id` for `H` and, unguarded, had
    // `H`'s field slot applied to the array (r11w13-orch-varhandle-set-on-an-
    // array-receiver-throws-aioobe-not-cce). `reference_arg_admitted` refuses
    // an array against a class target, so the slow path below settles it.
    let recv_cid = ctx.class_id_of_object(receiver);
    if recv_cid.as_u32() != meta.class_id || ctx.object_is_array(receiver) {
        // A lambda proxy's capture (wave 79, `lk_lambda_proxy_var_handle`):
        // the proxy is final and outside the loaded hierarchy, so only its own
        // instances are admitted; the by-name cast below cannot name it and
        // admitted anything. HotSpot's text names the proxy by `getName()`.
        if meta.class_id >= 0x8000_0000 {
            let proxy = ClassId::new(meta.class_id);
            if crate::lang_class::lambda_proxy_class_name(&*ctx, proxy).is_some() {
                return Some(owner_cast_failure(ctx, receiver, proxy));
            }
        }
        let target = format!("L{};", meta.class_name);
        let recv = Value::Object(Some(receiver));
        if reference_arg_admitted(ctx, recv, &target) == Some(false) {
            return Some(reference_cast_failure(ctx, recv, &target));
        }
    }
    vh_value_refusal(ctx, meta, value?)
}

/// `CRATONVM_VH_CAS_EXPECTED_CAST` (nio21 wave 12, lane L3; default ON, both
/// modes): the field and static arms of `compareAndSet` / `weakCompareAndSet*`
/// / `compareAndExchange*` cast the EXPECTED value to the variable's type, as
/// the JDK's bodies do (`handle.fieldType.cast(expected)`, evaluated before
/// the new value's cast), so a wrong-typed expectation is HotSpot's
/// `ClassCastException` instead of a failed comparison. `0` restores
/// judging the new value alone
/// (`docs/feature-designs/native-dispatch-architecture.md`).
pub fn vh_cas_expected_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The refusal an INSTANCE-field compare-and-set / compare-and-exchange owes,
/// in the JDK's argument order: the receiver (`receiverType.cast(holder)`),
/// then the expectation (`fieldType.cast(expected)`, under
/// [`vh_cas_expected_cast_enabled`]), then the new value
/// (`fieldType.cast(value)`). Judged on the same terms as
/// [`vh_instance_refusal`]: nothing for a handle with no meta or no class name.
pub fn vh_instance_cas_refusal(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    receiver: ObjectRef,
    expected: Value,
    new_val: Value,
) -> Option<MethodCallFailed> {
    if !vh_cas_expected_cast_enabled() {
        return vh_instance_refusal(ctx, meta, receiver, Some(new_val));
    }
    if let Some(refusal) = vh_instance_refusal(ctx, meta, receiver, Some(expected)) {
        return Some(refusal);
    }
    let meta = meta.filter(|m| !m.class_name.is_empty())?;
    vh_value_refusal(ctx, meta, new_val)
}

/// The STATIC-field twin of [`vh_instance_cas_refusal`]: the expectation's
/// cast (under [`vh_cas_expected_cast_enabled`]), then the new value's.
pub fn vh_static_cas_refusal(
    ctx: &mut dyn NativeContext,
    meta: &VarHandleMeta,
    expected: Value,
    new_val: Value,
) -> Option<MethodCallFailed> {
    if vh_cas_expected_cast_enabled() {
        if let Some(refusal) = vh_value_refusal(ctx, meta, expected) {
            return Some(refusal);
        }
    }
    vh_value_refusal(ctx, meta, new_val)
}

/// The VALUE half of [`vh_instance_refusal`]: refuse a value that does not fit
/// the handle's declared field type. Shared with the STATIC-field store paths
/// (`set`, `compareAndSet`, `compareAndExchange`, `getAndSet`), which judged
/// nothing at all until round 11 wave 4 — `VH.set((Object) "x")` on a static
/// `int[]` or `Integer` field stored the `String`.
pub fn vh_value_refusal(
    ctx: &mut dyn NativeContext,
    meta: &VarHandleMeta,
    value: Value,
) -> Option<MethodCallFailed> {
    // A PRIMITIVE field given a reference. Every store-mode native now runs
    // `vh_adapt_value_args` first, which unboxes a wrapper and throws the
    // call-site-correct CCE / WMTE / NPE for anything else (round 11 wave 16),
    // so a reference reaches this arm only from a caller that skipped the
    // adaptation. It stays as the backstop it was: a non-wrapper is refused
    // (WMTE, the `String`-typed-site answer) rather than stored as an address.
    // Its old claim that a wrapper was fine to STORE here was the wrong value
    // of `R11W15RtVarHandleObjectSite` -- the wrapper's address went into the
    // slot.
    if matches!(
        meta.field_desc.as_str(),
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C"
    ) {
        if let Value::Object(Some(o)) = value {
            // An `Integer[]`'s header carries `Integer`'s class id, and read as
            // a wrapper it was STORED into the `int` field. An array is never a
            // wrapper; name it as the array it is.
            let (name, is_array) = if ctx.object_is_array(o) {
                (entry_value_type_display(&*ctx, o), true)
            } else {
                let cid = ctx.class_id_of_object(o);
                (ctx.class_name_of_id(cid).unwrap_or_default(), false)
            };
            if is_array || crate::lang_class::wrapper_to_prim_desc(&name).is_none() {
                // The variable's Java name (`int`), not its descriptor
                // (`I`), as every other refusal here spells it. Built before
                // the throw borrows `ctx` mutably.
                let text = format!(
                    "cannot convert {} to {}",
                    name.class_text(&*ctx),
                    vh_prim_java_name(meta.field_desc.as_bytes()[0])
                );
                return Some(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &text,
                ));
            }
        }
        return None;
    }
    // An ARRAY-typed field. `reference_arg_admitted` answers "cannot tell" for
    // every `[` descriptor, so `VH.set(h, (Object) "x")` on an `int[]` field
    // used to STORE the `String` (r11w3-rt-varhandle-array-typed-field-stores-unchecked).
    if meta.field_desc.starts_with('[') {
        if let Value::Object(Some(obj)) = value {
            if vh_array_field_value_admitted(&*ctx, obj, &meta.field_desc) == Some(false) {
                let have = entry_value_type_display(&*ctx, obj);
                return Some(
                    RuntimeError::ClassCastException {
                        message: format!(
                            "Cannot cast {have} to {}",
                            jvm_type_display(&meta.field_desc)
                        ),
                    }
                    .into(),
                );
            }
        }
        return None;
    }
    if reference_arg_admitted(ctx, value, &meta.field_desc) == Some(false) {
        return Some(reference_cast_failure(ctx, value, &meta.field_desc));    }
    None
}

// ---------------------------------------------------------------------------
// VarHandle value adaptation: the `asType` step a primitive variable owes its
// value parameters (round 11 wave 16, lane vh)
// ---------------------------------------------------------------------------
//
// # The wrong value this closes
//
// `F.set(h, (Object) Integer.valueOf(i))` on an `int` field `h.f` read back
// the POINTER BITS of the `Integer`, not `i`, in every mode including
// `--nojit` (probe `R11W15RtVarHandleObjectSite`: `boxed bad=20000`, HotSpot
// `bad=0`). The value arrives as `Value::Object(Some(Integer))` because the
// call site's static type is `Object`; `vh_value_refusal` admitted every
// wrapper, `varhandle_set` handed the reference to `set_field`, and
// `coerce_field_value_for_slot` stored the address into the `int` slot
// (`PointerIntoPrimitive`, 60 000 hits in the run's descriptor-coercion
// census). A `Short` read back the same way, a `Long` (HotSpot: CCE) was
// stored as its address, and a `String` threw WMTE where HotSpot throws CCE.
// `compareAndSet(h, (Object) 0, (Object) 1)` compared an `Integer` reference
// against the `int` slot and always FAILED.
//
// HotSpot runs `asType(callSiteType)` before the access. For a primitive
// variable of type `p` and a value whose call-site static type is `s`:
//
// * `s` primitive: widening only (`primitive_widens(s, p)`), else WMTE. The
//   widening is NUMERIC: `set(h, 5)` on a `float` field stores `5.0f`. The
//   heap's slot coercion reinterprets an `Int` at an `F` slot as raw bits,
//   so the conversion has to happen here.
// * `s` a reference: `MethodType.canConvert` admits it statically only if
//   `s` is a supertype of `p`'s wrapper, or a wrapper that widens to `p`
//   (`String`, `Long`-into-`int`: WMTE). At run time `unboxWiden` then
//   takes a null to NPE, a non-wrapper to CCE (`(Number) x`), a wrapper that
//   does not widen to `p` to CCE (`Integer.class.cast(x)`), and widens the
//   rest numerically.
//
// `s` comes from `cratonvm_native_api::poly_call_site::take_vh_value_site`.
// When no door armed it (the JIT's site-cached dispatch, a direct call), the
// static check is skipped and the run-time `Object`-site rules apply: a
// wrapper is unboxed and widened, anything else is CCE, null is NPE.

/// The Java name of a primitive descriptor byte.
pub fn vh_prim_java_name(p: u8) -> &'static str {
    match p {
        b'Z' => NAME_BOOLEAN,
        b'B' => NAME_BYTE,
        b'C' => NAME_CHAR,
        b'S' => NAME_SHORT,
        b'I' => NAME_INT,
        b'J' => NAME_LONG,
        b'F' => NAME_FLOAT,
        b'D' => NAME_DOUBLE,
        _ => "?",
    }
}

/// The dotted wrapper class name of a primitive descriptor byte, as
/// `Class.cast`'s message spells it.
pub fn vh_prim_wrapper_display(p: u8) -> &'static str {
    match p {
        b'Z' => "java.lang.Boolean",
        b'B' => "java.lang.Byte",
        b'C' => "java.lang.Character",
        b'S' => "java.lang.Short",
        b'I' => "java.lang.Integer",
        b'J' => "java.lang.Long",
        b'F' => "java.lang.Float",
        b'D' => "java.lang.Double",
        _ => "java.lang.Object",
    }
}

/// `MethodType.canConvert(site, p)` for a value parameter whose call-site
/// class is `site` and a PRIMITIVE variable `p`. `None` when the site is
/// unknown (nothing armed), which skips the static half.
pub fn vh_site_admits_primitive(site: u8, p: u8) -> Option<bool> {
    use cratonvm_native_api::poly_call_site::vh_site;
    if site == vh_site::UNKNOWN {
        return None;
    }
    if vh_site::is_primitive(site) {
        return Some(primitive_widens(site, p));
    }
    if let Some(w) = vh_site::wrapper_prim(site) {
        return Some(primitive_widens(w, p));
    }
    Some(match site {
        // Every wrapper is `Serializable`, `Comparable` and (JDK 15+)
        // `Constable`.
        vh_site::OBJECT | vh_site::SERIALIZABLE | vh_site::COMPARABLE | vh_site::CONSTABLE => true,
        // `Boolean` and `Character` are not `Number`s.
        vh_site::NUMBER => !matches!(p, b'Z' | b'C'),
        // Only `Integer`, `Long`, `Float` and `Double` are `ConstantDesc`s.
        vh_site::CONSTANT_DESC => matches!(p, b'I' | b'J' | b'F' | b'D'),
        _ => false,
    })
}

/// Widen `value`, read as primitive kind `src`, to primitive kind `dst`
/// numerically (JLS 5.1.2). `None` when `value` does not read as `src` or
/// `dst` is not reachable from it; the caller then keeps the value as it is.
///
/// `raw_slot` is set for a value read out of a wrapper's `value` field, where
/// an `F` slot can present as its raw bits in a `Value::Int`. An ARGUMENT is
/// never raw: an `Int` there is an `int`.
pub fn vh_widen_primitive(value: Value, src: u8, dst: u8, raw_slot: bool) -> Option<Value> {
    enum Num {
        I(i32),
        J(i64),
        F(f32),
        D(f64),
    }
    let n = match (src, value) {
        (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Int(i)) => Num::I(i),
        (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Long(l)) => Num::I(l as i32), // Cast: compact slot, low half
        (b'J', Value::Long(l)) => Num::J(l),
        (b'J', Value::Int(i)) => Num::J(i as i64),
        (b'F', Value::Float(f)) => Num::F(f),
        (b'F', Value::Int(bits)) if raw_slot => Num::F(f32::from_bits(bits as u32)), // Cast: raw float bits
        (b'D', Value::Double(d)) => Num::D(d),
        _ => return None,
    };
    Some(match (dst, n) {
        (b'Z' | b'B' | b'C' | b'S' | b'I', Num::I(i)) => Value::Int(i),
        (b'J', Num::I(i)) => Value::Long(i as i64),
        (b'J', Num::J(l)) => Value::Long(l),
        (b'F', Num::I(i)) => Value::Float(i as f32),
        (b'F', Num::J(l)) => Value::Float(l as f32),
        (b'F', Num::F(f)) => Value::Float(f),
        (b'D', Num::I(i)) => Value::Double(i as f64),
        (b'D', Num::J(l)) => Value::Double(l as f64),
        (b'D', Num::F(f)) => Value::Double(f as f64),
        (b'D', Num::D(d)) => Value::Double(d),
        _ => return None,
    })
}

/// The `WrongMethodTypeException` for a value parameter `asType` refuses
/// statically.
pub fn vh_value_wmte(ctx: &mut dyn NativeContext, value: Value, p: u8) -> MethodCallFailed {
    let from = match value {
        Value::Object(Some(o)) if ctx.object_is_array(o) => entry_value_type_display(&*ctx, o),
        Value::Object(Some(o)) => {
            let cid = ctx.class_id_of_object(o);
            ctx.class_name_of_id(cid)
                .unwrap_or_default()
                .class_text(&*ctx)
        }
        Value::Object(None) => "null".to_string(),
        Value::Int(_) => NAME_INT.to_string(),
        Value::Long(_) => NAME_LONG.to_string(),
        Value::Float(_) => NAME_FLOAT.to_string(),
        Value::Double(_) => NAME_DOUBLE.to_string(),
        _ => "?".to_string(),
    };
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("cannot convert {from} to {}", vh_prim_java_name(p)),
    )
}

/// Raise the `WrongMethodTypeException` of an `asType` refusal of a VarHandle
/// parameter (`raise`, which keeps today's two-type text) and leave the
/// handle's shape for the door, which knows the call site's descriptor and
/// rewrites the message as HotSpot's
/// `cannot convert MethodHandle(<access mode type>) to <call-site type>`
/// ([`varhandle_astype_refusal_with_site`]; interpreter round i2 wave 53,
/// lane L4,
/// `interpreter-L4-varhandle-null-coordinate-and-value-type-messages-differ-FIXED-20261017`).
/// `vh` must be current: the shape is read before `raise` allocates. A handle
/// that cannot describe itself exactly leaves no record (the text stays).
#[cold]
#[inline(never)]
pub fn vh_astype_refused(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    raise: impl FnOnce(&mut dyn NativeContext) -> MethodCallFailed,
) -> MethodCallFailed {
    // An `IndirectVarHandle` describes itself through its own fields; every
    // other handle through this model's row (or a real array handle's class).
    let shape = if is_indirect_var_handle(ctx, vh) {
        indirect_vh_shape(ctx, vh)
    } else if let Some(shape) = segment_vh_shape(ctx, vh) {
        // A real `SegmentVarHandle` (wave 57, lane L4): its form's types.
        Some(shape)
    } else {
        let (_kind, value, coordinates, exact) = vh_value_and_coordinate_descriptors(ctx, vh);
        exact.then_some((coordinates, value))
    };
    let failed = raise(ctx);
    if let Some((coordinates, value)) = shape {
        if let MethodCallFailed::ExceptionThrown(exception) = &failed {
            cratonvm_native_api::poly_call_site::note_vh_astype_refusal(
                cratonvm_native_api::poly_call_site::VhAsTypeRefusal {
                    exception: *exception,
                    coordinates,
                    value,
                },
            );
        }
    }
    failed
}

/// `asType`'s RETURN direction for a VarHandle access mode that returns its
/// variable (`get*`, `getAndSet*`, `getAndAdd*`, `getAndBitwise*`,
/// `compareAndExchange*`): at a call site whose return the door left in the
/// parameter-site word's top byte (`poly_call_site::vh_param_site_word`), a
/// variable whose type does not convert to it (`MethodType.canConvert`,
/// [`mh_can_convert`]: a narrowing primitive, a reference that is neither a
/// wrapper supertype nor a wrapper widening to it, a primitive whose wrapper
/// the reference site is not a supertype of) is HotSpot's
/// `WrongMethodTypeException`, raised before the access, its text rewritten
/// by the door as for a parameter ([`vh_astype_refused`]). A primitive site
/// answered `0` before (the box of the variable's own type reached the
/// site's unboxing, which zeroes a foreign wrapper); a reference site raised
/// W6-1's own text (`vm_exec::unbox_poly_return_checked`).
///
/// Hot path: one predictable branch for an `Object` or `void` site (`ret ==
/// 0`), and otherwise byte compares against the row the native's prologue
/// already read (the variable's descriptor). Everything else -- a mismatch,
/// a handle with no row -- is the cold half.
///
/// Interpreter round i2 wave 57, lane L4
/// (`interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`,
/// `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`).
///
/// Answers the site's primitive when the variable is typed as exactly that
/// primitive's wrapper (`Integer` at an `int` site), else `0`: `asType`
/// unboxes such a variable through `ValueConversions.unbox<W>(<W> x)`, whose
/// `null` raises `Cannot invoke "<W>.<p>Value()" because "x" is null`, where
/// every other reference variable goes through `primitiveConversion`
/// ([`vh_own_wrapper_null`]; interpreter round i2 wave 59, lane L4).
#[inline]
pub fn vh_return_site_check(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    site: u64,
    row: &VhPrologueRow,
) -> Result<u8, MethodCallFailed> {
    let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
    if ret == 0 {
        return Ok(0);
    }
    if let Some(Some(m)) = row {
        match m.field_desc.as_bytes().first() {
            // The site's own primitive.
            Some(&v) if v == ret => return Ok(0),
            // A reference variable at a reference site (a class code sits
            // below every descriptor letter): `R -> R` always converts, the
            // cast is the access's (VH2-3).
            Some(b'L' | b'[') if ret < b'A' => return Ok(0),
            // A primitive at its own wrapper's site (`(Integer) ints.get(h)`).
            Some(&v)
                if cratonvm_native_api::poly_call_site::vh_site::wrapper_prim(ret) == Some(v) =>
            {
                return Ok(0)
            }
            _ => {}
        }
    }
    vh_return_site_check_cold(ctx, args, ret, row)
}

/// The access's result `result` at a primitive site `own` whose variable is
/// typed as that primitive's wrapper ([`vh_return_site_check`]'s answer; `0`:
/// not such a site): a `null` is HotSpot's `unbox<W>(<W> x)` NPE, which
/// names `"x"` (`L4W58VarHandleReturnNullText` row `integer-null-as-int`,
/// measured on 25.0.3). Everything else passes through. One predictable
/// branch on the hot path. Interpreter round i2 wave 59, lane L4.
#[inline]
pub fn vh_own_wrapper_null(
    ctx: &mut dyn NativeContext,
    own: u8,
    result: MethodCallResult,
) -> MethodCallResult {
    if own == 0 {
        return result;
    }
    vh_own_wrapper_null_cold(ctx, own, result)
}

#[cold]
#[inline(never)]
pub fn vh_own_wrapper_null_cold(
    ctx: &mut dyn NativeContext,
    own: u8,
    result: MethodCallResult,
) -> MethodCallResult {
    if !matches!(result, Ok(Some(Value::Object(None)))) {
        return result;
    }
    let prim = byte_view_desc(own);
    let Some(wrapper) = wrapper_desc_for_primitive(prim) else {
        return result;
    };
    match mh_unbox_for_primitive(ctx, prim, Value::Object(None), Some(wrapper)) {
        Err(npe) => Err(npe),
        Ok(_) => result,
    }
}

/// The cold half of [`vh_return_site_check`]: the variable's descriptor from
/// the handle's FFM layout carrier, its row, or a real array handle's
/// component (a handle that describes itself no other way is not judged),
/// against the site's primitive `ret`.
#[cold]
#[inline(never)]
pub fn vh_return_site_check_cold(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ret: u8,
    row: &VhPrologueRow,
) -> Result<u8, MethodCallFailed> {
    if !crate::nbflags().vh_primitive_return_astype {
        return Ok(0);
    }
    let Some(Value::Object(Some(vh))) = args.first() else {
        return Ok(0);
    };
    let vh = *vh;
    let variable = if let Some(shape) = p67_segment_vh_shape(&*ctx, vh) {
        Some(layout_vh_carrier_desc(shape.carrier).to_string())
    } else if let Some(m) = vh_row_or_read(ctx, vh, row) {
        Some(m.field_desc.clone())
    } else if is_indirect_var_handle(ctx, vh) {
        // A combinator (`MethodHandles.filterValue`, ...): its own `value`.
        indirect_vh_field(ctx, vh, "value").and_then(|m| strict_mirror_descriptor(&*ctx, m))
    } else if let Some((_, value)) = segment_vh_shape(ctx, vh) {
        // A real `SegmentVarHandle` (`ValueLayout.varHandle()` under
        // `--real-jdk`): its form's erased get type.
        Some(value)
    } else {
        real_array_var_handle_descriptors(&*ctx, vh).map(|(value, _)| value)
    };
    let Some(variable) = variable.filter(|v| !v.is_empty()) else {
        return Ok(0);
    };
    // The variable typed as the site's own wrapper (wave 59, lane L4): it
    // converts, and its `null` names `"x"` (`vh_own_wrapper_null`).
    if ret >= b'A'
        && byte_view_desc(ret).as_bytes()[0] == ret
        && wrapper_desc_for_primitive(byte_view_desc(ret)) == Some(variable.as_str())
    {
        return Ok(ret);
    }
    let converts = if ret < b'A' {
        // A reference site other than `Object`: only a PRIMITIVE variable
        // is judged (`P -> R` must box to a subtype of the site's class).
        let v = variable.as_bytes()[0];
        !vh_site_is_primitive_desc(v) || vh_site_class_accepts_primitive(ret, v)
    } else {
        let ret_desc = byte_view_desc(ret);
        ret_desc.as_bytes()[0] != ret || mh_can_convert(&variable, ret_desc)
    };
    if converts {
        return Ok(0);
    }
    // `raise`'s own text names the two types, as the parameter refusals do;
    // the door that armed the word rewrites it as HotSpot's
    // `cannot convert MethodHandle(<access mode type>) to <call-site type>`.
    let site_name = if ret < b'A' {
        "the call site's type"
    } else {
        vh_prim_java_name(ret)
    };
    let text = format!(
        "cannot convert {} to {site_name}",
        descriptor_to_class_name(&variable).class_text(&*ctx),
    );
    Err(vh_astype_refused(ctx, vh, |ctx| {
        crate::phases_early::throw_jca_exc(ctx, "java/lang/invoke/WrongMethodTypeException", &text)
    }))
}

/// Is `b` the descriptor byte of one of the eight primitives?
pub fn vh_site_is_primitive_desc(b: u8) -> bool {
    cratonvm_native_api::poly_call_site::vh_site::is_primitive(b)
}

/// `MethodType.canConvert(p, site)` for a PRIMITIVE variable `p` returned
/// into a reference call site of [`vh_site`](cratonvm_native_api::poly_call_site::vh_site)
/// class `class` (never `OBJECT`, which the word does not carry): the site's
/// class must be assignable from `p`'s wrapper ([`reference_accepts_wrapper`]'s
/// closed table, by class code).
pub fn vh_site_class_accepts_primitive(class: u8, p: u8) -> bool {
    use cratonvm_native_api::poly_call_site::vh_site;
    match class {
        vh_site::OBJECT | vh_site::SERIALIZABLE | vh_site::COMPARABLE | vh_site::CONSTABLE => true,
        vh_site::NUMBER => matches!(p, b'B' | b'S' | b'I' | b'J' | b'F' | b'D'),
        vh_site::CONSTANT_DESC => matches!(p, b'I' | b'J' | b'F' | b'D'),
        // A wrapper site takes exactly its own primitive's box; any other
        // class (and every array type) takes none.
        _ => vh_site::wrapper_prim(class) == Some(p),
    }
}

/// The door half of [`vh_astype_refused`]: when `failed` is the exception a
/// VarHandle native refused with and left a record for, rewrite its message
/// with the access mode type of `method_name` and the call site's
/// `call_site` descriptor, each with the leading `VarHandle` the invoker's
/// type carries, as `MethodType.toString` prints them (simple names). Every
/// other failure, and every way of not knowing, returns `failed` unchanged.
///
/// For the doors that armed the parameter-site word and hold the real
/// call-site descriptor: the interpreter's signature-polymorphic block, its
/// site-cache replay, and the JIT's site-cached native dispatch. Allocates
/// (the new text); the exception is pinned across it.
#[cold]
#[inline(never)]
pub fn varhandle_astype_refusal_with_site(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    method_name: &str,
    call_site: &str,
) -> MethodCallFailed {
    let MethodCallFailed::ExceptionThrown(exception) = &failed else {
        return failed;
    };
    let exception = *exception;
    let Some(record) = cratonvm_native_api::poly_call_site::take_vh_astype_refusal(exception)
    else {
        return failed;
    };
    let Some(access_type) = vh_access_type_of_method(method_name) else {
        return failed;
    };
    let own = vh_access_mode_descriptor(access_type, &record.coordinates, &record.value);
    let shown = vh_invoker_descriptor(&own)
        .and_then(|d| method_type_display_in(&*ctx, &d))
        .zip(vh_invoker_descriptor(call_site).and_then(|d| method_type_display_in(&*ctx, &d)));
    let Some((own, site)) = shown else {
        return failed;
    };
    let text = format!("cannot convert MethodHandle{own} to {site}");
    let pin = ctx.pin_native_root(exception);
    let detail = ctx.create_string_uninterned(&text);
    let exception = ctx.read_native_pin(pin, exception);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(exception, "detailMessage", Value::Object(Some(detail)));
    MethodCallFailed::ExceptionThrown(exception)
}

/// ONE value parameter of a VarHandle access on a PRIMITIVE variable of type
/// `p`, adapted as `asType` would: see the block comment above.
pub fn vh_adapt_value(
    ctx: &mut dyn NativeContext,
    p: u8,
    value: Value,
    site: u8,
) -> Result<Value, MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::vh_site;
    match value {
        Value::Object(maybe) => {
            if vh_site_admits_primitive(site, p) == Some(false) {
                return Err(vh_value_wmte(ctx, value, p));
            }
            let Some(o) = maybe else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!(
                        "Cannot unbox a null value into a VarHandle variable of type {}",
                        vh_prim_java_name(p)
                    )),
                }
                .into());
            };
            if ctx.object_is_array(o) {
                return Err(RuntimeError::ClassCastException {
                    message: format!(
                        "Cannot cast {} to java.lang.Number",
                        entry_value_type_display(&*ctx, o)
                    ),
                }
                .into());
            }
            let cid = ctx.class_id_of_object(o);
            let name = ctx.class_name_of_id(cid).unwrap_or_default();
            let Some(wdesc) = crate::lang_class::wrapper_to_prim_desc(&name) else {
                return Err(RuntimeError::ClassCastException {
                    message: format!("Cannot cast {} to java.lang.Number", name.class_text(&*ctx)),
                }
                .into());
            };
            let w = wdesc.as_bytes()[0];
            if !primitive_widens(w, p) {
                return Err(RuntimeError::ClassCastException {
                    message: format!(
                        "Cannot cast {} to {}",
                        name.class_text(&*ctx),
                        vh_prim_wrapper_display(p)
                    ),
                }
                .into());
            }
            let raw = crate::lang_class::coerce_reflective_field_value(ctx.get_field(o, 0), wdesc);
            // A wrapper whose payload does not read as its own kind is not
            // one this can convert; keep the reference and let the store
            // arms below report it, which is today's behaviour for it.
            Ok(vh_widen_primitive(raw, w, p, true).unwrap_or(value))
        }
        Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
            let src = if vh_site::is_primitive(site) {
                if !primitive_widens(site, p) {
                    return Err(vh_value_wmte(ctx, value, p));
                }
                site
            } else {
                match value {
                    Value::Int(_) => b'I',
                    Value::Long(_) => b'J',
                    Value::Float(_) => b'F',
                    _ => b'D',
                }
            };
            Ok(vh_widen_primitive(value, src, p, false).unwrap_or(value))
        }
        other => Ok(other),
    }
}

/// The primitive type of the variable a VarHandle access touches, or `None`
/// for a reference variable and for every handle shape this does not adapt
/// (byte-array/`ByteBuffer` views, FFM layout and segment handles).
pub fn vh_primitive_var_desc(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    args: &[Value],
) -> Option<u8> {
    match meta {
        Some(m) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC => {
            primitive_char(&m.field_desc)
        }
        // A byte-array or `ByteBuffer` view (round 15 wave 3, lane vh4): its
        // variable is the view's primitive (`field_desc`), and `asType`
        // converts the VALUE to it like any other. Before, the raw value went
        // to `byte_view_encode`/`byte_view_set`, which encode its BITS: an
        // `int` into a `float` view stored `Float.intBitsToFloat(5)` for `5`,
        // a boxed `Integer` through an `Object` site stored `0`, and a `long`
        // into an `int` view stored its low half where HotSpot throws
        // `WrongMethodTypeException`.
        Some(m)
            if matches!(
                m.kind,
                VH_KIND_BYTE_VIEW_LE
                    | VH_KIND_BYTE_VIEW_BE
                    | VH_KIND_BYTE_BUFFER_VIEW_LE
                    | VH_KIND_BYTE_BUFFER_VIEW_BE
            ) =>
        {
            if vh_view_value_adapt_enabled() {
                primitive_char(&m.field_desc)
            } else {
                None
            }
        }
        Some(m) if m.kind != VH_KIND_ARRAY => None,
        // An array-element handle: our ARRAY kind, or a real-JDK
        // `VarHandle*$Array` with no side-table entry. The shape test is
        // `vh_array_call`'s, minus the meta read already made here.
        _ => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return None,
            };
            if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array)
                || !matches!(args.get(2), Some(Value::Int(_)))
            {
                return None;
            }
            primitive_char(array_element_desc(&*ctx, arr))
        }
    }
}

/// Adapt the trailing `n_values` value parameters of a VarHandle store-mode
/// call (`set*`: 1, `getAndSet*`/`getAndAdd*`/`getAndBitwise*`: 1,
/// `compareAnd*`/`weakCompareAndSet*`: 2) for a PRIMITIVE variable, using the
/// call-site classes `site` the door armed (`take_vh_value_site`, low byte =
/// last parameter, high byte = the one before it).
///
/// Borrows `args` unchanged — no allocation — for a reference variable, for a
/// shape it does not adapt, and whenever every value already has the
/// variable's own kind, which is every exactly-typed call site.
pub fn vh_adapt_value_args<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &'a [Value],
    n_values: usize,
    site: u16,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    vh_adapt_value_args_row(ctx, this, args, n_values, site, &None)
}

/// The variable type [`vh_check_value_site_row`] judges the values against
/// (JIT round 21, lane vhmerge;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// That check runs BEFORE the array coordinate's cast, so for an
/// array-element handle the variable type must be the HANDLE's declared
/// component type (`asType` judges the method type, which knows nothing of
/// the run-time array), not the element type of whatever array the caller
/// passed. [`vh_primitive_var_desc`] reads the run-time array: correct after
/// the cast (the two agree whenever the cast admits), wrong before it, where
/// `VZ.getAndBitwiseOr((Object) ints, 0, true)` judged `true` against `int`
/// and threw `WrongMethodTypeException` instead of HotSpot's
/// `ClassCastException` (`R20FuzzGVhArrayAll1` refuse code 62 -> 88,
/// `R20FuzzIVhArr1` 22222 CCE -> WMTE). The declared type comes from the
/// same place the cast reads it (the side-table `class_name`, or the real-JDK
/// handle's first coordinate); when it is unknown, or the cast is switched
/// off, the run-time answer stands as before.
pub fn vh_static_value_var_desc(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    args: &[Value],
) -> Option<u8> {
    let array_arm = match meta {
        Some(m) => m.kind == VH_KIND_ARRAY,
        // A real-JDK handle has no row; an FFM one's second coordinate is a
        // `long` offset (the cast's own early-out).
        None => !matches!(
            args.get(2),
            Some(Value::Long(_) | Value::Float(_) | Value::Double(_))
        ),
    };
    if array_arm && vh_array_coordinate_cast_enabled() {
        let real: Option<Arc<str>>;
        let declared: Option<&str> = match meta {
            Some(m) => Some(m.class_name.as_str()),
            None => {
                real = vh_real_array_coordinate_desc(&*ctx, this);
                real.as_deref()
            }
        };
        if let Some(d) = declared.and_then(|d| d.strip_prefix('[')) {
            // `[I` -> `I`; a reference or nested array component is not a
            // primitive variable, so no primitive value check applies.
            return primitive_char(d);
        }
    }
    vh_primitive_var_desc(ctx, meta, args)
}

/// Check static value conversions before executing coordinate casts or null
/// checks. This does not unbox values: runtime conversion still follows the
/// coordinate adapter, while an impossible method type wins before either.
pub fn vh_check_value_site_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    n_values: usize,
    site: u16,
    row: &VhPrologueRow,
) -> Result<(), MethodCallFailed> {
    if n_values == 0 || args.len() < 1 + n_values {
        return Ok(());
    }
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(());
    };
    let meta = vh_row_or_read(ctx, this, row);
    let Some(p) = vh_static_value_var_desc(ctx, this, meta.as_deref(), args) else {
        return Ok(());
    };
    let first = args.len() - n_values;
    let site_of = |i: usize| -> u8 {
        match args.len() - 1 - i {
            0 => (site & 0xff) as u8,
            1 => (site >> 8) as u8,
            _ => 0,
        }
    };
    // `asType` runs `MethodType.canConvert` over EVERY parameter before it
    // converts any value, so a static refusal of the second value wins over a
    // run-time refusal of the first: `compareAndSet(h, (Object) null,
    // (String) s)` on an `int` field is WMTE, not the first value's NPE.
    // `vh_adapt_value` repeats each check; this pass only fixes the ORDER.
    for (i, v) in args.iter().enumerate().skip(first) {
        let s = site_of(i);
        let refused = match v {
            Value::Object(_) => vh_site_admits_primitive(s, p) == Some(false),
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                cratonvm_native_api::poly_call_site::vh_site::is_primitive(s)
                    && !primitive_widens(s, p)
            }
            _ => false,
        };
        if refused {
            let v = *v;
            return Err(vh_astype_refused(ctx, this, |ctx| vh_value_wmte(ctx, v, p)));
        }
    }
    Ok(())
}

/// [`vh_adapt_value_args`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]).
pub fn vh_adapt_value_args_row<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &'a [Value],
    n_values: usize,
    site: u16,
    row: &VhPrologueRow,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    if n_values == 0 || args.len() < 1 + n_values {
        return Ok(Cow::Borrowed(args));
    }
    // "Already the variable's own kind" needs the variable's type, so the
    // type is asked first. `vh_meta_get` is a thread-local memo hit on the
    // hot path, and a reference variable returns right after it.
    let meta = vh_row_or_read(ctx, this, row);
    let Some(p) = vh_primitive_var_desc(ctx, meta.as_deref(), args) else {
        // A REFERENCE variable given a primitive value boxes it
        // (`vh_box_value_args`); everything else is borrowed unchanged.
        return vh_box_value_args(ctx, this, meta.as_deref(), args, n_values, site);
    };
    let first = args.len() - n_values;
    let natural = |v: &Value| match p {
        b'J' => matches!(v, Value::Long(_)),
        b'F' => matches!(v, Value::Float(_)),
        b'D' => matches!(v, Value::Double(_)),
        _ => matches!(v, Value::Int(_)),
    };
    // A primitive site that NARROWS (`set(h, 5)` on a `byte` field) arrives
    // in the natural `Int` representation, so a known site is judged even
    // then; an unknown one cannot be.
    let site_of = |i: usize| -> u8 {
        let from_end = args.len() - 1 - i;
        match from_end {
            0 => (site & 0xff) as u8, // Cast: low byte = last parameter
            1 => (site >> 8) as u8,   // Cast: high byte = the one before it
            _ => 0,
        }
    };
    let needs = (first..args.len()).any(|i| {
        !natural(&args[i]) || {
            let s = site_of(i);
            cratonvm_native_api::poly_call_site::vh_site::is_primitive(s) && s != p
        }
    });
    if !needs {
        return Ok(Cow::Borrowed(args));
    }
    vh_check_value_site_row(ctx, args, n_values, site, row)?;
    let mut out = args.to_vec();
    for (i, slot) in out.iter_mut().enumerate().skip(first) {
        *slot = vh_adapt_value(ctx, p, *slot, site_of(i))?;
    }
    Ok(Cow::Owned(out))
}

// ---------------------------------------------------------------------------
// VarHandle value adaptation, REFERENCE variable: `asType` BOXES a primitive
// value (round 11 wave 17, lane vh)
// ---------------------------------------------------------------------------
//
// javac does not autobox the argument of a signature-polymorphic call, so
// `O.set(h, 5)` on an `Object` field is `invokevirtual VarHandle.set:(LH;I)V`
// and the native receives `Value::Int(5)`. HotSpot's `asType` boxes it
// (`Integer.valueOf(5)`) when `MethodType.canConvert(int, R)` holds -- when
// the variable's declared type `R` is a supertype of the wrapper -- and throws
// `WrongMethodTypeException` otherwise (`int` into a `String` or `Long`
// field). CratonVM stored the raw `Int` and `coerce_field_value_for_slot`
// turned it into `null` at the reference slot
// (r11w16-vh-primitive-into-reference-variable-stores-null).
//
// Boxing ALLOCATES, and a moving collection there would leave every reference
// in `args` -- the handle, the receiver or array, a reference expected value --
// stale in the copy handed back. So every reference argument and every box is
// rooted in a `NativeHandleScope` and the returned vector is rebuilt from the
// handles after the LAST allocation. The callers re-read the handle from the
// returned vector for the same reason.

/// The primitive descriptor string of a descriptor byte, for the boxing call.
pub fn vh_prim_desc_str(p: u8) -> &'static str {
    match p {
        b'Z' => "Z",
        b'B' => "B",
        b'C' => "C",
        b'S' => "S",
        b'J' => "J",
        b'F' => "F",
        b'D' => "D",
        _ => "I",
    }
}

/// `MethodType.canConvert(p, R)` for a PRIMITIVE value `p` and a REFERENCE
/// variable declared `decl` (`L..;` or `[..`): `R.isAssignableFrom(wrapper(p))`.
///
/// Every wrapper is an `Object`, a `Serializable`, a `Comparable` and (JDK 15+)
/// a `Constable`; all but `Boolean` and `Character` are `Number`s; only
/// `Integer`, `Long`, `Float` and `Double` are `ConstantDesc`s. The same table
/// as [`vh_site_admits_primitive`], read from the other side.
pub fn vh_reference_admits_primitive(decl: &str, p: u8) -> bool {
    if decl.starts_with('[') {
        return false;
    }
    let name = decl
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(decl);
    match name {
        "java/lang/Object"
        | "java/io/Serializable"
        | "java/lang/Comparable"
        | "java/lang/constant/Constable" => true,
        "java/lang/Number" => !matches!(p, b'Z' | b'C'),
        "java/lang/constant/ConstantDesc" => matches!(p, b'I' | b'J' | b'F' | b'D'),
        other => crate::lang_class::wrapper_to_prim_desc(other)
            .is_some_and(|w| w.as_bytes().first() == Some(&p)),
    }
}

/// The declared type of the REFERENCE variable a VarHandle access touches.
///
/// `Some(Some(desc))`: a reference variable declared `desc`. `Some(None)`: an
/// array-element handle over a reference array whose handle carries no
/// readable `componentType` (the static check is then skipped, and the store
/// refusals judge the box as they judge any reference). `None`: a primitive
/// variable, or a shape this does not adapt (views, FFM handles, a handle with
/// no side-table entry that is not an array-element access).
pub fn vh_reference_var_desc(
    ctx: &mut dyn NativeContext,
    meta: Option<&VarHandleMeta>,
    this: ObjectRef,
    args: &[Value],
) -> Option<Option<String>> {
    match meta {
        Some(m) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC => {
            match m.field_desc.as_bytes().first() {
                Some(b'L' | b'[') => Some(Some(m.field_desc.clone())),
                _ => None,
            }
        }
        Some(m) if m.kind != VH_KIND_ARRAY => None,
        // The array-element shape test of `vh_primitive_var_desc`.
        _ => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return None,
            };
            if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array)
                || !matches!(args.get(2), Some(Value::Int(_)))
                || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
            {
                return None;
            }
            // The HANDLE's component type decides `asType`, not the array's
            // run-time one (an `Object[]` handle over a `String[]` boxes, and
            // the store then fails with `ArrayStoreException`).
            let declared = match ctx.get_field_by_name(this, "componentType") {
                Value::Object(Some(c)) => mirror_class_name(&*ctx, c).map(|n| {
                    if n.starts_with('[') {
                        n
                    } else {
                        format!("L{n};")
                    }
                }),
                _ => None,
            };
            Some(declared)
        }
    }
}

/// Adapt the trailing `n_values` value parameters of a VarHandle store-mode
/// call for a REFERENCE variable: a primitive value is boxed as `asType`
/// boxes it, after the static `canConvert` check. See the block comment above.
///
/// Borrows `args` unchanged -- no allocation -- when no value is primitive,
/// which is every reference call site, and for every shape that is not a
/// reference variable.
pub fn vh_box_value_args<'a>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    args: &'a [Value],
    n_values: usize,
    site: u16,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::vh_site;
    let first = args.len() - n_values;
    let is_prim = |v: &Value| {
        matches!(
            v,
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)
        )
    };
    if !args[first..].iter().any(is_prim) {
        return Ok(Cow::Borrowed(args));
    }
    let Some(declared) = vh_reference_var_desc(ctx, meta, this, args) else {
        return Ok(Cow::Borrowed(args));
    };
    // The primitive TYPE of each value: the armed site's, else the `Value`
    // variant's (`Int` is `int`, right for every `int` site and wrong only for
    // an unarmed `char`/`boolean`/`byte`/`short` one), and the value in that
    // type's own representation.
    let site_of = |i: usize| -> u8 {
        match args.len() - 1 - i {
            0 => (site & 0xff) as u8, // Cast: low byte = last parameter
            1 => (site >> 8) as u8,   // Cast: high byte = the one before it
            _ => 0,
        }
    };
    let typed = |i: usize, v: Value| -> (u8, Value) {
        let from = match v {
            Value::Long(_) => b'J',
            Value::Float(_) => b'F',
            Value::Double(_) => b'D',
            _ => b'I',
        };
        let s = site_of(i);
        if vh_site::is_primitive(s) {
            (s, vh_widen_primitive(v, from, s, false).unwrap_or(v))
        } else {
            (from, v)
        }
    };
    // Static pass first, over every value: `asType` refuses before it boxes.
    if let Some(decl) = declared.as_deref() {
        for (i, v) in args.iter().enumerate().skip(first) {
            if !is_prim(v) {
                continue;
            }
            let (p, _) = typed(i, *v);
            if !vh_reference_admits_primitive(decl, p) {
                let text = format!(
                    "cannot convert {} to {}",
                    vh_prim_java_name(p),
                    jvm_type_display(decl)
                );
                return Err(vh_astype_refused(ctx, this, |ctx| {
                    crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/invoke/WrongMethodTypeException",
                        &text,
                    )
                }));
            }
        }
    }
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let mut rooted: Vec<Option<cratonvm_native_api::NativeHandle>> = Vec::with_capacity(args.len());
    for v in args {
        rooted.push(match v {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        });
    }
    let mut boxes: Vec<(usize, cratonvm_native_api::NativeHandle)> = Vec::with_capacity(n_values);
    for (i, v) in args.iter().enumerate().skip(first) {
        if !is_prim(v) {
            continue;
        }
        let (p, typed_value) = typed(i, *v);
        // `valueOf`, so the canonical caches apply, as they do to `asType`'s
        // boxing on HotSpot.
        let boxed = box_value_canonical(&mut *scope, typed_value, vh_prim_desc_str(p));
        if let Value::Object(Some(b)) = boxed {
            let h = scope.root(b);
            boxes.push((i, h));
        }
    }
    // After the last allocation: every reference re-read through its handle.
    let mut out = args.to_vec();
    for (i, h) in rooted.iter().enumerate() {
        if let Some(h) = h {
            out[i] = Value::Object(Some(scope.get(h)));
        }
    }
    for (i, h) in &boxes {
        out[*i] = Value::Object(Some(scope.get(h)));
    }
    Ok(Cow::Owned(out))
}

/// The element type a ONE-dimensional primitive array descriptor (`[I`, `[B`,
/// ...) names, or `None` for every other descriptor — including `[[I`, whose
/// elements are references.
pub fn one_dim_primitive_array_element(desc: &str) -> Option<ArrayElementType> {
    let b = desc.as_bytes();
    if b.len() != 2 || b[0] != b'[' {
        return None;
    }
    Some(match b[1] {
        b'Z' => ArrayElementType::Boolean,
        b'C' => ArrayElementType::Char,
        b'F' => ArrayElementType::Float,
        b'D' => ArrayElementType::Double,
        b'B' => ArrayElementType::Byte,
        b'S' => ArrayElementType::Short,
        b'I' => ArrayElementType::Int,
        b'J' => ArrayElementType::Long,
        _ => return None,
    })
}

/// May the non-null `value` be stored into a field declared with the ARRAY
/// descriptor `field_desc`? Same contract as [`reference_arg_admitted`]:
/// `Some(false)` is a positive mismatch, `None` is "cannot tell, do not refuse".
///
/// Array-ness comes from the heap, not the class name — a CratonVM array's
/// `class_id_of_object` is its COMPONENT class (see `entry_value_casts_to`,
/// which settles the same shape question the same way). What the shape
/// settles on its own:
///
/// * a non-array is never an array;
/// * a one-dimensional primitive array field admits exactly that array;
/// * a primitive array is never a reference array (`Object[]`, `String[]`) nor
///   a multi-dimensional one (`int[]` is not an `int[][]`).
///
/// A reference array into a reference-array field (`Integer[]` into a
/// `String[]` field) needs its component class, so the VM judges it with
/// `checkcast`'s own predicate (`NativeClassAccess::array_value_casts_to_desc`,
/// the interpreter's `array_is_assignable_to`) — a `VarHandle` set is
/// `asType` plus a cast to the field type, so it refuses exactly what
/// `checkcast` refuses (an `Object[]` is no `T[]` in any mode since
/// i10-L2). A context that cannot answer (`None`, the mocks) still fails open
/// (r11w7-rt-varhandle-ref-array-field-check-patch).
pub fn vh_array_field_value_admitted(
    ctx: &dyn NativeContext,
    value: ObjectRef,
    field_desc: &str,
) -> Option<bool> {
    vh_array_field_value_admitted_with(ctx, value, field_desc, |v, d| {
        ctx.array_value_casts_to_desc(v, d)
    })
}

/// [`vh_array_field_value_admitted`] with the reference-array verdict passed
/// in, so a test can drive the `Some(false)` arm without a VM behind the
/// context. Production passes `NativeClassAccess::array_value_casts_to_desc`.
pub fn vh_array_field_value_admitted_with(
    ctx: &dyn NativeContext,
    value: ObjectRef,
    field_desc: &str,
    reference_array_casts: impl FnOnce(ObjectRef, &str) -> Option<bool>,
) -> Option<bool> {
    if !field_desc.starts_with('[') {
        return None;
    }
    if !ctx.object_is_array(value) {
        return Some(false);
    }
    let have = ctx.heap_element_type_of(value);
    match one_dim_primitive_array_element(field_desc) {
        Some(want) => Some(have == want),
        None if have == ArrayElementType::Reference => reference_array_casts(value, field_desc),
        None => Some(false),
    }
}

/// The exception an array-element `VarHandle` store of `value` into `arr`
/// raises, or `None` when the store may proceed
/// (r11w4-rt-varhandle-array-element-stores-unchecked).
///
/// The JDK's `VarHandleReferences.Array` makes two checks, and this VM never
/// ran either, so an `Integer` could be stored into a `String[]`:
///
/// 1. the invocation's `asType` casts the value to the handle's own
///    `componentType`, which gives a `ClassCastException`;
/// 2. `runtimeTypeCheck` checks the value against the ARRAY's actual
///    component (a `String[]` seen through an `Object[]` handle), which gives
///    an `ArrayStoreException`.
///
/// Both refuse only on a positive answer. Check 1 goes through
/// [`reference_arg_admitted`]. Check 2 goes through
/// `aastore_element_assignable`, the predicate the interpreter's `aastore`
/// uses, which must never produce a false `ArrayStoreException`. A primitive
/// array, a null value, and a handle with no readable `componentType` (our
/// synthetic layout) skip the check that does not apply.
pub fn vh_array_store_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    value: Value,
) -> Option<MethodCallFailed> {
    vh_array_store_refusal_route(ctx, vh, arr, value, false)
}

/// [`vh_array_store_refusal`] with the store's road: `aastore` is plain
/// `set` (an `aastore` in the JDK body, so a class-name message), `false`
/// every `runtimeTypeCheck` mode ([`vh_array_store_check_failure`]).
pub fn vh_array_store_refusal_route(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    value: Value,
    aastore: bool,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(stored)) = value else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    if let Some(refusal) = vh_array_component_cast_refusal(ctx, vh, stored) {
        // Wave 26 (lane vhase): a `runtimeTypeCheck` mode on an array that is
        // not exactly the handle's `arrayType` takes the JDK's slow path,
        // `reflectiveTypeCheck` against the array's REAL component. That
        // component is a subtype of the handle's (the coordinate cast held),
        // so a value the handle's component refuses is refused there too:
        // `new ArrayStoreException()`, not the fast path's `Class.cast` CCE.
        if !aastore && vh_array_covariant_slow_path(ctx, vh, arr) {
            return Some(vh_array_store_check_failure(&*ctx, stored, false));
        }
        return Some(refusal);
    }
    if ctx.aastore_element_assignable(arr, stored) == Some(false) {
        return Some(vh_array_store_check_failure(&*ctx, stored, aastore));    }
    None
}

/// `CRATONVM_VH_ARRAY_HIDDEN_COMPONENT_CAST` (default ON, both modes; JIT
/// round 21 wave 26, lane vhase): a HIDDEN `componentType` cannot be resolved
/// by name, so `reference_arg_admitted` answered `None` (admit) and plain
/// `set` of a `String` through `arrayElementVarHandle(H[].class)` fell to the
/// `aastore` check (`ArrayStoreException: java.lang.String`). HotSpot's
/// `componentType.cast(value)` throws `ClassCastException: Cannot cast
/// java.lang.String to H` first (`R21Fz10VhCovariant1`, `HV/covH set
/// String`). The verdict is taken on the mirror's class id instead: a hidden
/// class is never named by another class's `extends` / `implements`, so its
/// only instances are its own and those of subclasses `is_subclass` knows.
/// `0` restores the admit.
pub fn vh_array_hidden_component_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether `value` is an instance of the HIDDEN class the mirror `component`
/// reflects: `Some(verdict)`, or `None` when the switch is off, the mirror
/// names no class id, or that class is not hidden (the name path decides).
pub fn vh_hidden_component_admits(
    ctx: &dyn NativeContext,
    component: ObjectRef,
    value: ObjectRef,
) -> Option<bool> {
    if !vh_array_hidden_component_cast_enabled() {
        return None;
    }
    let cid = mirror_class_id(ctx, component)?;
    if !ctx.is_class_hidden(cid) {
        return None;
    }
    if ctx.object_is_array(value) {
        return Some(false);
    }
    let vid = ctx.class_id_of_object(value);
    Some(vid == cid || ctx.is_subclass(vid, cid))
}

/// `CRATONVM_VH_ARRAY_COVARIANT_SLOW_PATH` (default ON, both modes; JIT round
/// 21 wave 26, lane vhase): JDK 25 `VarHandleReferences.Array.runtimeTypeCheck`
/// casts the value to the handle's `componentType` (a `ClassCastException`)
/// only when `handle.arrayType == oarray.getClass()`; otherwise it runs
/// `reflectiveTypeCheck`, whose failure is a message-less
/// `ArrayStoreException`. HotSpot 25.0.3 measured (`R21Fz10VhCovariant1`,
/// `CS/covCS {setVolatile,getAndSet,CAS} Integer`: an `Integer` through a
/// `CharSequence[]` handle into a `String[]`): `ArrayStoreException()`, where
/// CratonVM threw `Cannot cast java.lang.Integer to java.lang.CharSequence`.
/// Plain `set` always casts (`componentType.cast(value)`), so it is not
/// switched. `0` restores the CCE on every array.
pub fn vh_array_covariant_slow_path_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether `arr` is positively NOT of the handle's exact `arrayType`
/// (`runtimeTypeCheck`'s slow path), under
/// [`vh_array_covariant_slow_path_enabled`]. Decided on the heap header (a
/// reference array's header carries its COMPONENT's class id) against the
/// handle's `componentType` mirror. Answers `false` (the fast path, the old
/// behaviour) whenever it cannot be sure: no readable mirror or class id, a
/// primitive component, or a header id of `0`. An array component is
/// compared by name ([`vh_array_name_is_covariant`]; round 22 wave 1).
pub fn vh_array_covariant_slow_path(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
) -> bool {
    if !vh_array_covariant_slow_path_enabled() {
        return false;
    }
    let Value::Object(Some(component)) = ctx.get_field_by_name(vh, "componentType") else {
        return false;
    };
    let Some(name) = mirror_class_name(&*ctx, component) else {
        return false;
    };
    if matches!(
        name.as_str(),
        "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short"
    ) {
        return false;
    }
    if name.starts_with('[') {
        // JIT round 22 wave 1, lane review (same switch): an ARRAY component
        // (an `Object[][]` handle's `Object[]`) is decided by name, since the
        // header's id is then the component ARRAY class's. A `String[][]`
        // through an `Object[][]` handle takes `reflectiveTypeCheck` too, so
        // an `Integer` stored by `setVolatile` / `getAndSet` / CAS is the
        // message-less `ArrayStoreException`, not check 1's `Cannot cast
        // java.lang.Integer to [Ljava.lang.Object;`. Different names prove
        // different classes; equal names keep the fast path, as before.
        if ctx.class_id_of_object(arr).as_u32() == 0
            || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
        {
            return false;
        }
        let have = crate::lang_class::array_class_internal_name(&*ctx, arr);
        return vh_array_name_is_covariant(&have, &name);
    }
    let Some(want) = mirror_class_id(&*ctx, component) else {
        return false;
    };
    let have = ctx.class_id_of_object(arr);
    vh_array_header_is_covariant(have.as_u32(), want.as_u32())
}

/// [`vh_array_covariant_slow_path`]'s verdict on raw ids: a known (non-zero)
/// header component that differs from the handle's component.
pub fn vh_array_header_is_covariant(header_component: u32, handle_component: u32) -> bool {
    header_component != 0 && header_component != handle_component
}

/// [`vh_array_covariant_slow_path`]'s verdict for an ARRAY component (JIT
/// round 22 wave 1, lane review): the array's own class name `array_name`
/// (`[[Ljava/lang/String;`) is not the handle's `arrayType`, `[` + the
/// component's name. Either spelling of the separators is accepted.
pub fn vh_array_name_is_covariant(array_name: &str, handle_component: &str) -> bool {
    let have = array_name.replace('.', "/");
    let want = format!("[{}", handle_component.replace('.', "/"));
    have != want
}

/// `CRATONVM_VH_ARRAY_RUNTIME_TYPE_CHECK_NULL_MESSAGE` (default ON, both
/// modes; JIT round 21 wave 25, lane vhase): the `ArrayStoreException` of a
/// reference-array `VarHandle` store refused by the array's OWN component
/// carries HotSpot's message. JDK 25 `VarHandleReferences.Array` has two
/// roads: plain `set` is `array[index] = handle.componentType.cast(value)`,
/// an `aastore`, whose refusal names the value's class (`java.lang.Integer`);
/// every other store mode (`setVolatile` / `setOpaque` / `setRelease`, the
/// `compareAndSet` / `compareAndExchange*` / `weakCompareAndSet*` family and
/// `getAndSet*`) passes the value through `runtimeTypeCheck` ->
/// `reflectiveTypeCheck`, which catches the `Class.cast` failure and throws
/// `new ArrayStoreException()`: a NULL message (measured, HotSpot 25.0.3,
/// `R21VhConstRefRows` cold rows). `0` restores the class name on every mode.
pub fn vh_array_runtime_type_check_null_message_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The `ArrayStoreException` a reference-array `VarHandle` store of `stored`
/// owes once the array's own component refused it: `aastore` (plain `set`)
/// names the value's class; the `runtimeTypeCheck` modes have no message
/// under [`vh_array_runtime_type_check_null_message_enabled`].
pub fn vh_array_store_check_failure(
    ctx: &dyn NativeContext,
    stored: ObjectRef,
    aastore: bool,
) -> MethodCallFailed {
    vh_array_store_check_error(
        aastore,
        vh_array_runtime_type_check_null_message_enabled(),
        || entry_value_type_display(ctx, stored),
    )
    .into()
}

/// [`vh_array_store_check_failure`]'s decision, the switch passed in. An
/// EMPTY `ArrayStoreException` message is a NULL `getMessage()`
/// (`RuntimeError::as_java_throwable`).
pub fn vh_array_store_check_error(
    aastore: bool,
    null_message: bool,
    class_text: impl FnOnce() -> String,
) -> RuntimeError {
    let message = if aastore || !null_message {
        class_text()
    } else {
        String::new()
    };
    RuntimeError::ArrayStoreException { message }
}

/// The `compareAndSet` / `compareAndExchange` EXPECTED value's half of
/// [`vh_array_store_refusal`] (round 15 wave 4, lane vh5): `asType` casts it
/// to the handle's `componentType` too (`VarHandleReferences.Array` also
/// casts it in the body), so a wrong-typed expectation is
/// `ClassCastException` on HotSpot, where CratonVM compared it and answered
/// `false` / the witness. Check 1 only: `runtimeTypeCheck` is the stored
/// value's. Before check 1 of the NEW value, in parameter order. Under
/// `CRATONVM_VH_ARRAY_COORDINATE_CAST`.
pub fn vh_array_expected_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    expected: Value,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(expected)) = expected else {
        return None;
    };
    if !vh_array_coordinate_cast_enabled()
        || ctx.heap_element_type_of(arr) != ArrayElementType::Reference
    {
        return None;
    }
    vh_array_component_cast_refusal(ctx, vh, expected)
}

/// `CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX` (default on, both modes; round 15
/// wave 5, lane vh7; F3 of `r15w4-vh5-review-findings-CLOSED-20260930.md`): the
/// reference array `set` arm runs the stored value's cast to the handle's
/// `componentType` (check 1 of [`vh_array_store_refusal`]) BEFORE its index
/// check, and only the `ArrayStoreException` check after it. HotSpot 25
/// measured (`R15Vh7ArrayCastOrder`): `set` casts first (`setOob` CCE), while
/// `compareAndSet`, `compareAndExchange` and `getAndSet` check the index
/// first (AIOOBE), so those arms keep index-first and are not switched. `0`
/// restores index-first for `set` too (`SA.set((Object) strs, 99, (Object)
/// 1)`: `ArrayIndexOutOfBoundsException` instead of HotSpot's
/// `ClassCastException`).
pub fn vh_array_cast_before_index_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX_SET_ONLY` (default ON, both modes;
/// JIT round 21 wave 26, lane vhase): [`vh_array_cast_before_index_enabled`]
/// applies to plain `set` ONLY. `varhandle_set_mode` serves all four set
/// modes, so the cast-first order also reached `setVolatile` / `setRelease` /
/// `setOpaque`, whose JDK 25 bodies (`VarHandleReferences.Array`) are
/// `UNSAFE.putReference*(array, (long) Preconditions.checkIndex(..) <<
/// ashift + abase, runtimeTypeCheck(handle, array, value))`: Java evaluates
/// the offset argument (the index check) BEFORE the value's cast. Plain `set`
/// is `array[index] = handle.componentType.cast(value)`, whose RHS runs
/// before the `aastore`'s own bounds check, so it alone casts first.
/// HotSpot 25.0.3 measured (`R21Fz9VhArrRef1`, `wrong@-1` rows): `set` CCE,
/// `setVolatile` / `setRelease` / `setOpaque` AIOOBE. `0` restores cast-first
/// on all four set modes.
pub fn vh_array_cast_before_index_set_only_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether the set arm serving `mode` casts the value BEFORE its index check.
pub fn vh_array_mode_casts_before_index(mode: &str) -> bool {
    vh_array_mode_casts_before_index_with(
        mode,
        vh_array_cast_before_index_enabled(),
        vh_array_cast_before_index_set_only_enabled(),
    )
}

/// [`vh_array_mode_casts_before_index`] with both switches passed in.
pub fn vh_array_mode_casts_before_index_with(mode: &str, cast_first: bool, set_only: bool) -> bool {
    cast_first && (mode == "set" || !set_only)
}

/// The refusal the reference array `set` arm owes BEFORE its index check
/// under [`vh_array_mode_casts_before_index`]: the stored value's cast to
/// the handle's `componentType`. `None` with the switch off, for a mode that
/// checks the index first, for a primitive array, and for values the cast
/// admits.
pub fn vh_array_casts_before_index(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    stored: Value,
    mode: &str,
) -> Option<MethodCallFailed> {
    if !vh_array_mode_casts_before_index(mode) {
        return None;
    }
    let Value::Object(Some(stored)) = stored else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    vh_array_component_cast_refusal(ctx, vh, stored)
}

/// What the reference array `set` arm checks AFTER its index check: all of
/// [`vh_array_store_refusal`], or, when [`vh_array_casts_before_index`]
/// already ran the cast, only check 2 (`ArrayStoreException`: for plain `set`
/// worded with the stored value's class name, as HotSpot's `aastore`:
/// `java.lang.Integer`; for `setVolatile` / `setOpaque` / `setRelease` no
/// message, [`vh_array_store_check_failure`]).
pub fn vh_array_refusal_after_index(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    arr: ObjectRef,
    stored: Value,
    mode: &str,
) -> Option<MethodCallFailed> {
    // Wave 25 (lane vhase): only plain `set` is an `aastore` in the JDK body;
    // `setVolatile` / `setOpaque` / `setRelease` go through `runtimeTypeCheck`
    // (a null-message `ArrayStoreException`).
    let aastore = mode == "set";
    // Wave 26 (lane vhase): a mode that did not cast before the index runs
    // the whole refusal (cast, then the array's own component) here.
    if !vh_array_mode_casts_before_index(mode) {
        return vh_array_store_refusal_route(ctx, vh, arr, stored, aastore);
    }
    let Value::Object(Some(stored)) = stored else {
        return None;
    };
    if ctx.heap_element_type_of(arr) != ArrayElementType::Reference {
        return None;
    }
    if ctx.aastore_element_assignable(arr, stored) == Some(false) {
        return Some(vh_array_store_check_failure(&*ctx, stored, aastore));    }
    None
}

/// Check 1 of [`vh_array_store_refusal`]: `asType`'s cast of the non-null
/// reference `value` to a real-JDK array handle's `componentType`, or `None`.
pub fn vh_array_component_cast_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    stored: ObjectRef,
) -> Option<MethodCallFailed> {
    let value = Value::Object(Some(stored));
    if let Value::Object(Some(component)) = ctx.get_field_by_name(vh, "componentType") {
        if let Some(name) = mirror_class_name(&*ctx, component) {
            // `reference_arg_admitted` judges `L...;` only; an array or
            // primitive component name is left to check 2.
            let primitive = matches!(
                name.as_str(),
                "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short"
            );
            if !primitive && !name.starts_with('[') {
                let target = format!("L{name};");
                // Wave 26 (lane vhase): an undecided name (a hidden component
                // does not resolve by name) is asked of the mirror's class id.
                let verdict = reference_arg_admitted(ctx, value, &target);
                if verdict == Some(false)
                    || (verdict.is_none()
                        && vh_hidden_component_admits(&*ctx, component, stored) == Some(false))
                {
                    return Some(reference_cast_failure(ctx, value, &target));
                }
            }
            // Round 15 wave 4 (lane vh5): an ARRAY component (an `int[][]`
            // handle's `int[]`) is `asType`'s cast of the value as well, so a
            // `long[]` stored through it is `ClassCastException: Cannot cast
            // [J to [I` on HotSpot, not the `ArrayStoreException` check 2
            // raised. Under the coordinate cast's switch (the same `asType`).
            if name.starts_with('[')
                && vh_array_coordinate_cast_enabled()
                && vh_array_coordinate_admits(&*ctx, stored, &name) == Some(false)
            {
                return Some(reference_cast_failure(ctx, value, &name));            }        }    }
    None
}

/// `CRATONVM_MH_INVOKE_RETURN_REFUSAL` -- default ON (JIT round 16, lane mh; a
/// correctness fix, both modes): the `invoke` door refuses a call site whose
/// return the handle's return cannot convert to (`asType`'s
/// `WrongMethodTypeException`). `0`/`false`/`off`/`no` restores the
/// parameter-only check. Read once.
pub fn mh_invoke_return_refusal_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Do the `MethodHandles` combinator factories run the JDK's argument checks
/// (`dropArgumentChecks`, `filterArgumentsCheckArity` / `filterArgumentChecks`,
/// `filterReturnValue`'s, `foldArgumentChecks`, `collectArguments`',
/// `catchException`'s, `permuteArguments`', `guardWithTest`'s) in this VM's
/// mode? `--real-jdk` always (the round-15 rule); `--compatible` too since JIT
/// round 16 (lane mh), because both modes target HotSpot: `R16MhErrors`
/// `drop-pos` / `filter-mismatch` / `filter-ret-mismatch` built adapters under
/// `--compatible` where HotSpot 25 throws `IllegalArgumentException`.
/// `CRATONVM_MH_COMBINATOR_CHECKS_COMPATIBLE=0` restores the `--real-jdk`-only
/// gate (a kill switch for a `--compatible` arm that trips on a synthetic
/// handle's type). Read only under `--compatible`.
///
/// Interpreter round i1 wave 50 (lane L4) landed the same `--compatible` arm
/// per VM: [`NativeContext::mh_combinator_checks`] (`ClassRealm`,
/// `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE`, on by default). Merged: the
/// checks run when that per-VM answer says so, and under `--compatible` either
/// variable at `0` turns the `--compatible` arm off.
pub fn mh_combinator_checks_in_this_mode(ctx: &dyn NativeContext) -> bool {
    static COMPAT_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // `JdkInvoke` (nio21 wave 12, L3): the row takes `--real-jdk`'s arm.
    ctx.mh_combinator_checks()
        && (ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
            || *COMPAT_ON.get_or_init(|| {
                true
            }))
}

/// `CRATONVM_MH_INVOKE_COLLECTOR_ARITY_REFUSAL` -- default ON (JIT round 16,
/// lane mh; a correctness fix, both modes): a variable-arity collector's
/// `invoke` at another arity judges each collected argument's conversion to
/// the array's component ([`invoke_collector_arity_refusal`]), an array at the
/// site's tail is the fixed-arity pass-through rather than an element, and a
/// refusal at the collected position names the collected type, as HotSpot's
/// `asCollector(..).asType(site)` does. `0`/`false`/`off`/`no` restores the
/// round-15 rule. Read once.
pub fn mh_invoke_collector_arity_refusal_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `(p0 .. p(n-2), component x k)ret`: the type of `asCollector(arrayType, k)`
/// over a handle whose parameters are `params` (the last one the array).
pub fn collected_descriptor(params: &[String], ret: &str, component: &str, k: usize) -> String {
    let fixed = &params[..params.len().saturating_sub(1)];
    let mut out = String::from("(");
    for p in fixed {
        out.push_str(p);
    }
    for _ in 0..k {
        out.push_str(component);
    }
    out.push(')');
    out.push_str(ret);
    out
}

/// [`invoke_narrowing_arg_refusal`] for a call site whose arity is not the
/// handle's (round 16, lane mh): only a variable-arity collector called with
/// at least its fixed parameters has an answer, `asCollector(arrayType, k)
/// .asType(site)`. Every fixed position, every collected argument (against the
/// component) and the return are `MethodType.canConvert` ([`mh_can_convert`]);
/// a refusal is `WrongMethodTypeException: cannot convert MethodHandle<the
/// collected type> to <site>`. HotSpot 25, `R16MhErrors` `invoke-varargs-bad`:
/// `vsum(int...)` invoked as `(int,String)int` is `cannot convert
/// MethodHandle(int,int)int to (int,String)int`; this door collected the
/// `String` as `0` and answered 1. `None` for every other shape.
pub fn invoke_collector_arity_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    declared_params: &[String],
    declared_ret: &str,
    site_params: &[String],
    site_ret: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    if !mh_invoke_collector_arity_refusal_enabled() || !mh_is_varargs_collector(&*ctx, mh) {
        return None;
    }
    let n = declared_params.len();
    let component = declared_params.last()?.strip_prefix('[')?;
    if site_params.len() + 1 < n {
        return None;
    }
    let fixed = n - 1;
    let k = site_params.len() - fixed;
    let params_refused = site_params.iter().enumerate().any(|(i, site)| {
        let target = if i < fixed {
            declared_params[i].as_str()
        } else {
            component
        };
        !mh_can_convert(site, target)
    });
    let return_refused =
        !mh_can_convert(declared_ret, site_ret) && mh_invoke_return_refusal_enabled();
    if !params_refused && !return_refused {
        return None;
    }
    let collected = collected_descriptor(declared_params, declared_ret, component, k);
    let shown_declared = method_type_display_in(&*ctx, &collected)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

pub fn invoke_narrowing_arg_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    kind: i32,
    call_site: &str,
    declared: Option<&str>,
) -> Option<MethodCallFailed> {
    if !mh_strict_exact_signature()
        || !(kind_has_authoritative_adapted_type(kind) || invoker_has_own_type(&*ctx, mh, kind))
    {
        return None;
    }
    // The handle's `mh_declared_descriptor`, read once by the door (round 12
    // wave 7).
    let declared = declared?;
    if declared == call_site || (declared == "()V" && call_site != "()V") {
        return None;
    }
    let (declared_params, declared_ret) = split_descriptor_params(&declared)?;
    let (site_params, site_ret) = split_descriptor_params(call_site)?;
    if declared_params.len() != site_params.len() {
        // Round 16 (lane mh): a variable-arity collector called at ANOTHER
        // arity is `asCollector(arrayType, k).asType(site)`, which converts
        // every collected argument to the component.
        return invoke_collector_arity_refusal(
            ctx,
            mh,
            &declared_params,
            &declared_ret,
            &site_params,
            &site_ret,
            call_site,
        );
    }
    // Round 16 (lane mh): `asType` converts the handle's RETURN to the call
    // site's as well (`MethodType.canConvert(newType.rtype, rtype)`, in the
    // adapter's direction: the handle's return to the site's), so
    // `(String) intToInt.invoke(3)` and `(short) intToInt.invoke(3)` are
    // `WrongMethodTypeException` on HotSpot (`R16MhErrors` `invoke-ret-str` /
    // `invoke-ret-short`). This door answered 6, and for the `String` site
    // handed an `Integer` to code typed `String`. A refused return or a
    // refused parameter below raises the same exception with the same
    // message, so the order between them does not show.
    let return_refused =
        !mh_can_convert(&declared_ret, &site_ret) && mh_invoke_return_refusal_enabled();
    // Round 15 wave 5 (lane mh4): a pair with a REFERENCE on one side and a
    // primitive on the other is `MethodType.canConvert`'s too (`mh_can_convert`,
    // the transcription `asType` already uses): a `Long` at an `int` parameter
    // unboxes to `long`, which does not narrow, so `asType` refuses it with
    // `WrongMethodTypeException` before any argument is looked at. This door
    // let it through to the argument casts, whose unboxing raised
    // `ClassCastException` (`R15Mh2InvokeExactType` `null-order row3`: HotSpot
    // WMTE=5000, CratonVM CCE=5000 in every arm). Two references are a
    // runtime cast, never this refusal's. Switch
    // `CRATONVM_MH_INVOKE_BOXED_ARG_REFUSAL` (default ON), read only on a pair
    // that refuses.
    //
    // A variable-arity COLLECTOR's trailing array parameter is not converted
    // but COLLECTED when the site's argument there is no array of it
    // (`AsVarargsCollector.asTypeUncached`: `asCollector(arrayType, 1)
    // .asType(newType)`), so that position is judged against the array's
    // COMPONENT: `SHOW(Object[]).invoke(7)` collects the `7` (HotSpot
    // "1:Integer"; `R15Mh2InvokeExactType` `sites-rotate` case 5, which this
    // rule refused as `(Object[])String` to `(int)String` until hand-back 3).
    let collector_tail = if mh_is_varargs_collector(&*ctx, mh) {
        declared_params
            .last()
            .and_then(|p| p.strip_prefix('['))
            .map(|component| (declared_params.len() - 1, component.to_string()))
    } else {
        None
    };
    let mut narrows = false;
    let mut refused_at_tail = false;
    for (i, (d, site)) in declared_params.iter().zip(site_params.iter()).enumerate() {
        let refused = match (&collector_tail, primitive_char(d), primitive_char(site)) {
            // The collected position: the element's conversion to the
            // component (`canConvert`; two references are a runtime cast),
            // under this rule's switch, as before the collector rule there
            // was no refusal at an array position at all.
            // Round 16 (lane mh): an ARRAY at the site's tail may be the
            // collector's own array type (`arrayType.isAssignableFrom`), which
            // `AsVarargsCollector.asTypeUncached` passes through at fixed
            // arity; judging it against the component refused
            // `(String,int[])V`'s collector at `(Object,int[])V`. An array is
            // never refused here (fail open: two references are a runtime
            // cast).
            (Some((tail, _)), _, _)
                if *tail == i
                    && site.starts_with('[')
                    && mh_invoke_collector_arity_refusal_enabled() =>
            {
                false
            }
            (Some((tail, component)), _, _) if *tail == i => {
                !mh_can_convert(site, component)
            }
            (_, Some(to), Some(from)) => !primitive_widens(from, to),
            (_, None, None) => false,
            _ => {
                !mh_can_convert(site, d)
            }
        };
        if refused {
            narrows = true;
            refused_at_tail = collector_tail.as_ref().is_some_and(|(tail, _)| *tail == i);
            break;
        }
    }
    if !narrows && !return_refused {
        return None;
    }
    // A refusal at a collector's collected position is `asCollector(arrayType,
    // 1).asType(site)`'s, so HotSpot names the COLLECTED type: `cannot convert
    // MethodHandle(int)void to (long)void` for an `(int[])void` collector.
    let shown_declared = match &collector_tail {
        Some((_, component)) if refused_at_tail && mh_invoke_collector_arity_refusal_enabled() => {
            let collected = collected_descriptor(&declared_params, &declared_ret, component, 1);
            method_type_display_in(&*ctx, &collected)?
        }
        _ => method_type_display_in(&*ctx, &declared)?,
    };
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
    ))
}

/// Widen `value` from the return type dispatch produced to the return type the
/// handle DECLARES, or `None` when no widening applies.
///
/// **Why a retyped handle needs this at all.** `asType` here is a STAMP:
/// `mh_with_stamped_type` clones the handle and writes the new `MethodType`
/// into `type`, leaving `MH_DESC` -- the descriptor dispatch actually runs
/// against -- as the leaf member's. So
/// `findStatic(Math,"max",(int,int)int).asType((int,int)long)` ran the leaf,
/// produced an `int`, and boxed it as `Integer` against the LEAF descriptor.
/// The call site wanted `J`, so the return coercion downstream fabricated a
/// zero -- and, once the strict return-type check landed, threw instead -- on a
/// call HotSpot answers `2`. Recorded as the known blind spot in
/// `vm_exec::mh_strict_invokeexact`'s own comment; this is the half that closes
/// it.
///
/// Widening only, and only between primitives, exactly the set JLS 5.1.2
/// permits. `byte`/`short`/`char`/`int` share `Value::Int` here, so the
/// integral-to-integral widenings need no conversion and are absent below.
/// Anything else -- a narrowing, a reference, a `void` -- returns `None` and
/// leaves today's behaviour untouched.
pub fn widen_return_value(value: Value, from_ret: u8, to_ret: u8) -> Option<Value> {
    if from_ret == to_ret {
        return None;
    }
    let as_i64 = |v: Value| match v {
        Value::Int(i) => Some(i64::from(i)),
        Value::Long(l) => Some(l),
        _ => None,
    };
    Some(match (from_ret, to_ret) {
        (b'B' | b'S' | b'C' | b'I', b'J') => Value::Long(as_i64(value)?),
        (b'B' | b'S' | b'C' | b'I', b'F') => Value::Float(as_i64(value)? as f32),
        (b'B' | b'S' | b'C' | b'I', b'D') => Value::Double(as_i64(value)? as f64),
        (b'J', b'F') => Value::Float(as_i64(value)? as f32),
        (b'J', b'D') => Value::Double(as_i64(value)? as f64),
        (b'F', b'D') => match value {
            Value::Float(f) => Value::Double(f64::from(f)),
            _ => return None,
        },
        _ => return None,
    })
}

/// Narrow `value` from the return type dispatch produced to the return type
/// the handle DECLARES — the JLS 5.1.3 primitive narrowing conversions,
/// which is the piece [`widen_return_value`] deliberately excludes.
///
/// **Why this is safe beside a widening-only sibling.** A stamped `type`
/// NARROWER than `MH_DESC`'s return is only ever reachable through
/// `explicitCastArguments`: `asType` refuses a narrowing conversion eagerly,
/// at `mh_astype_refusal` (before any handle carrying one is ever produced),
/// so a handle that reaches `box_return_against_target` with a narrowing
/// return cannot have come from `asType`. `explicitCastArguments`'s own
/// contract is exactly JLS 5.1.3-style narrowing (an inserted cast), not
/// `asType`'s widening-only rule — see `mh_explicit_cast_refusal`'s doc
/// comment. Measured (`x.explicitCastReturn`,
/// `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`):
/// `explicitCastArguments(len, (String)byte)` on a 6-char string answered
/// `0` — the un-narrowed `int` boxed against the LEAF descriptor, which the
/// call site's own unboxing (expecting a `Byte`) then read as a different
/// wrapper's layout — where HotSpot 25 answers `6`.
///
/// Uses Rust's `as` casts throughout: for the integral truncations that
/// matches the JVM's `i2b`/`i2s`/`i2c`/`l2i` bytecode exactly, and for the
/// float/double-to-integral conversions (`f2i`/`f2l`/`d2i`/`d2l`) Rust's
/// saturating-cast semantics (NaN → 0, out-of-range → the bound) match the
/// JVM's specified `f2i`/`d2i`-family behaviour exactly too — this is not a
/// coincidence, both are defined the same way.
pub fn narrow_return_value(value: Value, from_ret: u8, to_ret: u8) -> Option<Value> {
    if from_ret == to_ret {
        return None;
    }
    let as_i32 = |v: Value| match v {
        Value::Int(i) => Some(i),
        Value::Long(l) => Some(l as i32),
        Value::Float(f) => Some(f as i32),
        Value::Double(d) => Some(d as i32),
        _ => None,
    };
    Some(match (from_ret, to_ret) {
        (b'I' | b'S' | b'C' | b'J' | b'F' | b'D', b'B') => Value::Int(as_i32(value)? as i8 as i32),
        (b'I' | b'C' | b'J' | b'F' | b'D', b'S') => Value::Int(as_i32(value)? as i16 as i32),
        (b'I' | b'S' | b'B' | b'J' | b'F' | b'D', b'C') => Value::Int(as_i32(value)? as u16 as i32),
        (b'J' | b'F' | b'D', b'I') => Value::Int(as_i32(value)?),
        (b'F', b'J') => match value {
            Value::Float(f) => Value::Long(f as i64),
            _ => return None,
        },
        (b'D', b'J') => match value {
            Value::Double(d) => Value::Long(d as i64),
            _ => return None,
        },
        (b'D', b'F') => match value {
            Value::Double(d) => Value::Float(d as f32),
            _ => return None,
        },
        _ => return None,
    })
}

/// Apply [`widen_return_value`]/[`narrow_return_value`] to a dispatch result
/// and re-box it against the TARGET descriptor rather than the leaf one.
///
/// `target` is the handle's declared type for `invokeExact` and the CALL SITE
/// for `invoke` -- the two places the JDK's own `asType(callSiteType)` step
/// would have performed the conversion. Falls back to today's leaf-descriptor
/// boxing whenever the target is absent, unparseable, or not a widening or
/// narrowing this pair of functions covers.
pub fn box_return_against_target(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    leaf_desc: &str,
    target: Option<&str>,
) -> MethodCallResult {
    let Some(target) = target else {
        return auto_box_return(ctx, result, leaf_desc);
    };
    let from = return_type_desc(leaf_desc).as_bytes().first().copied();
    let to = return_type_desc(target).as_bytes().first().copied();
    let (Some(from), Some(to)) = (from, to) else {
        return auto_box_return(ctx, result, leaf_desc);
    };
    if from == to {
        return auto_box_return(ctx, result, leaf_desc);
    }
    match result {
        Ok(Some(value)) => {
            match widen_return_value(value, from, to)
                .or_else(|| narrow_return_value(value, from, to))
            {
                Some(converted) => auto_box_return(ctx, Ok(Some(converted)), target),
                None => auto_box_return(ctx, Ok(Some(value)), leaf_desc),
            }
        }
        other => auto_box_return(ctx, other, leaf_desc),
    }
}

/// A door's return conversion, decided before its dispatch: everything the
/// door does with the leaf's result afterwards, as one value (interpreter
/// round i2 wave 59, lane L4; proposal
/// `i58-L4-proposal-one-return-plan-for-every-method-handle-door`). Every
/// door (`invoke`, `invokeExact`, the three `invokeWithArguments` paths and
/// the two converting lanes) builds one with [`DoorReturn::invoke`],
/// [`DoorReturn::exact`] or [`DoorReturn::with_arguments`] and hands its
/// result to [`door_return_apply`], where each used to repeat the four steps
/// (and the `invokeWithArguments` doors had once missed one of them).
#[derive(Clone, Copy, Debug)]
pub struct DoorReturn<'a> {
    pub kind: i32,
    /// The leaf's descriptor (`MH_DESC`).
    pub leaf: &'a str,
    /// The handle's declared `type`.
    pub declared: Option<&'a str>,
    /// The call site [`return_cast_refusal`] casts to (`None`: the declared
    /// type's cast only).
    pub cast_site: Option<&'a str>,
    /// An `explicitCastArguments` stamp ([`MH_MARK_EXPLICIT_CAST`]).
    pub explicit_stamp: bool,
    /// The marking bits [`invoke_return_unbox`] reads.
    pub marks: i32,
    /// The return [`invoke_return_unbox`] converts to.
    pub unbox_target: Option<&'a str>,
    /// The return [`box_return_against_target`] widens or narrows to
    /// (`None`: the leaf's own boxing).
    pub box_target: Option<&'a str>,
}

impl<'a> DoorReturn<'a> {
    /// `invoke`'s: `asType(site)`, so the cast, the unboxing and the boxing
    /// are the call site's, except a reference site over a primitive declared
    /// return the leaf does not share ([`invoke_boxes_declared_return`]).
    pub fn invoke(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        site: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
    ) -> Self {
        let box_target = if invoke_boxes_declared_return(leaf, declared, site) {
            declared
        } else {
            site
        };
        DoorReturn {
            kind,
            leaf,
            declared,
            cast_site: site,
            explicit_stamp,
            marks,
            unbox_target: site,
            box_target,
        }
    }

    /// `invokeExact`'s: the declared type is the call site's.
    pub fn exact(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
    ) -> Self {
        DoorReturn {
            kind,
            leaf,
            declared,
            cast_site: None,
            explicit_stamp,
            marks,
            unbox_target: declared,
            box_target: declared,
        }
    }

    /// `invokeWithArguments`'s: an `Object` site, so a primitive is boxed as
    /// the leaf's unless the declared primitive differs
    /// ([`invoke_with_arguments_boxes_declared_return`], `boxes_declared`).
    pub fn with_arguments(
        kind: i32,
        leaf: &'a str,
        declared: Option<&'a str>,
        marks: i32,
        explicit_stamp: bool,
        boxes_declared: bool,
    ) -> Self {
        DoorReturn {
            box_target: if boxes_declared { declared } else { None },
            ..DoorReturn::exact(kind, leaf, declared, marks, explicit_stamp)
        }
    }

    // The tail decisions (interpreter round i2 wave 60, lane L4; proposal
    // `i58-L4-proposal-one-return-plan-for-every-method-handle-door`, "Left"):
    // whether the door has work after its dispatch, and so whether it may arm
    // `MH_TAIL`, is a projection of the plan; each door used to recompute it
    // from the same inputs.

    /// An `invoke` plan's [`MH_TAIL`] token: the call site's return
    /// ([`door_tail_token`]) unless the return cast has work after the
    /// dispatch ([`return_cast_may_apply`]), else `0`.
    pub fn invoke_tail(&self) -> u8 {
        if return_cast_may_apply(self.kind, self.leaf, self.declared, self.cast_site) {
            return 0;
        }
        self.cast_site
            .map_or(0, |site| door_tail_token(return_type_desc(site)))
    }

    /// An `invokeExact` plan's [`MH_TAIL`] token for the call site `site`:
    /// the site's return token when the declared return's agrees (the door
    /// converts the leaf's result to the declared return, the interpreter then
    /// to the site's), `0` when the inner return cast has work after the
    /// dispatch or the two disagree.
    pub fn exact_tail(&self, site: Option<&str>) -> u8 {
        if return_cast_may_apply(self.kind, self.leaf, self.declared, None) {
            return 0;
        }
        let token = site.map_or(0, |site| door_tail_token(return_type_desc(site)));
        let agrees = self
            .declared
            .is_none_or(|d| door_tail_token(return_type_desc(d)) == token);
        if agrees {
            token
        } else {
            0
        }
    }

    /// Does an `invokeWithArguments` plan do work after its dispatch: a
    /// declared primitive to box, the inner return cast, or the inner
    /// unboxing ([`inner_return_unbox_may_apply`])? Its doors arm their tail
    /// only when it does not.
    pub fn with_arguments_works_after(&self) -> bool {
        self.box_target.is_some()
            || return_cast_may_apply(self.kind, self.leaf, self.declared, None)
            || inner_return_unbox_may_apply(self.kind, self.leaf, self.declared, self.marks)
    }
}

/// [`DoorReturn`]'s steps on `result`, in the order every door took them: the
/// reference cast's refusal, the reference -> primitive unboxing, then (a
/// constructor's new object aside) the widening / narrowing and the box.
pub fn door_return_apply(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    plan: &DoorReturn<'_>,
) -> MethodCallResult {
    // A handle DECLARED `void` over a leaf that returns a value
    // (`dropReturn(target)`, `target.asType((..)void)`) discards it: `asType`'s
    // return rule, so `(Object) dropReturn(twice).invoke(3)` is `null` on
    // HotSpot 25.0.3, where the door boxed the leaf's `6`
    // (`L4W59CombinatorCallSiteTypes` row `dropReturn`; interpreter round i2
    // wave 59, lane L4, `CRATONVM_MH_VOID_DECLARED_DISCARDS`). The rest of the
    // plan then runs from the declared type.
    let discards = plan.declared.is_some_and(|d| d.ends_with(")V"))
        && !plan.leaf.ends_with(")V")
        && plan.kind != MH_KIND_CONSTRUCTOR
        && kind_has_authoritative_adapted_type(plan.kind)
        && crate::nbflags().mh_void_declared_discards;
    if discards {
        let declared = plan.declared.unwrap_or(plan.leaf);
        let discarded = DoorReturn {
            leaf: declared,
            ..*plan
        };
        return door_return_apply_steps(ctx, result.map(|_| None), &discarded);
    }
    door_return_apply_steps(ctx, result, plan)
}

/// [`door_return_apply`]'s four steps.
pub fn door_return_apply_steps(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    plan: &DoorReturn<'_>,
) -> MethodCallResult {
    if let Some(refusal) = return_cast_refusal(
        ctx,
        &result,
        plan.kind,
        plan.leaf,
        plan.declared,
        plan.cast_site,
        plan.explicit_stamp,
    ) {
        return Err(refusal);
    }
    if let Some(answer) = invoke_return_unbox(
        ctx,
        &result,
        plan.marks,
        plan.kind,
        plan.leaf,
        plan.declared,
        plan.unbox_target,
    ) {
        return answer;
    }
    // A constructor's handle already answers the new object; boxing it
    // against its `V` leaf would answer `null`.
    if plan.kind == MH_KIND_CONSTRUCTOR {
        return result;
    }
    box_return_against_target(ctx, result, plan.leaf, plan.box_target)
}

pub fn return_type_desc(desc: &str) -> &str {
    match desc_close_paren(desc) {
        Some(pos) => &desc[pos + 1..],
        None => "",
    }
}

/// Byte index of the `)` that CLOSES a method descriptor's parameter list,
/// found by walking the parameter tokens.
///
/// Not `find(')')` / `rfind(')')`: JVMS 4.2.2 bars only `. ; [ /` from a class
/// name, so `(LA)B;)LC)D;` is legal — one parameter of class `A)B`, returning
/// class `C)D` — and the first `)` cuts the parameter list short while the
/// last one cuts the return type. `cratonvm_jit_api::descriptor_param_list_end`
/// is the same walk; this crate does not depend on `jit-api`. `None` for a
/// descriptor whose parameter list never closes (malformed).
pub fn desc_close_paren(desc: &str) -> Option<usize> {
    let bytes = desc.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'('));
    while i < bytes.len() && bytes[i] != b')' {
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
    }
    (bytes.get(i) == Some(&b')')).then_some(i)
}

/// Auto-box a primitive return value if the method descriptor returns a primitive.
/// This is needed because signature-polymorphic invoke() returns Object, but the
/// underlying method may return a primitive (int, long, etc.).
pub fn auto_box_return(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    desc: &str,
) -> MethodCallResult {
    let ret_desc = return_type_desc(desc);
    match ret_desc {
        // CANONICAL — this is the signature-polymorphic `invoke` /
        // `invokeWithArguments` return path. Measured
        // `mh.invokeWithArgsInt` / `Char` / `Long` = true and
        // `mh.invokeAsObject` = true; `mhnc.asTypeFloat` = false and
        // `mhoob.asTypeInt1000` = false are the two arms
        // `box_value_canonical` delegates back to `box_value` itself.
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C" => match result {
            Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
            Ok(Some(val)) => Ok(Some(box_value_canonical(ctx, val, ret_desc))),
            other => other,
        },
        "V" => match result {
            // void → null for Object return, but preserve thrown exceptions.
            // Previously this arm unconditionally returned Ok(Some(Object(None))),
            // silently swallowing any Err(ExceptionThrown) from dispatch — the
            // root cause of KC26 / many Quarkus / Spring Boot rc=0 silent exits
            // when an inner native invocation raised through MethodHandle.invokeExact.
            Ok(_) => Ok(Some(Value::Object(None))),
            err => err,
        },
        // Reference / array return ('L…;' or '[…'): an Object-typed polymorphic
        // invoke MUST push a value (null at worst) — `Ok(None)` pushes NOTHING, so
        // the caller's `areturn`/consumer underflows the operand stack. This
        // happens when an adapter chain (guardWithTest / asSpreader / asType) has
        // an Object effective return type but the dispatched leaf method is `void`
        // (mh_dispatch yields `Ok(None)`). Groovy's IndyInterface is the canonical
        // case: every call site returns Object, but the resolved method (e.g.
        // `addRepositories(Closure)`) is void — `fromCache`'s
        // `cachedMethodHandle.invokeExact(args)` then underflowed at its `areturn`.
        // HotSpot's asType(...→Object) bakes a void→null filter into the adapter;
        // our asType is a passthrough, so normalize here. Thrown exceptions and a
        // present value pass through unchanged.
        _ if matches!(ret_desc.as_bytes().first(), Some(b'L') | Some(b'[')) => match result {
            Ok(None) => Ok(Some(Value::Object(None))),
            other => other,
        },
        _ => result, // void handled above; any other shape unchanged
    }
}

/// Step 1 of `i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`
/// (interpreter round i1 wave 45, lane L4): whether the invoke doors and
/// [`mh_dispatch_body`] dispatch a real JDK `DelegatingMethodHandle` through
/// its target. The `true` is its own last commit, so one revert restores the
/// old answer (the doors read this model's slots out of such a handle's
/// bounds and answer `null` or garbage).
pub const DELEGATING_HANDLES_DISPATCH_THROUGH_TARGET: bool = true;

/// The handle a door dispatches for `mh`: `mh` itself, or, for a real JDK
/// `java.lang.invoke.DelegatingMethodHandle` (`MethodHandleImpl$IntrinsicMethodHandle`,
/// `$WrappedMember`, `$CountingWrapper`), the handle it forwards to.
///
/// Such a handle has none of this model's slots (`object_num_fields <=
/// MH_BOUND`, the width test every reader of the slots makes), so the doors
/// used to read `MH_KIND` / `MH_DESC` out of its bounds. JDK 25 builds one in
/// `MethodHandleImpl.makeIntrinsic` (`makeArrayElementAccessor`,
/// `MethodHandles.identity`), `makeWrappedMember` and `bindCaller`. Each is
/// behaviourally its target (`DelegatingMethodHandle.getTarget()`; the
/// reinvoker `LambdaForm` calls it with the same arguments), and all four
/// permitted subclasses of the sealed class keep it in a `private final
/// MethodHandle target` field, which is read directly: no Java runs, so
/// nothing the caller holds can move. Taken only when the delegating handle's
/// `type` is its target's (then an `invokeExact` check against either agrees).
///
/// `MethodHandleImpl$AsVarargsCollector` is left alone: its `asType` COLLECTS
/// trailing arguments at another arity, which the target does not (the JDK
/// body of `asVarargsCollector` is pinned to this model's `MH_VARARGS`
/// marking). Cost on the hot path: the width compare; the rest runs only for
/// a handle that is not this model's.
pub fn mh_door_handle(ctx: &mut dyn NativeContext, mh: ObjectRef) -> ObjectRef {
    if ctx.object_num_fields(mh) > MH_BOUND || !DELEGATING_HANDLES_DISPATCH_THROUGH_TARGET {
        return mh;
    }
    mh_jdk_delegation_target(ctx, mh).unwrap_or(mh)
}

/// The `InternalError` [`mh_dispatch`] raises for a handle this model cannot
/// run (`CRATONVM_MH_FOREIGN_HANDLE_RAISES`, default on; `0` restores the
/// silent zero value). Names the handle's class, so the failing call is
/// attributable. `CRATONVM_DBG_MH_DISPATCH=1` also prints it.
#[cold]
#[inline(never)]
pub fn mh_foreign_handle_refusal(ctx: &mut dyn NativeContext, mh: ObjectRef) -> MethodCallFailed {
    let class = ctx
        .class_name_of_id(ctx.class_id_of_object(mh))
        .unwrap_or_else(|| "?".to_string())
        .class_text(&*ctx);
    let message = format!(
        "CratonVM cannot invoke a {class}: it is not one of this VM's method handles \
         and runs a LambdaForm this VM does not execute"
    );
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_FOREIGN] {message}");
    }
    RuntimeError::InternalError { message }.into()
}

/// Run a real JDK `MethodHandle` this model has no slots for (a
/// `DirectMethodHandle`, a `BoundMethodHandle` species, ...) the way
/// `invokeBasic` runs it on HotSpot: the static method its `LambdaForm`
/// linked (`form.vmentry`, a `MemberName` of kind `REF_invokeStatic`, in a
/// pregenerated `*$Holder` or a spun `LambdaForm$MH` class), with the handle
/// itself first and then the arguments (`linkToStatic`'s layout, through
/// `native_method_handle_link_to`).
///
/// `None` -- the caller raises as before -- unless `mh` is a
/// `java.lang.invoke.MethodHandle` whose form has a static `vmentry` and the
/// arguments are as many as the handle's parameters. Interpreter round i2
/// wave 53, lane L4 (step 2 of
/// `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`), behind
/// `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY`.
///
/// Since wave 54 the arguments are converted to the handle's own parameter
/// types first, as `asType(callSiteType)` converts them on HotSpot
/// ([`adapt_leaf_args`]: a box unboxed and widened at a primitive parameter, a
/// raw primitive widened, or boxed at a reference one, a reference cast),
/// with `site` -- the call site a door armed ([`take_entry_call_site`]) -- for
/// the static types; wave 53 raised for any argument not already of the
/// parameter's variant. A site of another arity is `asType`'s
/// `WrongMethodTypeException`. Arguments already of the handle's types at the
/// handle's own site (`invokeExact`, `invokeBasic`) take no conversion.
#[cold]
#[inline(never)]
pub fn mh_run_foreign_vmentry(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: Option<&str>,
) -> Option<MethodCallResult> {
    let handle_class = ctx.class_id_by_name("java/lang/invoke/MethodHandle")?;
    if !ctx.is_subclass(ctx.class_id_of_object(mh), handle_class) {
        return None;
    }
    let entry = mh_foreign_vmentry(ctx, mh)?;
    let desc = mh_declared_descriptor(ctx, mh)?;
    let (params, _ret) = split_descriptor_params(&desc)?;
    // Wave 55 (`CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`): a real
    // `AsVarargsCollector` at a site of another type is the JDK's own
    // `asType(siteType)` (`asTypeUncached` collects), then that handle.
    if let Some(s) = site.filter(|s| *s != desc) {
        if crate::nbflags().mh_foreign_varargs_run_jdk
            && split_descriptor_params(s).is_some_and(|(p, _)| p.len() == extra_args.len())
            && ctx
                .class_name_arc_of_id(ctx.class_id_of_object(mh))
                .as_deref()
                == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
        {
            return mh_foreign_varargs_collect(ctx, mh, extra_args, s, &desc);
        }
    }
    if let Some(site_params) = site.and_then(split_descriptor_params).map(|(p, _)| p) {
        // A real `AsVarargsCollector` COLLECTS at another arity (its `asType`),
        // which this does not model: it keeps the refusal of its caller.
        if site_params.len() != params.len()
            && site_params.len() == extra_args.len()
            && ctx
                .class_name_arc_of_id(ctx.class_id_of_object(mh))
                .as_deref()
                != Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
        {
            let shown_declared = method_type_display_in(&*ctx, &desc)?;
            let shown_site = method_type_display_in(&*ctx, site?)?;
            return Some(Err(mh_door_wmte(
                ctx,
                MhWmteRaise::AsType,
                &format!("cannot convert MethodHandle{shown_declared} to {shown_site}"),
            )));
        }
    }
    if params.len() != extra_args.len() {
        return None;
    }
    let typed = params
        .iter()
        .zip(extra_args)
        .all(|(p, v)| match p.as_bytes().first() {
            Some(b'Z' | b'B' | b'C' | b'S' | b'I') => matches!(v, Value::Int(_)),
            Some(b'J') => matches!(v, Value::Long(_)),
            Some(b'F') => matches!(v, Value::Float(_)),
            Some(b'D') => matches!(v, Value::Double(_)),
            Some(b'L' | b'[') => matches!(v, Value::Object(_)),
            _ => false,
        });
    let converts = !typed || site.is_some_and(|s| s != desc);
    // An `AsVarargsCollector`'s trailing argument may be one to COLLECT, which
    // a conversion to the array parameter would refuse: wave 53's rule.
    if !typed
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(mh))
            .as_deref()
            == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
    {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        let class = ctx
            .class_name_of_id(ctx.class_id_of_object(mh))
            .unwrap_or_default();
        let from = if converts {
            site.unwrap_or("its arguments' own types")
        } else {
            ""
        };
        let converting = if converts {
            format!(" (converting from {from})")
        } else {
            String::new()
        };
        eprintln!("[MH_FOREIGN] running {class}{desc} through its form's vmentry{converting}");
    }
    let (mh, entry, converted) = if converts {
        // A box the conversion mints is an allocation: the handle (and its
        // entry) are re-read after it; `adapt_leaf_args` re-reads the
        // arguments itself.
        let site_params = leaf_site_params(site, 0, extra_args.len());
        let pin = ctx.pin_native_root(mh);
        let converted = adapt_leaf_args(
            ctx,
            extra_args,
            &desc,
            site_params.as_deref(),
            mh_explicit_scope(),
        );
        let mh = ctx.read_native_pin(pin, mh);
        ctx.unpin_native_roots(pin);
        match converted {
            Ok(values) => (mh, mh_foreign_vmentry(ctx, mh)?, Some(values)),
            Err(refusal) => return Some(Err(refusal)),
        }
    } else {
        (mh, entry, None)
    };
    let values = converted.as_deref().unwrap_or(extra_args);
    // Wave 56 (`CRATONVM_IBG_SPIN_ON_MISS`): the JDK's VarHandle invoker over
    // one of this VM's synthetic `VarHandle`s, which its spun form cannot
    // serve (`getMethodHandle` reads the absent `vform`).
    // Wave 57 (`CRATONVM_VH_SYNTHETIC_MH_TABLE`): `getMethodHandleUncached`
    // answers a synthetic `VarHandle` at the source, whoever asks, so the
    // form runs as it is and neither screen is needed.
    let seeded: Vec<Value>;
    let (mh, entry, values) =
        if crate::nbflags().ibg_spin_on_miss && !crate::nbflags().vh_synthetic_mh_table {
            if let Some(answer) = mh_foreign_vh_invoker_over_synthetic(ctx, mh, values, &desc) {
                return Some(answer);
            }
            // ... and any other real handle that carries one (an `asType` /
            // `bindTo` of that invoker inlines its form): the synthetic
            // `VarHandle`'s `methodHandleTable` is seeded for the access modes
            // the handle's `AccessDescriptor`s name.
            match mh_foreign_seed_synthetic_vh_tables(ctx, mh, values) {
                Some((mh, again)) => {
                    seeded = again;
                    (mh, mh_foreign_vmentry(ctx, mh)?, seeded.as_slice())
                }
                None => (mh, entry, values),
            }
        } else {
            (mh, entry, values)
        };
    // Wave 55 (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`): an entry the held
    // `InvokerBytecodeGenerator.generateCustomizedCode` native fabricated
    // (`LambdaForm.MH`, a method no class declares) is no code to run: a real
    // direct handle is its `member` (`mh_real_direct_dispatch`), anything else
    // keeps its caller's refusal. Running it was a `NoSuchMethodError` naming
    // `java.lang.invoke.LambdaForm.MH(..)`.
    if crate::nbflags().mh_foreign_vmentry_first && mh_vmentry_is_fabricated(ctx, entry) {
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_FOREIGN] {desc}: its form's vmentry is fabricated; dispatching by member"
            );
        }
        return mh_real_direct_dispatch(ctx, mh, values, true);
    }
    // Wave 54 (`CRATONVM_MH_FOREIGN_DMH_BY_MEMBER`): a plain direct handle's
    // linker would only fetch its `member` and make this `linkTo*` call.
    if crate::nbflags().mh_foreign_dmh_by_member {
        if let Some((member, ensure_init)) = mh_foreign_dmh_member(ctx, mh, entry) {
            if crate::nbflags().dbg_mh_dispatch {
                let init = if ensure_init {
                    " (initializing its class first)"
                } else {
                    ""
                };
                eprintln!("[MH_FOREIGN] dispatching DirectMethodHandle{desc} by its member{init}");
            }
            let (member, values) = if ensure_init {
                // `invokeStaticInit`'s barrier, the JDK's own
                // `internalMemberNameEnsureInit` (it runs `<clinit>`): a GC
                // point, so the arguments are pinned across it.
                let (pin_base, handles) = pin_mh_args(ctx, values);
                let member = ctx.invoke(
                    "java/lang/invoke/DirectMethodHandle",
                    "internalMemberNameEnsureInit",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(mh))],
                );
                let values = read_pinned_mh_args(ctx, &handles, values);
                if pin_base != usize::MAX {
                    ctx.unpin_native_roots(pin_base);
                }
                match member {
                    Ok(Some(Value::Object(Some(m)))) => (m, values),
                    Ok(_) => return None,
                    Err(e) => return Some(Err(e)),
                }
            } else {
                (member, values.to_vec())
            };
            let mut args = values;
            args.push(Value::Object(Some(member)));
            return Some(crate::native_method_handle_link_to(ctx, &args));
        }
    }
    let mut args = Vec::with_capacity(values.len() + 2);
    args.push(Value::Object(Some(mh)));
    args.extend_from_slice(values);
    args.push(Value::Object(Some(entry)));
    Some(crate::native_method_handle_link_to(ctx, &args))
}

/// The JDK's own VarHandle invoker (`Invokers.varHandleMethodInvoker` /
/// `varHandleMethodExactInvoker`: a `BoundMethodHandle$Species_L` over a
/// `VARHANDLE_INVOKER` / `VARHANDLE_EXACT_INVOKER` form, bound to its
/// `AccessDescriptor`) called on one of this VM's synthetic `VarHandle`s,
/// which have no `vform`: the form's `checkVarHandle*Type` reaches
/// `getMethodHandleUncached`, a `NullPointerException` on `this.vform` on
/// every call. Answered as this model's invoker of the same access mode and
/// type ([`vh_invoker_model_handle`], what the registered
/// `MethodHandles.varHandleInvoker` answers), with the arguments already of
/// the handle's own types. `None` for any other handle, a real `VarHandle`
/// (its form runs), or a `null` one (the form's `NullPointerException`).
/// Interpreter round i2 wave 56, lane L4 (`CRATONVM_IBG_SPIN_ON_MISS`).
#[cold]
#[inline(never)]
pub fn mh_foreign_vh_invoker_over_synthetic(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    values: &[Value],
    own: &str,
) -> Option<MethodCallResult> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(mh))
        .as_deref()
        != Some("java/lang/invoke/BoundMethodHandle$Species_L")
    {
        return None;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(mh, "form") else {
        return None;
    };
    let Value::Object(Some(kind)) = ctx.get_field_by_name(form, "kind") else {
        return None;
    };
    let exact = match ctx.get_field_by_name(kind, "name") {
        Value::Object(Some(s)) => match ctx.read_string(s).as_deref() {
            Some("VARHANDLE_INVOKER") => false,
            Some("VARHANDLE_EXACT_INVOKER") => true,
            _ => return None,
        },
        _ => return None,
    };
    let Some(Value::Object(Some(vh))) = values.first().copied() else {
        return None;
    };
    if matches!(ctx.get_field_by_name(vh, "vform"), Value::Object(Some(_))) {
        return None;
    }
    let (params, ret) = split_descriptor_params(own)?;
    if params.first().map(String::as_str) != Some("Ljava/lang/invoke/VarHandle;") {
        return None;
    }
    let inner = format!("({}){ret}", params[1..].concat());
    let Value::Object(Some(ad)) = ctx.get_field_by_name(mh, "argL0") else {
        return None;
    };
    let Value::Int(ordinal) = ctx.get_field_by_name(ad, "mode") else {
        return None;
    };
    // Two Java calls and an allocation: the arguments are pinned across them.
    let (pin_base, handles) = pin_mh_args(ctx, values);
    let mode = ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueFromOrdinal",
        "(I)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Int(ordinal)],
    );
    let invoker = match mode {
        Ok(Some(Value::Object(Some(mode)))) => {
            let method = access_mode_method_name(&access_mode_name(ctx, mode));
            if method.is_empty() {
                Ok(None)
            } else {
                vh_invoker_model_handle(ctx, &method, &inner, exact)
            }
        }
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    };
    let values = read_pinned_mh_args(ctx, &handles, values);
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    match invoker {
        Ok(Some(Value::Object(Some(invoker)))) => {
            if crate::nbflags().dbg_mh_dispatch {
                let e = if exact { "exact " } else { "" };
                eprintln!("[MH_FOREIGN] the JDK's {e}VarHandle invoker{own} over a synthetic VarHandle: this model's invoker{inner}");
            }
            Some(mh_dispatch(ctx, invoker, &values))
        }
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// A real JDK handle about to run its form's `vmentry` with one of this VM's
/// synthetic `VarHandle`s among its arguments or bound values, and a
/// `VarHandle$AccessDescriptor` beside it (an `asType` / `bindTo` of the JDK's
/// VarHandle invoker, whose edited form inlines `checkVarHandleGenericType`):
/// the form's `handle.getMethodHandle(ad.mode)` reads the `VarHandle`'s
/// `methodHandleTable` cache first and only on a miss reaches
/// `getMethodHandleUncached`, which needs the `vform` a synthetic handle does
/// not have. So the cache is seeded, for the access modes the descriptors
/// name, with what `getMethodHandleUncached` would answer: a handle of type
/// `accessModeType(mode)` with the `VarHandle` prepended that performs the
/// mode on its first argument (this model's invoker, as
/// [`mh_foreign_vh_invoker_over_synthetic`] uses). `Some` (the handle and the
/// arguments re-read: the seeding allocates and calls Java) only when an
/// entry was written. Interpreter round i2 wave 56, lane L4
/// (`CRATONVM_IBG_SPIN_ON_MISS`).
#[cold]
#[inline(never)]
pub fn mh_foreign_seed_synthetic_vh_tables(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    values: &[Value],
) -> Option<(ObjectRef, Vec<Value>)> {
    let vh_class = ctx.class_id_by_name("java/lang/invoke/VarHandle")?;
    let ad_class = ctx.class_id_by_name("java/lang/invoke/VarHandle$AccessDescriptor")?;
    let mut vhs: Vec<ObjectRef> = Vec::new();
    let mut modes: Vec<i32> = Vec::new();
    let mut candidates: Vec<ObjectRef> = values
        .iter()
        .filter_map(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .collect();
    for i in 0..ctx.object_num_fields(mh) {
        if let Value::Object(Some(o)) = ctx.get_field(mh, i) {
            candidates.push(o);
        }
    }
    for o in candidates {
        if ctx.object_is_array(o) {
            continue;
        }
        let cid = ctx.class_id_of_object(o);
        if cid == ad_class {
            if let Value::Int(m) = ctx.get_field_by_name(o, "mode") {
                if !modes.contains(&m) {
                    modes.push(m);
                }
            }
        } else if (cid == vh_class || ctx.is_subclass(cid, vh_class))
            && !vhs.contains(&o)
            && !matches!(ctx.get_field_by_name(o, "vform"), Value::Object(Some(_)))
            && !vh_has_synthetic_layout(ctx, o)
        {
            vhs.push(o);
        }
    }
    if vhs.is_empty() || modes.is_empty() {
        return None;
    }
    // Every Java call and allocation below is a GC point: the handle, the
    // arguments and the `VarHandle`s are pinned for the whole seeding.
    let mh_pin = ctx.pin_native_root(mh);
    let (args_base, handles) = pin_mh_args(ctx, values);
    let vh_pins: Vec<usize> = vhs.iter().map(|vh| ctx.pin_native_root(*vh)).collect();
    let mut wrote = false;
    for (k, vh0) in vhs.iter().enumerate() {
        for &mode in &modes {
            let vh = ctx.read_native_pin(vh_pins[k], *vh0);
            wrote |= vh_seed_method_handle_table(ctx, vh, mode).is_some();
        }
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    let again = read_pinned_mh_args(ctx, &handles, values);
    ctx.unpin_native_roots(mh_pin);
    let _ = args_base;
    if !wrote {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] seeded the methodHandleTable of {} synthetic VarHandle(s) for mode(s) {modes:?}",
            vhs.len()
        );
    }
    Some((mh, again))
}

/// One entry of [`mh_foreign_seed_synthetic_vh_tables`]: `vh`'s
/// `methodHandleTable[mode]`, created (`AccessMode.COUNT` long) when absent;
/// `Some(())` when written, `None` when it already held a handle or anything
/// could not be built (the form then fails as before).
pub fn vh_seed_method_handle_table(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    mode: i32,
) -> Option<()> {
    let index = usize::try_from(mode).ok()?;
    let table = match ctx.get_field_by_name(vh, "methodHandleTable") {
        Value::Object(Some(t)) => {
            if index >= ctx.array_length(t) {
                return None;
            }
            if matches!(ctx.get_array_element(t, index), Value::Object(Some(_))) {
                return None;
            }
            None
        }
        _ => Some(()),
    };
    let pin = ctx.pin_native_root(vh);
    let answer = vh_seed_method_handle_table_pinned(ctx, pin, vh, index, table.is_some());
    ctx.unpin_native_roots(pin);
    answer
}

pub fn vh_seed_method_handle_table_pinned(
    ctx: &mut dyn NativeContext,
    pin: usize,
    vh: ObjectRef,
    index: usize,
    create: bool,
) -> Option<()> {
    let handle = vh_model_mode_handle(ctx, pin, vh, index)?;
    let handle_pin = ctx.pin_native_root(handle);
    let vh_now = ctx.read_native_pin(pin, vh);
    let table = if create {
        let count = match ctx.invoke(
            "java/lang/invoke/VarHandle$AccessMode",
            "values",
            "()[Ljava/lang/invoke/VarHandle$AccessMode;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(all)))) => ctx.array_length(all),
            _ => return None,
        };
        let component =
            crate::lang_class::reflection_component_id(ctx, "java/lang/invoke/MethodHandle");
        let table = ctx.new_ref_array(component, count);
        let vh_now = ctx.read_native_pin(pin, vh);
        ctx.set_field_by_name(vh_now, "methodHandleTable", Value::Object(Some(table)));
        table
    } else {
        match ctx.get_field_by_name(vh_now, "methodHandleTable") {
            Value::Object(Some(t)) => t,
            _ => return None,
        }
    };
    if index >= ctx.array_length(table) {
        return None;
    }
    let handle = ctx.read_native_pin(handle_pin, handle);
    ctx.set_array_element(table, index, Value::Object(Some(handle)));
    Some(())
}

/// What `VarHandle.getMethodHandleUncached(mode)` answers on HotSpot -- a
/// handle of type `accessModeType(mode)` with the `VarHandle` prepended that
/// performs the access mode on its first argument -- for one of this VM's
/// synthetic `VarHandle`s (no `vform`): this model's invoker of that mode
/// ([`vh_invoker_model_handle`]). `vh` is pinned at `pin` by the caller (the
/// two Java calls and the allocation are GC points). `None` when anything
/// cannot be built. Shared by the wave-56 table seeding and the wave-57
/// intrinsic ([`varhandle_get_method_handle_uncached`]).
pub fn vh_model_mode_handle(
    ctx: &mut dyn NativeContext,
    pin: usize,
    vh: ObjectRef,
    index: usize,
) -> Option<ObjectRef> {
    let mode = match ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueFromOrdinal",
        "(I)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Int(index as i32)],
    ) {
        Ok(Some(Value::Object(Some(m)))) => m,
        _ => return None,
    };
    let mode_pin = ctx.pin_native_root(mode);
    let vh_now = ctx.read_native_pin(pin, vh);
    let mode_type = ctx.invoke_virtual(
        vh_now,
        "accessModeType",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(mode))],
    );
    let mode = ctx.read_native_pin(mode_pin, mode);
    let method = access_mode_method_name(&access_mode_name(ctx, mode));
    let desc = match mode_type {
        Ok(Some(Value::Object(Some(mt)))) => methodtype_to_descriptor(ctx, mt).unwrap_or_default(),
        _ => String::new(),
    };
    if method.is_empty() || desc.is_empty() {
        return None;
    }
    match vh_invoker_model_handle(ctx, &method, &desc, false) {
        Ok(Some(Value::Object(Some(h)))) => Some(h),
        _ => None,
    }
}

/// `VarHandle.getMethodHandleUncached(I)` -- a reviewed `Intrinsic`
/// registered only under `CRATONVM_VH_SYNTHETIC_MH_TABLE` (interpreter round
/// i2 wave 57, lane L4; proposal
/// `interpreter-L4-proposal-give-synthetic-varhandles-the-jdks-method-handle-table-FIXED-20261021`).
///
/// The JDK's `VarHandle.getMethodHandle(mode)` -- what
/// `Invokers.checkVarHandleGenericType` / `checkVarHandleExactType` (every
/// VarHandle invoker form, and every form that inlines one after an
/// `asType` / `bindTo`) call -- caches into `methodHandleTable` what this
/// method answers. Its body reads `vform.getMemberName(mode)`, and a
/// `VarHandle` this VM minted has no `vform`: a `NullPointerException` on
/// every miss, which wave 56 screened from the call side in
/// `mh_run_foreign_vmentry` (`mh_foreign_vh_invoker_over_synthetic`,
/// `mh_foreign_seed_synthetic_vh_tables`) for the handles that pass that
/// door. Answering here covers every consumer, whichever way it reached
/// `getMethodHandle`: for a synthetic handle, this model's invoker of the
/// mode ([`vh_model_mode_handle`]), which the JDK's own `getMethodHandle`
/// then caches; for any other receiver (a real `VarHandle`, a synthetic
/// layout), the JDK's own body (`invoke_special_bytecode_only`, which skips
/// this registration).
pub fn varhandle_get_method_handle_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(vh))), Some(Value::Int(mode))) = (args.first(), args.get(1)) {
        let (vh, mode) = (*vh, *mode);
        if let Ok(index) = usize::try_from(mode) {
            if !matches!(ctx.get_field_by_name(vh, "vform"), Value::Object(Some(_)))
                && !vh_has_synthetic_layout(ctx, vh)
            {
                let pin = ctx.pin_native_root(vh);
                let handle = vh_model_mode_handle(ctx, pin, vh, index);
                // The building is a GC point: the body below takes the
                // handle's current address.
                let vh = ctx.read_native_pin(pin, vh);
                ctx.unpin_native_roots(pin);
                if let Some(handle) = handle {
                    if crate::nbflags().dbg_mh_dispatch {
                        eprintln!(
                            "[VH_MH_TABLE] getMethodHandleUncached({mode}) of a synthetic VarHandle: this model's invoker"
                        );
                    }
                    return Ok(Some(Value::Object(Some(handle))));
                }
                return ctx.invoke_special_bytecode_only(
                    "java/lang/invoke/VarHandle",
                    "getMethodHandleUncached",
                    "(I)Ljava/lang/invoke/MethodHandle;",
                    &[Value::Object(Some(vh)), Value::Int(mode)],
                );
            }
        }
    }
    ctx.invoke_special_bytecode_only(
        "java/lang/invoke/VarHandle",
        "getMethodHandleUncached",
        "(I)Ljava/lang/invoke/MethodHandle;",
        args,
    )
}

/// A real JDK `MethodHandleImpl$AsVarargsCollector` called at `site`, a type
/// other than its own of the arguments' arity ([`mh_run_foreign_vmentry`],
/// `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`): what `invoke` does on HotSpot,
/// `asType(siteType)` -- the JDK's own body, whose `asTypeUncached` builds
/// (and caches) the collector, or refuses -- and then that handle, whose type
/// is the site. Interpreter round i2 wave 55, lane L4 (proposal
/// `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
/// Wave 53/54 left such a call to its caller's `InternalError`.
#[cold]
#[inline(never)]
pub fn mh_foreign_varargs_collect(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: &str,
    own: &str,
) -> Option<MethodCallResult> {
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!("[MH_FOREIGN] AsVarargsCollector at {site}: asType through the JDK's own body");
    }
    let answer = mh_foreign_varargs_collect_raw(ctx, mh, extra_args, site)?;
    // A reference site (`(Object) h.invoke(..)`, `invokeWithArguments`)
    // takes the result boxed from the collector's own return, which a door
    // reading the collector as this model's slotless handle does not box.
    Some(
        if matches!(return_type_desc(site).as_bytes().first(), Some(b'L' | b'[')) {
            auto_box_return(ctx, answer, own)
        } else {
            answer
        },
    )
}

pub fn mh_foreign_varargs_collect_raw(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra_args: &[Value],
    site: &str,
) -> Option<MethodCallResult> {
    // `asType` allocates (a `MethodType`, a `LambdaForm`, a spun class): the
    // arguments and the handle are pinned across it.
    let (pin_base, handles) = pin_mh_args(ctx, extra_args);
    let mh_pin = ctx.pin_native_root(mh);
    let adapted = match build_method_type_from_descriptor(ctx, site) {
        Ok(Some(mt)) => {
            let mh = ctx.read_native_pin(mh_pin, mh);
            // Wave 114: `mh_real_as_type_bytecode` reads an `asTypeCache` hit.
            mh_real_as_type_bytecode(ctx, mh, mt)
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let values = read_pinned_mh_args(ctx, &handles, extra_args);
    ctx.unpin_native_roots(mh_pin);
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    match adapted {
        // This model's collector (`asCollector` of a real handle keeps this
        // model's carrier unless `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK`): its
        // own dispatch, which the doors would have made for it.
        Ok(Some(Value::Object(Some(collector)))) if ctx.object_num_fields(collector) > MH_BOUND => {
            Some(mh_dispatch(ctx, collector, &values))
        }
        Ok(Some(Value::Object(Some(collector)))) => {
            mh_run_foreign_vmentry(ctx, collector, &values, Some(site))
        }
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The `member` of a real JDK `java.lang.invoke.DirectMethodHandle` (the class
/// itself, or `$Special` for the plain `invokeSpecial` linker; not
/// `$Interface`, whose linker checks its receiver, nor `$Constructor` /
/// `$Accessor`) whose form's `vmentry` is the plain `invokeStatic`,
/// `invokeStaticInit`, `invokeVirtual` or `invokeSpecial` linker
/// (`DirectMethodHandle$Holder`, or a spun form of the same kind) over a member
/// of the matching reference kind, and whether the linker's class-init barrier
/// is owed first (`invokeStaticInit`); `None` for anything else. Such a
/// linker's body is `linkTo*(args..., internalMemberName(mh))` (or
/// `internalMemberNameEnsureInit`), so the member IS the call
/// ([`mh_run_foreign_vmentry`]): an optimisation since wave 55, when the
/// linker's own `linkTo*` from bytecode reaches the native too
/// (`CRATONVM_MH_LINKTO_FROM_BYTECODE`). Reads only.
pub fn mh_foreign_dmh_member(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    entry: ObjectRef,
) -> Option<(ObjectRef, bool)> {
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    let special_class = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(mh))
        .as_deref()
    {
        Some("java/lang/invoke/DirectMethodHandle") => false,
        Some("java/lang/invoke/DirectMethodHandle$Special") => true,
        _ => return None,
    };
    let Value::Object(Some(name)) = ctx.get_field_by_name(entry, "name") else {
        return None;
    };
    let (want, ensure_init) = match (ctx.read_string(name).as_deref(), special_class) {
        (Some("invokeStatic"), false) => (REF_INVOKE_STATIC, false),
        (Some("invokeStaticInit"), false) => (REF_INVOKE_STATIC, true),
        (Some("invokeVirtual"), false) => (REF_INVOKE_VIRTUAL, false),
        (Some("invokeSpecial"), _) => (REF_INVOKE_SPECIAL, false),
        _ => return None,
    };
    let Value::Object(Some(member)) = ctx.get_field_by_name(mh, "member") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(member))
        .as_deref()
        != Some("java/lang/invoke/MemberName")
    {
        return None;
    }
    // `preparedLambdaForm` gives a statically bound `invokeVirtual` member
    // (final, private) the `invokeSpecial` linker; its member keeps its kind,
    // whose dispatch binds the same method.
    match ctx.get_field_by_name(member, "flags") {
        Value::Int(flags)
            if (flags >> 24) & 0x0F == want
                || (want == REF_INVOKE_SPECIAL && (flags >> 24) & 0x0F == REF_INVOKE_VIRTUAL) =>
        {
            Some((member, ensure_init))
        }
        _ => None,
    }
}

/// Is `entry` (a form's `vmentry`) one the held
/// `InvokerBytecodeGenerator.generate*` natives fabricated
/// (`alloc_resolved_member_name`: a static `MH` / `interpret_*` / `NFI` "of"
/// `java.lang.invoke.LambdaForm`, which declares no such method), rather than
/// a method a class really has? A real entry is in a `*$Holder` or a spun
/// `LambdaForm$MH` class. Reads only.
pub fn mh_vmentry_is_fabricated(ctx: &mut dyn NativeContext, entry: ObjectRef) -> bool {
    match ctx.get_field_by_name(entry, "clazz") {
        Value::Object(Some(mirror)) => {
            resolve_class_name_robust(ctx, mirror).as_deref() == Some("java/lang/invoke/LambdaForm")
        }
        _ => true,
    }
}

/// Does a door hand this real handle to the wave-53 `vmentry` route
/// ([`mh_run_foreign_vmentry`]) rather than to the JDK's `LambdaForm`
/// interpreter ([`mh_real_lambda_form_dispatch`])? Outside
/// `CRATONVM_PURE_BYTECODE`, with `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY` and
/// `CRATONVM_MH_FOREIGN_VMENTRY_FIRST` on, when its form's entry is real code
/// ([`mh_vmentry_is_fabricated`]). Interpreter round i2 wave 55, lane L4.
#[cold]
#[inline(never)]
pub fn mh_foreign_vmentry_first(ctx: &mut dyn NativeContext, mh: ObjectRef) -> bool {
    if !crate::nbflags().mh_foreign_vmentry_first
        || !crate::nbflags().mh_foreign_handle_runs_vmentry
        || cratonvm_native_api::unretire::pure_bytecode()
    {
        return false;
    }
    let entry = mh_foreign_vmentry(ctx, mh);
    let first = entry.is_some_and(|entry| !mh_vmentry_is_fabricated(ctx, entry));
    if crate::nbflags().dbg_mh_dispatch {
        let class = ctx
            .class_name_of_id(ctx.class_id_of_object(mh))
            .unwrap_or_default();
        if first {
            eprintln!(
                "[MH_FOREIGN] {class} runs its form's vmentry, not the LambdaForm interpreter"
            );
        } else {
            eprintln!(
                "[MH_FOREIGN] {class}: no real form vmentry ({})",
                if entry.is_some() {
                    "fabricated"
                } else {
                    "none"
                }
            );
        }
    }
    first
}

/// A real JDK handle's `form.vmentry` when it is a `MemberName` of kind
/// `REF_invokeStatic` ([`mh_run_foreign_vmentry`]), or `None`. Reads only.
pub fn mh_foreign_vmentry(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<ObjectRef> {
    const REF_INVOKE_STATIC: i32 = 6;
    let Value::Object(Some(form)) = ctx.get_field_by_name(mh, "form") else {
        return None;
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(entry))
        .as_deref()
        != Some("java/lang/invoke/MemberName")
    {
        return None;
    }
    let flags = match ctx.get_field_by_name(entry, "flags") {
        Value::Int(f) => f,
        _ => return None,
    };
    ((flags >> 24) & 0x0F == REF_INVOKE_STATIC).then_some(entry)}

/// The cold half of [`mh_door_handle`]: `Some(target)` when `mh` is a real
/// JDK `DelegatingMethodHandle` the doors may dispatch through, following
/// nested delegation (bounded).
pub fn mh_jdk_delegation_target(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<ObjectRef> {
    const AS_VARARGS_COLLECTOR: &str = "java/lang/invoke/MethodHandleImpl$AsVarargsCollector";
    let delegating = ctx.class_id_by_name("java/lang/invoke/DelegatingMethodHandle")?;
    let mut cur = mh;
    for _ in 0..8 {
        if ctx.object_num_fields(cur) > MH_BOUND {
            break;
        }
        let cid = ctx.class_id_of_object(cur);
        if cid == delegating || !ctx.is_subclass(cid, delegating) {
            break;
        }
        let class = ctx.class_name_of_id(cid).unwrap_or_default();
        if class == AS_VARARGS_COLLECTOR {
            break;
        }
        let target = match ctx.get_field_by_name(cur, "target") {
            Value::Object(Some(t)) => t,
            _ => break,
        };
        let own = mh_declared_descriptor(ctx, cur);
        let forwarded = mh_declared_descriptor(ctx, target);
        if own.is_none() || own != forwarded {
            break;
        }
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_DELEGATING] {class} {} -> target {}",
                own.as_deref().unwrap_or("?"),
                ctx.class_name_of_id(ctx.class_id_of_object(target))
                    .unwrap_or_else(|| "?".to_string()),
            );
        }
        cur = target;
    }
    (cur != mh).then_some(cur)
}

/// SBR3 wave 2 (lane mhperf): what a REAL method handle's calls need, decoded
/// once per handle instead of on every call. The doors rebuilt the same
/// descriptor `String`s, class name and member name (`link_to_method_descriptor`,
/// `resolve_class_name_robust`, `read_string`, six `get_field_by_name`) per call:
/// that was most of the 16x between `invokeExact` through a real `DirectMethodHandle`
/// and through the carrier.
///
/// Every field read is of a `final` field of the handle (`MethodHandle.type`,
/// `DirectMethodHandle.member`, `MemberName.clazz/name/type/flags`), so the record
/// is a pure function of the handle's identity.
pub struct RealHandleRecord {
    /// `type()` rendered as a descriptor: what `invokeExact` compares the call site with.
    pub declared: String,
    /// The method kinds of a `DirectMethodHandle` (`REF_invokeVirtual` .. `REF_invokeInterface`,
    /// not constructors or fields), or `None` for any other real handle.
    pub direct: Option<RealDirectTarget>,
    /// The field kinds of a `DirectMethodHandle` (`$Accessor` / `$StaticAccessor`), decoded
    /// only under `CRATONVM_MH_REAL_FIELD_RECORD` ([`mh_real_field_record_fast`]).
    pub field: Option<RealFieldTarget>,
    /// JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO`): the last
    /// linker pair [`mh_compiled_linker_pair`] answered for a compiled `invokeExact` site
    /// on this handle ([`mh_compiled_linker_pair_memo`]). Valid for the record's epoch.
    pub linker_pair: std::cell::RefCell<Option<LinkerPairMemo>>,
}

/// One [`mh_compiled_linker_pair`] answer for a handle (JIT round 21 lane reflect3): the
/// site descriptor's text, the caller class, the pair's objects (rows of the VM's rooted
/// linker table, which do not move within the epoch the record belongs to), and whether
/// the handle's class is a `BoundMethodHandle` species (the null-first-argument rule).
///
/// JIT round 21 lane reflect4 (correctness of the opt-in memo, unconditional): the site
/// text is compared by CONTENT. It was its address and length, but a compiled site's
/// descriptor lives in its `CompiledMethod`'s owned strings and is freed with the body, so
/// another site's different descriptor of the same length could reuse the address and be
/// handed this site's linker and appendix `MethodType` (whose `checkExactType` would then
/// pass for the wrong call shape).
#[derive(Clone)]
pub struct LinkerPairMemo {
    pub site: Box<str>,
    pub caller: u32,
    pub member: ObjectRef,
    pub appendix: ObjectRef,
    pub bmh_species: bool,
}

/// A real field accessor's shape (interpreter round i3 wave 73, lane L4): what
/// [`mh_real_direct_dispatch`] / [`mh_real_field_access`] re-derived on every call, all
/// of it read from `final` fields (`member`, `fieldOffset` / `staticOffset`,
/// `MemberName.flags` and `type`) or registered once per VM (the static offset's target).
pub struct RealFieldTarget {
    /// `REF_getField` .. `REF_putStatic`.
    pub ref_kind: i32,
    /// The instance slot, or the registered static offset.
    pub offset: usize,
    /// The field's descriptor (`mirror_to_descriptor` of `MemberName.type`).
    pub fdesc: String,
    /// The static field the offset was registered for; `None` for an instance field.
    pub target: Option<(ClassId, usize)>,
    /// `MemberName.flags` carries `ACC_VOLATILE` (interpreter round i3 wave 79,
    /// lane L4): the access is fenced as a volatile one ([`mh_real_field_access_resolved`]).
    pub volatile: bool,
}

pub struct RealDirectTarget {
    pub ref_kind: i32,
    pub class: String,
    /// The declaring class, resolved once (a static call then skips the by-name class lookup).
    pub owner: Option<ClassId>,
    pub name: String,
    /// The member's own `MethodType` (no receiver) as a descriptor.
    pub desc: String,
    pub params: Vec<String>,
    /// The owner an earlier static call settled (`invoke_static_settling`): once set, the
    /// call takes the native-to-Java callback memo instead of resolving by name.
    pub settled: std::cell::Cell<Option<ClassId>>,
    /// The static door's served-raise key, kept beside `settled` (interpreter round i2
    /// wave 65, lane L3; `invoke_static_settling_keyed`, the VM's to fill).
    pub door_key: cratonvm_native_api::SettledDoorKey,
    /// A `DirectMethodHandle$Interface`'s (`ref_kind` 9) `refc`, the interface every
    /// receiver must implement (`Interface.checkReceiver`); `None` for every other kind
    /// (JIT round 19 wave 1, lane mhiface; [`mh_interface_receiver_check_enabled`]).
    pub refc: Option<ClassId>,
}

pub const REAL_HANDLE_MEMO_SETS: usize = 64;

thread_local! {
    /// Per-thread memo of [`RealHandleRecord`]s, indexed by the handle's address.
    ///
    /// The soundness argument is `DIRECT_LANE_MEMO`'s: nothing moves or is freed
    /// within one collection epoch, so an equal address in an equal epoch under the
    /// same VM is the same object, and the record is a function of final fields. A
    /// context that reports VM identity `0` (the mocks) is never memoised.
    pub static REAL_HANDLE_MEMO: std::cell::RefCell<crate::AddrMemo<RealHandleRecord>> =
        std::cell::RefCell::new(crate::AddrMemo::new(REAL_HANDLE_MEMO_SETS));
}

pub fn real_handle_decode(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<RealHandleRecord> {
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    let declared = crate::link_to_method_descriptor(&*ctx, mt)?;
    let cid = ctx.class_id_of_object(this);
    let is_direct = ctx
        .class_name_arc_of_id(cid)
        .is_some_and(|n| n.starts_with("java/lang/invoke/DirectMethodHandle"));
    let direct = if is_direct {
        real_direct_target_decode(ctx, this)
    } else {
        None
    };
    if !is_direct && !matches!(ctx.get_field_by_name(this, "form"), Value::Object(Some(_))) {
        return None;
    }
    let field = if is_direct && direct.is_none() && mh_real_field_record_enabled() {
        real_field_target_decode(ctx, this)
    } else {
        None
    };
    Some(RealHandleRecord {
        declared,
        direct,
        field,
        linker_pair: std::cell::RefCell::new(None),
    })
}

/// `CRATONVM_MH_REAL_FIELD_RECORD` -- performance, default OFF, `truthy_word`
/// (interpreter round i3 wave 73, lane L4; proposal
/// `i72-L4-proposal-a-real-field-accessor-is-served-from-its-handle-record-20261105`):
/// see [`mh_real_field_record_fast`].
pub fn mh_real_field_record_enabled() -> bool {
    crate::nbflags().mh_real_field_record
}

/// The [`RealFieldTarget`] of a real field `DirectMethodHandle`, or `None` for
/// anything [`mh_real_field_access`] would not serve (no member, a method kind,
/// a missing or negative offset, an unregistered static offset): the ordinary
/// path then answers, its refusals included. Pure reads; no GC point.
pub fn real_field_target_decode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<RealFieldTarget> {
    let Value::Object(Some(member)) = ctx.get_field_by_name(this, "member") else {
        return None;
    };
    let Value::Int(flags) = ctx.get_field_by_name(member, "flags") else {
        return None;
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if !(1..=4).contains(&ref_kind) {
        return None;
    }
    let Value::Object(Some(member_type)) = ctx.get_field_by_name(member, "type") else {
        return None;
    };
    let is_static = ref_kind == 2 || ref_kind == 4;
    let offset_field = if is_static {
        "staticOffset"
    } else {
        "fieldOffset"
    };
    let offset = match ctx.get_field_by_name(this, offset_field) {
        Value::Long(v) if v >= 0 => v as usize,
        Value::Int(v) if v >= 0 => v as usize,
        _ => return None,
    };
    let fdesc = mirror_to_descriptor(&*ctx, member_type).into_owned();
    // gci cl48: against the handle's own `staticBase` (a same-named class of
    // another loader shares the offset).
    let target = if is_static {
        Some(real_static_accessor_target(ctx, this, offset)?)
    } else {
        None
    };
    // JIT round 20 wave 8c: an instance `fieldOffset` back to its slot (a
    // byte offset under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`).
    let offset = if is_static {
        offset
    } else {
        match crate::unsafe_natives_ext::unsafe_decode_field_offset(
            ctx.unsafe_byte_field_offsets(),
            offset,
        ) {
            (slot, 0) => slot,
            _ => return None,
        }
    };
    let volatile = flags & FIELD_ACC_VOLATILE != 0;
    Some(RealFieldTarget {
        ref_kind,
        offset,
        fdesc,
        target,
        volatile,
    })
}

/// Interpreter round i3 wave 73, lane L4 (`CRATONVM_MH_REAL_FIELD_RECORD`, default
/// off): a REAL field accessor (`DirectMethodHandle$Accessor` / `$StaticAccessor`,
/// what a `Lookup` field finder answers under `--real-jdk`) called by `invokeExact`
/// at its declared type (`call_site` equal to the record's `declared`), or by
/// `invokeBasic` (`call_site` `None`, `raw_return`), is served from its memoised
/// [`RealFieldTarget`]. The ordinary path ran [`mh_real_direct_dispatch`] per call:
/// an owned class name, four by-name reads and a rendered field descriptor before
/// the access.
///
/// The same answer, in the same order: `invokeExact`'s site refusals
/// ([`mh_site_access_refusal`], then the loader half of [`mh_real_exact_refusal`],
/// [`mh_exact_loader_refusal`], all it asks of a site equal to the declared type)
/// run first, then [`mh_real_field_access_resolved`], the tail of
/// [`mh_real_field_access`] (`checkBase`'s null check and frames, the static class
/// initialization barrier, the read or write). `None` declines: the ordinary path
/// runs, nothing disturbed.
pub fn mh_real_field_record_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    extra: &[Value],
    call_site: Option<&str>,
    raw_return: bool,
) -> Option<MethodCallResult> {
    if !mh_real_field_record_enabled() || crate::nbflags().dbg_mh_dispatch {
        return None;
    }
    // A carrier handle (this model's slots) is none of this lane's: one width compare.
    if !mh_is_real_handle(&*ctx, this) {
        return None;
    }
    let record = real_handle_record(ctx, this)?;
    let field = record.field.as_ref()?;
    match call_site {
        Some(site) => {
            if site != record.declared {
                return None;
            }
            if let Some(refusal) = mh_site_access_refusal(&*ctx, Some(site)) {
                return Some(Err(refusal));
            }
            if let Some(refusal) = mh_exact_loader_refusal(ctx, this, site) {
                return Some(Err(refusal));
            }
        }
        None if !raw_return => return None,
        None => {}
    }
    Some(mh_real_field_access_resolved(
        ctx,
        field.ref_kind,
        field.offset,
        &field.fdesc,
        field.target,
        field.volatile,
        extra,
        raw_return,
    ))
}

/// `CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE` -- performance, default OFF,
/// `truthy_word` (interpreter round i3 wave 80, lane L4; proposal
/// `i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record`):
/// see [`mh_real_field_converting_serve`].
pub fn mh_real_field_converting_serve_enabled() -> bool {
    crate::nbflags().mh_real_field_converting_serve
}

/// How a getter's value reaches a converting `invoke` site
/// ([`mh_field_converting_plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldConvertingReturn {
    /// The site's return is the field's own type (only the receiver differs),
    /// or a reference field read at an `Object` site: the value as it is.
    Same,
    /// A primitive field at a reference site that its wrapper is assignable
    /// to: `asType`'s boxing, canonical as `Integer.valueOf` is.
    Box,
    /// A primitive field at a wider primitive site (JLS 5.1.2): the site's
    /// primitive descriptor character.
    Widen(u8),
}

/// What [`mh_real_field_converting_serve`] does for one (declared, site) pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldConvertingPlan {
    /// The instance getter's declared receiver descriptor, when the site's
    /// receiver parameter is another reference type: `asType` casts the
    /// receiver to it. `None` for a static getter or an equal receiver type.
    pub receiver_cast: Option<String>,
    pub ret: FieldConvertingReturn,
}

/// The primitive descriptor characters `from` widens to (JLS 5.1.2).
pub fn field_converting_widens(from: u8, to: u8) -> bool {
    match from {
        b'B' => matches!(to, b'S' | b'I' | b'J' | b'F' | b'D'),
        b'S' | b'C' => matches!(to, b'I' | b'J' | b'F' | b'D'),
        b'I' => matches!(to, b'J' | b'F' | b'D'),
        b'J' => matches!(to, b'F' | b'D'),
        b'F' => to == b'D',
        _ => false,
    }
}

/// The reference types a primitive `prim`'s wrapper is assignable to that a
/// served site may name: `Object`, the wrapper itself, `Number` for the six
/// numeric wrappers, and `Serializable` / `Comparable`, which every wrapper
/// implements.
pub fn field_converting_boxes_to(prim: u8, site_ret: &str) -> bool {
    let wrapper = match prim {
        b'Z' => "Ljava/lang/Boolean;",
        b'B' => "Ljava/lang/Byte;",
        b'S' => "Ljava/lang/Short;",
        b'C' => "Ljava/lang/Character;",
        b'I' => "Ljava/lang/Integer;",
        b'J' => "Ljava/lang/Long;",
        b'F' => "Ljava/lang/Float;",
        b'D' => "Ljava/lang/Double;",
        _ => return false,
    };
    site_ret == wrapper
        || matches!(
            site_ret,
            "Ljava/lang/Object;" | "Ljava/io/Serializable;" | "Ljava/lang/Comparable;"
        )
        || (site_ret == "Ljava/lang/Number;" && !matches!(prim, b'Z' | b'C'))
}

/// The plan for serving a GETTER whose handle type is `declared` at the
/// `invoke` site `site` (`is_static`: a `StaticAccessor`), or `None` when the
/// site needs anything but the conversions `asType` makes without
/// user-visible effects: equal arity, the receiver a reference (cast to the
/// declared class), the return the field's type, a boxing to a supertype of
/// its wrapper, a primitive widening, or a reference read at `Object`. A void
/// site, an unboxing, a narrowing and an exact site (the other lanes') decline.
/// Pure.
pub fn mh_field_converting_plan(
    declared: &str,
    site: &str,
    is_static: bool,
) -> Option<FieldConvertingPlan> {
    if declared == site {
        return None;
    }
    let (dp, dr) = split_descriptor_params(declared)?;
    let (sp, sr) = split_descriptor_params(site)?;
    let arity = usize::from(!is_static);
    if dp.len() != arity || sp.len() != arity {
        return None;
    }
    let receiver_cast = match (dp.first(), sp.first()) {
        (None, None) => None,
        (Some(d), Some(s)) if d == s => None,
        (Some(d), Some(s)) if d.starts_with('L') && (s.starts_with('L') || s.starts_with('[')) => {
            Some(d.clone())        }
        _ => return None,
    };
    let ret = if sr == dr {
        FieldConvertingReturn::Same
    } else if dr.len() == 1 {
        let prim = dr.as_bytes()[0];
        if sr.len() == 1 {
            let to = sr.as_bytes()[0];
            if !field_converting_widens(prim, to) {
                return None;
            }
            FieldConvertingReturn::Widen(to)        } else if field_converting_boxes_to(prim, &sr) {
            FieldConvertingReturn::Box        } else {
            return None;        }    } else if (dr.starts_with('L') || dr.starts_with('[')) && sr == "Ljava/lang/Object;" {
        FieldConvertingReturn::Same    } else {
        return None;
    };
    Some(FieldConvertingPlan { receiver_cast, ret })}

/// A raw primitive field value widened to the site's primitive `to`
/// ([`field_converting_widens`] already admitted the pair). `Value::Int`
/// carries `byte` / `short` / `char` / `int`.
pub fn field_converting_widen(v: Value, to: u8) -> Value {
    match (v, to) {
        (Value::Int(i), b'S' | b'I') => Value::Int(i),
        (Value::Int(i), b'J') => Value::Long(i64::from(i)),
        (Value::Int(i), b'F') => Value::Float(i as f32),
        (Value::Int(i), b'D') => Value::Double(f64::from(i)),
        (Value::Long(l), b'F') => Value::Float(l as f32),
        (Value::Long(l), b'D') => Value::Double(l as f64),
        (Value::Float(f), b'D') => Value::Double(f64::from(f)),
        (other, _) => other,
    }
}

/// The declared receiver class of a real instance accessor: its `type()`'s
/// first parameter. Pure reads.
pub fn mh_real_declared_receiver_class(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ClassId> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") else {
        return None;
    };
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") else {
        return None;
    };
    if ctx.array_length(ptypes) == 0 {
        return None;
    }
    let Value::Object(Some(mirror)) = ctx.get_array_element(ptypes, 0) else {
        return None;
    };
    ctx.class_id_from_mirror(mirror)}

/// Does `this`'s `asTypeCache` hold an adapter of EXACTLY the site's
/// `MethodType` -- the row the site-type table filed for `call_site` when the
/// JDK's `asType` ran ([`mh_site_method_type`])? The JDK's `asType` would
/// answer that adapter (`asTypeCached` compares the type by identity). Pure
/// reads; `false` whenever the table has no row (the table off, a caller of a
/// loader that is not built in, or the first call).
pub fn mh_as_type_cache_holds_site(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    call_site: &str,
) -> bool {
    if !mh_site_type_table_enabled() {
        return false;
    }
    let Some(caller) = ctx.frame_class_ids_tail(1).0.first().copied() else {
        return false;
    };
    let Some(key) = mh_site_type_key(ctx.loader_id_of_class_definite(caller), call_site) else {
        return false;
    };
    let Some(site_mt) = ctx.bound_method_type_get(&key) else {
        return false;
    };
    let Value::Object(Some(cached)) = ctx.get_field_by_name(this, "asTypeCache") else {
        return false;
    };
    matches!(ctx.get_field_by_name(cached, "type"), Value::Object(Some(t)) if t == site_mt)}

