#[allow(unused_imports)]
use super::*;

/// `bindTo`'s pre-link (wave 67, `CRATONVM_MH_BINDTO_PRELINK_FIELD`): write
/// on the fresh bound copy `mh` (current, fully minted: `type`, `MH_BOUND`
/// and the field carry written) the bound link its first `invokeExact` would
/// write, through the same admission ([`field_link_candidate`] and
/// [`mh_field_link_try_bound`] with [`BoundLinkCall::BindTo`]). The link
/// names the bound receiver's exact class, which every linked call re-checks;
/// a reference setter that needs a value check is left to its first call.
/// Performs no allocation and no Java call; cannot throw.
pub fn mh_bindto_prelink_bound_field(ctx: &mut dyn NativeContext, mh: ObjectRef) {
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
pub fn note_compatible_field_link(class: &str, name: &str, declared: &str, is_static: bool) {
    let kind = if is_static { "static" } else { "instance" };
    eprintln!("[mh-field-link] --compatible {kind} link {class}.{name} at {declared}");
}

/// Clear a copied handle's link: the copy is a different handle (its `type`
/// is about to be restamped, or its marking changed), and whatever the source
/// was linked for does not describe it. Width-guarded.
pub fn mh_clear_field_link(ctx: &dyn NativeContext, mh: ObjectRef) {
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
pub fn mh_bindto_fixed_arity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
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
pub fn mh_bindto_shared_mint_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The adapter kinds [`bound_handle_arity_refusal`] judges: the combinator
/// kinds whose `type` is measured authoritative
/// ([`kind_has_authoritative_adapted_type`]) that are not direct members
/// (round 15 wave 5, lane mh4).
pub fn mh_adapter_arity_kind(kind: i32) -> bool {
    kind_has_authoritative_adapted_type(kind) && !kind_has_authoritative_type(kind)
}

/// `CRATONVM_MH_INVOKE_ADAPTER_ARITY` -- default ON (round 15 wave 5, lane
/// mh4; `r15w4-mh3-adapter-invoke-has-no-arity-refusal-FIXED-20260930.md`);
/// `0`/`false`/`off`/`no` stops [`bound_handle_arity_refusal`] judging the
/// adapter kinds and makes `insertArguments(t, pos)` with no values mint an
/// INSERT adapter again instead of answering `t`. Both modes. Read only on the
/// cold paths (about to refuse; an empty `insertArguments`).
pub fn mh_invoke_adapter_arity_enabled() -> bool {
    true
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
pub fn bound_handle_arity_refusal(
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
        true
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
pub fn bindto_takes_insert_adapter(kind: i32, already_bound: bool, desc: &str) -> bool {
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
pub fn bindto_null_takes_insert_adapter(kind: i32, recv: Value, desc: &str) -> bool {
    matches!(recv, Value::Object(None))
        && mh_bound_role(kind, desc) == Some(BoundRole::Capture)
}

/// What a handle's `MH_BOUND` slot holds, by kind (round 15 wave 5, lane mh4;
/// proposal MH3-3 of `jit-r15w4-mh3-proposals-RETIRED-20261003.md`). One place a new kind states
/// it, instead of each reader re-deriving it from the kinds it happens to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundRole {
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
pub fn mh_bound_role(kind: i32, desc: &str) -> Option<BoundRole> {
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

/// `bindto_shared_mint_applies` for a caller that has ALREADY decoded the
/// three strings (JIT round 16, lane mh): `bindTo` reads `MH_CLASS` /
/// `MH_NAME` / `MH_DESC` through `mh_read_class` / `_name` / `_desc` (the same
/// `read_string` of the same slot) a few lines earlier, with no GC point
/// between, so `strings_read` is that answer and the three decodes (a Rust
/// `String` each, every bind) are not paid twice.
pub fn bindto_shared_mint_applies_read(
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

/// `bindto_bound_type_descriptor` with the `type` render `bindTo` already made
/// for its leading-parameter guard (`rendered_type`, round 16 lane mh): the
/// same `methodtype_to_descriptor` of the same `type` object, so the bound
/// type of a non-virtual handle no longer renders it (a mirror-to-name
/// resolution per parameter) a second time per bind. `None` renders it here.
pub fn bindto_bound_type_descriptor_with(
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
pub fn mh_bound_type_table_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_MH_ARRAY_SETTER_COMPONENT_CAST` -- default ON (JIT round 16,
/// lane mh; a correctness fix, both modes): an `arrayElementSetter` handle's
/// reference value is cast to the handle's own component before the access
/// (`ClassCastException`, as HotSpot's `asType`), not refused by the store
/// (`ArrayStoreException`). `0`/`false`/`off`/`no` restores the store-only
/// refusal. Read once.
pub fn mh_array_setter_component_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
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
pub fn bound_method_type_shareable(ctx: &dyn NativeContext, desc: &str) -> bool {
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
pub fn bound_type_component_shareable(ctx: &dyn NativeContext, name: &str) -> bool {
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
pub fn is_primitive_descriptor_name(name: &str) -> bool {
    matches!(
        name,
        "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short" | "void"
    )
}

/// A name only the hidden-class or spinning machinery produces (`$$`, `/0x`,
/// a `.`); never a row.
pub fn is_spun_looking_descriptor_name(name: &str) -> bool {
    name.contains("$$") || name.contains("/0x") || name.contains('.')
}

/// By its NAME alone, is the reference component `name` a `java/...` class,
/// an array of one, or a primitive array? Only built-in loaders may define
/// `java.*` (a user loader is refused the package), so the name is the class.
pub fn bound_type_component_java_named(name: &str) -> bool {
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
pub fn bound_type_components(desc: &str) -> Option<Vec<String>> {
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
pub fn mh_bound_type_table_app_classes_enabled() -> bool {
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
pub fn bound_type_component_builtin_loaded(ctx: &dyn NativeContext, name: &str) -> bool {
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
pub fn bound_method_type_from_descriptor(
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
pub fn bound_method_type_through_table(
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
pub fn method_type_through_table_near(
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
pub fn bound_method_type_table_miss(
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
pub fn bound_type_descriptor_java_named(desc: &str) -> bool {
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
pub fn bound_type_class_key_ids(
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
pub fn descriptor_component_class_id(
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
pub fn bound_type_class_key_admissible(
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
pub fn mh_loader_scoped_types_enabled() -> bool {
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
pub fn bound_type_resolution_near(
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
pub fn bound_method_type_through_class_key(
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
pub fn alloc_bound_method_handle(
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
pub fn mh_bindto_fast_mint_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The source slots a fast-mint line is keyed on, after the source's address:
/// every slot `bindTo`'s ordinary mint reads (`MH_CLASS` / `MH_NAME` /
/// `MH_DESC` decoded and shared, `MH_KIND`, `MH_BOUND` for "already bound" and
/// the static owner, `MH_VARARGS` for the marking, `MH_SPECIAL_CALLER`,
/// `MH_CAST_CHAIN` for the chain); `type` (by name), which the guard renders,
/// follows them in the key. Any of them rewritten in place is a miss.
pub const BINDTO_MINT_GUARD_SLOTS: [usize; 8] = [
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
pub const BINDTO_MINT_MEMO_SLOTS: usize = 16;

/// The words a fast-mint line is keyed on, read off the source on entry to
/// `bindTo` ([`bindto_fast_mint_probe`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindToMintKey {
    pub vm: usize,
    pub epoch: u64,
    pub src: usize,
    /// [`BINDTO_MINT_GUARD_SLOTS`] then `type`, as [`bindto_mint_word`]s.
    pub guard: [u64; 9],
}

/// What the slow path answered for a [`BindToMintKey`].
#[derive(Clone, Debug)]
pub struct BindToMintAnswer {
    pub kind: i32,
    /// The bound handle's `type` descriptor (`bindto_bound_type_descriptor_with`).
    pub type_desc: std::rc::Rc<str>,
    /// The variable-arity bit `alloc_bound_method_handle` writes.
    pub collects: bool,
    /// The recorded owner a bound `STATIC` handle keeps (`MH_STATIC_OWNER`).
    pub static_owner: Option<u32>,
    /// The bound value's class the cast admitted on a positive reading, or
    /// `None`: the leading parameter is one the cast never judges.
    pub recv_class: Option<u32>,
    /// What the slow path carried to a bound instance field copy (wave 65's
    /// [`BoundFieldCarry`]; a line holds one under
    /// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`, wave 66). A hit re-reads it
    /// off the source ([`bindto_fast_mint_refresh_carry`]).
    pub field_carry: Option<BoundFieldCarry>,
    /// The bound field link the slow path's pre-link wrote on its copy
    /// (wave 68, `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`), replayed by a hit
    /// ([`bindto_fast_mint_replay_prelink`]); `None` when it wrote none.
    pub prelink: Option<BoundPrelinkMemo>,
    /// A second admitted class of the same line and its pre-link (wave 69,
    /// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`; [`bindto_mint_answer_merged`]),
    /// or `None`. Only ever set beside a `Some` `recv_class`.
    pub alt: Option<BindToMintAlt>,
}

/// The receiver-dependent words of a second class a fast-mint line admits
/// (interpreter round i2 wave 69, lane L4; proposal
/// `i68-L4-proposal-fast-mint-line-keeps-a-second-receiver-class-20261101`).
/// Every other word of a [`BindToMintAnswer`] depends on the line's key alone
/// (the source's slots in one collection epoch), so a line filled for class A
/// and then for class B of the same key differs only in these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindToMintAlt {
    /// The class the slow path's cast admitted on a positive reading.
    pub recv_class: u32,
    /// The pre-link remembered for a receiver of that class.
    pub prelink: Option<BoundPrelinkMemo>,
}

/// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES` (default OFF, opt-in,
/// performance; interpreter round i2 wave 69, lane L4): a fast-mint line
/// refilled for the same key with a second admitted class keeps the first as
/// [`BindToMintAnswer::alt`] instead of losing it, so a source bound
/// alternately to receivers of two classes hits every time.
pub fn mh_bindto_fast_mint_two_classes_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_two_classes
}

/// Do two answers for the same key agree on every receiver-independent word
/// (kind, bound type, marking, static owner, the carry's owner)? They always
/// should; a disagreement keeps only the newer answer. Pure.
pub fn bindto_mint_answers_share_shape(a: &BindToMintAnswer, b: &BindToMintAnswer) -> bool {
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
pub fn bindto_mint_answer_merged(
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
pub fn bindto_mint_answer_for_class(answer: BindToMintAnswer, cid: u32) -> Option<BindToMintAnswer> {
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
pub struct BoundPrelinkMemo {
    /// The carry's `link_state` the link was derived from: a fresh copy's
    /// `old` state, hence its minted bit and relink count.
    pub carry_state: i32,
    /// The packed [`MH_FIELD_LINK`] state written (`FL_LINKED | FL_BOUND`,
    /// never `FL_VALUE_CHECK`, `FL_STATIC` or `FL_OWNER_ADMITS`).
    pub state: i32,
    /// [`MH_FIELD_LINK_CLASS`]: the bound receiver's exact class.
    pub class_raw: i32,
    /// [`MH_FIELD_LINK_SITE`]: the interned declared type.
    pub site_id: i32,
}

/// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK` (default OFF, opt-in, performance;
/// interpreter round i2 wave 68, lane L4): see [`BoundPrelinkMemo`]. Needs
/// `CRATONVM_MH_BINDTO_PRELINK_FIELD` (the link it replays is that switch's).
pub fn mh_bindto_fast_mint_prelink_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_prelink
}

/// What a fast-mint line may remember of the link the slow path's pre-link
/// just wrote on its fresh bound copy `mh` (current), derived from `carry`,
/// when `on`: a bound, unchecked instance link whose `type` is the copy's
/// own. `None` otherwise (nothing linked, or a shape a hit must re-derive).
/// Reads only.
pub fn bindto_prelink_memo_of_with(
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
pub fn bindto_prelink_memo_of(
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
pub fn bindto_fast_mint_replay_prelink_with(
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
pub fn bindto_fast_mint_replay_prelink(
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
pub fn mh_bindto_fast_mint_fields_enabled() -> bool {
    crate::nbflags().mh_bindto_fast_mint_fields
}

/// Does `bindTo`'s ordinary mint of an unbound, chain-free source of `kind`
/// fill a fast-mint line? The direct kinds always; an instance field getter /
/// setter when `fields` (wave 66). A static field accessor never reaches the
/// fill (`bindTo` refuses a static getter and adapts a static setter). Pure.
pub fn bindto_fast_mint_fills_kind(kind: i32, fields: bool) -> bool {
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
pub fn bindto_fast_mint_refresh_carry(
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
pub fn bindto_fast_mint_refresh_carry_with(
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
pub struct BindToMintLine {
    pub key: BindToMintKey,
    pub answer: BindToMintAnswer,
}

thread_local! {
    /// Per-thread fast-mint lines (MHF-2), direct-mapped by the source's
    /// address. A context reporting VM identity `0` (the mocks) is never
    /// memoised outside the unit tests' own override.
    pub static BINDTO_MINT_MEMO: std::cell::RefCell<Vec<Option<BindToMintLine>>> =
        std::cell::RefCell::new(vec![None; BINDTO_MINT_MEMO_SLOTS]);
}

/// The VM identity the fast mint keys on, or `None` when it is off (the switch,
/// or a context with no identity).
pub fn bindto_fast_mint_vm(ctx: &dyn NativeContext) -> Option<usize> {
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
pub fn bindto_mint_word(v: Value) -> u64 {
    match v {
        Value::Object(Some(o)) => o.as_ptr() as u64,
        Value::Object(None) => 0,
        Value::Int(i) => (1u64 << 63) | u64::from(i as u32),
        _ => u64::MAX,
    }
}

/// The fast-mint key of `src` on entry to `bindTo`, or `None` when the fast
/// mint is off or `src` is narrower than the shared mint's layout. Reads only.
pub fn bindto_fast_mint_probe(ctx: &dyn NativeContext, src: ObjectRef) -> Option<BindToMintKey> {
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

pub fn bindto_mint_line_index(src: usize) -> usize {
    (src >> 3) & (BINDTO_MINT_MEMO_SLOTS - 1)
}

/// The memoised answer for `key` and the bound value `recv`, or `None` (the
/// slow path). A hit needs the same key and a non-null, non-array `recv` the
/// line admits (any, or exactly the admitted class). Reads only.
pub fn bindto_fast_mint_hit(
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
pub fn bindto_fast_mint_admits(
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
pub fn bindto_fast_mint_fill(ctx: &dyn NativeContext, key: BindToMintKey, answer: BindToMintAnswer) {
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
pub fn bindto_fast_mint_finish(
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
pub fn bound_static_raw_desc(ctx: &dyn NativeContext, mh: ObjectRef, raw: &str) -> Option<String> {
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
pub fn bound_capture_raw_desc(ctx: &dyn NativeContext, mh: ObjectRef, raw: &str) -> Option<String> {
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
pub fn mh_astype_capture_raw_desc_enabled() -> bool {
    true
}

/// The registered `MethodHandle.invoke` native past its refusals: the
/// `asType` adaptation and the dispatch lanes, in order (moved out unchanged
/// in JIT round 17 wave 3, lane mh4, so the caller can cast the result).
/// `args` are the call's arguments after the handle.
pub fn mh_invoke_registered_body(
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
pub fn mh_interface_null_receiver_message_enabled() -> bool {
    true
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
pub fn mh_interface_null_receiver_npe(
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
pub fn mh_real_invoke_with_arguments_door(
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
pub fn mh_real_iwa_direct(
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
pub fn mh_real_iwa_direct_element(
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

/// NATIVE21-4: the component of a PRIMITIVE trailing array parameter
/// (`[I`, `[J`, ...): its descriptor letter and the element type of the array
/// [`mh_real_iwa_varargs_direct`] allocates. `None` for a reference array
/// (`[Lp/C;`, `[[I`) or anything else.
pub fn mh_iwa_primitive_component(trailing: &str) -> Option<(u8, ArrayElementType)> {
    let bytes = trailing.as_bytes();
    if bytes.len() != 2 || bytes[0] != b'[' {
        return None;
    }
    let element_type = match bytes[1] {
        b'Z' => ArrayElementType::Boolean,
        b'B' => ArrayElementType::Byte,
        b'C' => ArrayElementType::Char,
        b'S' => ArrayElementType::Short,
        b'I' => ArrayElementType::Int,
        b'J' => ArrayElementType::Long,
        b'F' => ArrayElementType::Float,
        b'D' => ArrayElementType::Double,
        _ => return None,
    };
    Some((bytes[1], element_type))}

/// NATIVE21-4: `value` unboxed as the primitive `primitive` (a descriptor
/// letter) when it is EXACTLY that primitive's wrapper -- what
/// `asType(Object -> primitive)` does without a widening conversion or a
/// failure. `None` declines (a `null`, which the adapter's unboxing turns into
/// a `NullPointerException`; another wrapper, which widens or raises a
/// `ClassCastException`; anything that is not a box), so the JDK's own body
/// answers. Reads only; no GC point.
pub fn mh_iwa_exact_unboxed(ctx: &mut dyn NativeContext, primitive: u8, value: Value) -> Option<Value> {
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
    // The value an array element of this component stores: an `int` for the
    // sub-int kinds, else the kind's own variant.
    let raw = crate::lang_class::unbox_value(&*ctx, object);
    match (primitive, raw) {
        (b'I' | b'Z' | b'B' | b'S' | b'C', Value::Int(_))
        | (b'J', Value::Long(_))
        | (b'F', Value::Float(_))
        | (b'D', Value::Double(_)) => Some(raw),
        _ => None,
    }
}

/// `CRATONVM_MH_IWA_VARARGS_DIRECT` -- performance, default ON, `=0` kill (JIT
/// round 21, lane native; page
/// `jr21-iwa-small-varargs-call-runs-the-collector-lambda-forms-FIXED-20261015.md`).
pub fn mh_iwa_varargs_direct_enabled() -> bool {
    crate::nbflags().mh_iwa_varargs_direct
}

/// `AsVarargsCollector.invokeWithArguments`' `super.invokeWithArguments(..)`
/// (its `<= 127` path) on a collector whose fixed-arity `target` is a REAL
/// `DirectMethodHandle` of a static, virtual or interface method with a
/// trailing array (a reference component, or since JIT tier2 wave 5, lane nat21,
/// NATIVE21-4 a primitive one whose elements are exact wrappers): the JDK body builds
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
pub fn mh_real_iwa_varargs_direct(
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
    // NATIVE21-4 (JIT tier2 wave 5, lane nat21): a PRIMITIVE component
    // (`int...`, `long...`, ...) is collected in place too, each trailing
    // element an exact wrapper of the component (`mh_iwa_exact_unboxed`).
    let primitive_component = mh_iwa_primitive_component(trailing);
    let (component, comp_cid) = if primitive_component.is_some() {
        // The trailing parameter IS `arrayType` (see above); a mirror that
        // names another array class vetoes. A mirror this model cannot name
        // as a descriptor does not.
        if let Some(named) = strict_mirror_descriptor(&*ctx, array_type) {
            if named.starts_with('[') && named != trailing {
                return None;
            }
        }
        ("", ClassId::new(0))
    } else {
        let component = trailing.strip_prefix("[L")?.strip_suffix(';')?;
        let comp_cid = mirror_class_id(&*ctx, array_type)
            .and_then(|arr_cid| ctx.array_component_class_id(arr_cid))?;
        if ctx.class_name_arc_of_id(comp_cid).as_deref() != Some(component) {
            return None;
        }
        (component, comp_cid)
    };
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
    // The unboxed trailing elements of a primitive component (primitives hold
    // no reference, so they survive the allocation below as they are).
    let mut unboxed: Vec<Value> = Vec::new();
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
        if let Some((primitive, _)) = primitive_component {
            unboxed.push(mh_iwa_exact_unboxed(ctx, primitive, value)?);
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
    let fresh = match primitive_component {
        Some((_, element_type)) => ctx.try_new_array(element_type, collected)?,
        None => ctx.try_new_ref_array(comp_cid, collected)?,
    };
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
        let value = match unboxed.get(j) {
            Some(primitive) => *primitive,
            None => ctx.get_array_element(arr_now, lead + j),
        };
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
pub fn first_descriptor_param(desc: &str) -> Option<&str> {
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
pub const MH_CLASS_FOR_IWA_BODY: &str = "java/lang/invoke/MethodHandle";

pub const IWA_ADAPT_MEMO_SLOTS: usize = 64;

pub struct IwaAdaptLine {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub n: usize,
    pub adapted: Option<ObjectRef>,
}

thread_local! {
    /// Per-thread memo of `this.asType(genericMethodType(n))` for [`mh_real_iwa_adapted`],
    /// keyed by the handle's address and `n`, valid for one collection epoch under one VM
    /// (the argument of [`REAL_HANDLE_MEMO`]: nothing moves or is freed within an epoch,
    /// and a handle's `type` is final, so the adapted handle of an equal address is the
    /// same object's). A context that reports VM identity `0` (the mocks) is never memoised.
    pub static IWA_ADAPT_MEMO: std::cell::RefCell<Vec<IwaAdaptLine>> =
        std::cell::RefCell::new(
            (0..IWA_ADAPT_MEMO_SLOTS)
                .map(|_| IwaAdaptLine { vm: 0, epoch: 0, addr: 0, n: 0, adapted: None })
                .collect(),
        );
}

/// `this.asType(MethodType.genericMethodType(n))` through the JDK's own bodies (its
/// `genericMethodType` cache and single-slot `asType` cache), memoised per collection
/// epoch so the two by-name Java calls are paid on a miss only. `Ok(None)` declines.
pub fn mh_real_iwa_adapted(
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
pub fn mh_iwa_null_base_of_instance_accessor(
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

pub fn mh_real_invoke_with_arguments_lanes(
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
            ctx.array_length(a)        }
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
pub fn iwa_served_check_customized(
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
pub fn iwa_served_uncount(ctx: &dyn NativeContext, adapter: ObjectRef, previous: i32) {
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
pub fn mh_real_iwa_served_lanes(
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
    // NATIVE21-5, census step (JIT tier2 wave 5, lane nat21): which handle
    // classes reach the spun-form lane. `CRATONVM_DBG=mh-finder-census` prints
    // `iwa.lane4-this <class>` (the handle `invokeWithArguments` was called on)
    // and `iwa.lane4-adapted <class>` (its `asType(genericMethodType(n))`)
    // with counts. Off, one relaxed load.
    if crate::mh_finder_census::enabled() {
        let class_text = |ctx: &dyn NativeContext, handle: ObjectRef| -> String {
            ctx.class_name_arc_of_id(ctx.class_id_of_object(handle))
                .map_or_else(|| "?".to_string(), |name| name.to_string())
        };
        let this_now = ctx.read_native_pin(this_pin, this);
        let this_class = class_text(&*ctx, this_now);
        let adapted_class = class_text(&*ctx, adapted);
        crate::mh_finder_census::event_named("iwa.lane4-this", &this_class);
        crate::mh_finder_census::event_named("iwa.lane4-adapted", &adapted_class);
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

pub const REAL_BIND_MEMO_SLOTS: usize = 64;

#[derive(Clone, Copy)]
pub struct RealBindLine {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub filled: bool,
    pub proto: Option<ObjectRef>,
    pub form: Option<ObjectRef>,
    pub class: Option<ClassId>,
    pub x_class: Option<ClassId>,
    /// A second bound-value class the JDK body admitted for this same receiver in this
    /// same epoch (JIT round 21 wave 19, lane mhbind2; [`mh_bindto_real_memo_two_classes_enabled`]),
    /// or `None`. Both passed the receiver's `leadingReferenceParameter().cast(x)`, which is
    /// a function of the receiver's `final` `type` and the value's class only.
    pub x_class_alt: Option<ClassId>,
    /// The species' field slots, resolved when the line was filed (JIT round 21 wave 17,
    /// lane mhbind; MHB-3, [`mh_bindto_real_mint_slots_enabled`]), or `None`: the mint
    /// resolves each field by name.
    pub slots: Option<RealBindSlots>,
}

/// The most bound arguments a species' [`RealBindSlots`] keeps (a wider species mints
/// by name).
pub const REAL_BIND_MAX_ARGS: usize = 8;

/// The slots of a `BoundMethodHandle` species' `type`, `form` and `arg<T><i>` fields
/// (JIT round 21 wave 17, lane mhbind; MHB-3): the mint stores by slot, without a
/// `format!` and two by-name resolutions per bound argument per `bindTo`.
#[derive(Clone, Copy)]
pub struct RealBindSlots {
    pub type_slot: usize,
    pub form_slot: usize,
    pub args: [usize; REAL_BIND_MAX_ARGS],
    /// Each bound argument's field descriptor byte (its species letter:
    /// `L`, `I`, `J`, `F` or `D`), for the batched mint (j27 wave 2, lane
    /// INVOKE; [`mh_bindto_real_mint_batch_enabled`]).
    pub desc: [u8; REAL_BIND_MAX_ARGS],
    pub count: usize,
}

/// `CRATONVM_MH_BINDTO_REAL_MINT_BATCH` -- perf, default ON, `=0` off (j27 wave 2,
/// lane INVOKE; page `docs/feature-designs/native-dispatch-architecture.md`,
/// `bind-only`): a memo-hit mint ([`mh_real_bind_to_mint_slots`]) reads the
/// prototype's fields in ONE typed batch and stores the fresh species' fields in
/// ONE typed batch ([`real_bind_mint_batched`]), instead of one descriptor-aware
/// door per field (two reads and four stores for `Species_LL`, each paying its own
/// forwarding, corrupt-cell watch and descriptor lookup). Same values, same slots,
/// same order. Asked per hit: memoised.
pub fn mh_bindto_real_mint_batch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The field descriptor byte of a `BoundMethodHandle` species letter, or `None`
/// for a letter no species uses.
pub fn species_letter_descriptor(letter: char) -> Option<u8> {
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
/// `docs/feature-designs/native-dispatch-architecture.md`): a repeat
/// `bindTo` of a REAL handle ([`mh_real_bind_to`]) mints its species instance by the
/// slots resolved when the memo line was filed ([`RealBindSlots`]). Read when a line is
/// filed (the JDK body's answer), never on a hit.
pub fn mh_bindto_real_mint_slots_enabled() -> bool {
    true
}

/// `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES` -- perf, default ON, `=0` off (JIT round 21
/// wave 19, lane mhbind2; page `docs/feature-designs/native-dispatch-architecture.md`,
/// "Round 21 wave 19"): a [`mh_real_bind_to`] memo line keeps a SECOND bound-value class.
/// A line held one class, so `h.bindTo(x)` over values of two alternating classes
/// (`R21MhbindRows bind-each-mixed`: `Acc`, `Acc2`, `Acc`, ...) missed on every call and ran
/// the JDK body (8-11 us here) each time, then re-filed the line for the other class. When
/// the JDK body answers for the same receiver (same VM, epoch and address as the line it
/// replaces), the replaced line's class is kept as the alternate: it passed the same
/// receiver's cast, whose answer depends only on the receiver's `final` `type` and the
/// value's class. Read when a line is filed, never on a hit.
pub fn mh_bindto_real_memo_two_classes_enabled() -> bool {
    true
}

/// The [`RealBindSlots`] of species `class` (named `BoundMethodHandle$Species_<letters>`),
/// or `None` when a field does not resolve or the species is wider than
/// [`REAL_BIND_MAX_ARGS`]. Cold: runs when a memo line is filed.
pub fn real_bind_slots_of(ctx: &dyn NativeContext, class: ClassId) -> Option<RealBindSlots> {
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
    pub static REAL_BIND_MEMO: std::cell::RefCell<Vec<RealBindLine>> =
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

pub fn mh_real_bind_to(
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
pub fn mh_real_bind_to_mint(
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
pub fn mh_real_bind_to_mint_slots(
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
pub fn real_bind_mint_batched(
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
// Under `--real-jdk` every `Lookup` finder is the JDK's own bytecode, and a
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
// bench `L4W74FinderMintSplitBench.java`, probe
// `L4W74MethodTypeIntern.java`.
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
// `L4W75InternMemoMissBench.java`.

/// `java/lang/invoke/MethodType`.
pub const MT_INTERN_CLASS: &str = "java/lang/invoke/MethodType";
/// `MethodType.makeImpl(Class<?> rtype, Class<?>[] ptypes, boolean trusted)`.
pub const MT_INTERN_DESC: &str = "(Ljava/lang/Class;[Ljava/lang/Class;Z)Ljava/lang/invoke/MethodType;";
/// Lines of a fresh [`MT_INTERN_MEMO`] (a power of two; two ways per set).
pub const MT_INTERN_MEMO_SLOTS: usize = 128;
/// The most lines a thread's memo grows to (a power of two). Wave 75: a
/// thread that keeps re-interning more types than its memo holds (the
/// thrash the victim bitmap counts) grows it fourfold, up to this cap
/// (~360 KiB of lines on a 64-bit host); a thread that interns a stream of
/// types once each (a boot) has no thrash and keeps [`MT_INTERN_MEMO_SLOTS`].
pub const MT_INTERN_MEMO_MAX_SLOTS: usize = 4096;
/// Bits of each victim bitmap generation (a power of two): one bit per hash
/// of a key evicted while live, so a miss can tell "evicted here before"
/// (thrash: a bigger memo would have answered) from "never seen" (a cold
/// miss). Two generations of [`MT_INTERN_VICTIM_GENERATION`] evictions each
/// keep the false-positive rate of a cold miss at most ~12% (6% x 2).
pub const MT_INTERN_VICTIM_BITS: usize = 16384;
/// Evictions per victim bitmap generation.
pub const MT_INTERN_VICTIM_GENERATION: usize = 1024;
/// The most parameter types a memo key holds; a longer type runs the body.
pub const MT_INTERN_MAX_PARAMS: usize = 6;

/// `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO` -- performance, default OFF,
/// `truthy_word`. Read at registration (the `Intrinsic` exists only while it
/// is on, and, since wave 75, only in a VM without a JIT:
/// [`mh_method_type_intern_memo_registers`]); see the section comment above.
pub fn mh_method_type_intern_memo_enabled() -> bool {
    crate::nbflags().mh_method_type_intern_memo
}

/// Does `r`'s VM get the `makeImpl` intrinsic? The switch, and no JIT.
///
/// Wave 75 (host, `L4W74FinderMintSplitBench --real-jdk`, 5 reps): with the
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
pub fn mh_method_type_intern_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_method_type_intern_memo_enabled()
        && r.interpreted_intrinsic_registers_row(MT_INTERN_BODY_ROW)
}

/// The memo's row token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (wave 80):
/// listed, a JIT VM registers it and marks `makeImpl` body-compilable without
/// the global switch.
pub const MT_INTERN_BODY_ROW: &str = "method-type-intern";

/// The components of one `makeImpl` call: the addresses of the return-type
/// mirror and of the first `len` parameter mirrors (the rest zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MtInternKey {
    pub rtype: usize,
    pub len: usize,
    pub params: [usize; MT_INTERN_MAX_PARAMS],
}

#[derive(Clone, Copy)]
pub struct MtInternLine {
    pub epoch: u64,
    pub key: MtInternKey,
    pub mt: ObjectRef,
}

/// One thread's `makeImpl` memo: two-way sets of lines, valid for one VM
/// (`vm`; another VM's first filing resets the memo) and, line by line, for
/// the collection epoch the line was filed in.
pub struct MtInternMemo {
    pub vm: usize,
    /// `2 * sets` lines; set `s` is lines `2s` (the more recent) and `2s+1`.
    pub lines: Vec<Option<MtInternLine>>,
    /// The current victim generation: one bit per hash of a key evicted
    /// while live (same epoch).
    pub victims: Vec<u64>,
    /// The previous generation (consulted, never set).
    pub victims_old: Vec<u64>,
    /// Evictions recorded in `victims`.
    pub victim_count: usize,
    /// Misses since the last growth or window reset.
    pub misses: usize,
    /// Of `misses`, those on a key a victim generation remembers.
    pub thrash: usize,
}

impl MtInternMemo {
    pub fn new() -> Self {
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
    pub fn set_of(&self, hash: usize) -> usize {
        2 * (hash & (self.lines.len() / 2 - 1))
    }

    /// The victim bit of `hash` (other bits than the ones that pick a set).
    pub fn victim_bit(hash: usize) -> usize {
        hash.rotate_right(17) & (MT_INTERN_VICTIM_BITS - 1)
    }

    pub fn get(&mut self, vm: usize, epoch: u64, key: &MtInternKey) -> Option<ObjectRef> {
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
    pub fn note_miss(&mut self, vm: usize, key: &MtInternKey) {
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
    pub fn should_grow(&self) -> bool {
        self.lines.len() < MT_INTERN_MEMO_MAX_SLOTS
            && self.thrash >= self.lines.len() / 4
            && 2 * self.thrash >= self.misses
    }

    pub fn put(&mut self, vm: usize, epoch: u64, key: MtInternKey, mt: ObjectRef) {
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

    pub fn note_victim(&mut self, hash: usize) {
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
    pub fn grow(&mut self, epoch: u64) {
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
    pub static MT_INTERN_MEMO: std::cell::RefCell<MtInternMemo> =
        std::cell::RefCell::new(MtInternMemo::new());
    /// `MethodType`'s class id in the VM of identity `.0` (a boot class: never
    /// unloaded), so the body runs without a by-name class lookup.
    pub static MT_INTERN_CLASS_ID: std::cell::Cell<Option<(usize, ClassId)>> =
        const { std::cell::Cell::new(None) };
    /// `makeImpl`'s declared-method index in `MethodType` of the VM of
    /// identity `.0`, as the VM last handed it back through
    /// `invoke_special_bytecode_only_hinted` (wave 76, L4;
    /// `CRATONVM_NATIVE_OWN_BODY_HINT`, which the VM reads: off, it hands
    /// back `None` and nothing is kept).
    pub static MT_INTERN_BODY_HINT: std::cell::Cell<Option<(usize, ClassId, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// The memo key of a `makeImpl` call (`args` = `rtype`, `ptypes`, `trusted`),
/// or `None` when it has none: a `null` return type, array or element (the
/// body raises the NPE), a non-array, or more than [`MT_INTERN_MAX_PARAMS`]
/// parameters. Pure reads; no GC point.
pub fn mt_intern_key(ctx: &dyn NativeContext, args: &[Value]) -> Option<MtInternKey> {
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
        *slot = p.as_ptr() as usize;    }
    Some(MtInternKey {
        rtype: rtype.as_ptr() as usize,
        len,
        params,
    })
}

/// A well-mixed hash of a key: mirror addresses are aligned and close
/// together, and the LOW bits pick the set, so they are mixed in (wave 74's
/// slot was a shifted XOR of the addresses).
pub fn mt_intern_hash(key: &MtInternKey) -> usize {
    let mut h = (key.rtype as u64) ^ (key.len as u64).rotate_left(29);
    for (i, p) in key.params.iter().enumerate().take(key.len) {
        h ^= (*p as u64).rotate_left(((i as u32) * 11 + 13) % 64);
    }
    // splitmix64's finalizer.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (h ^ (h >> 31)) as usize
}

/// File `mt` for `key`, read in epoch `epoch` (the caller has checked that no
/// collection ran since).
pub fn mt_intern_memo_put(vm: usize, epoch: u64, key: MtInternKey, mt: ObjectRef) {
    MT_INTERN_MEMO.with(|m| m.borrow_mut().put(vm, epoch, key, mt));
}

/// `MethodType`'s class id in VM `vm`, once a body's answer has told it.
pub fn mt_intern_class_id(vm: usize) -> Option<ClassId> {
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
pub fn mt_make_impl_jdk_body(
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
pub fn mt_make_impl_intrinsic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
// Interpreter round i3 wave 77, lane L4: a repeat `--real-jdk` field finder is
// minted from its first answer (`CRATONVM_MH_FINDER_MINT_MEMO`, default on
// since wave 78)
// ---------------------------------------------------------------------------
//
// `lk.findGetter(Box.class, "v", int.class)` costs ~31-37 us under `--real-jdk
// --nojit` (the JDK's `resolveOrFail` + `getDirectField` interpreted: ~165
// calls and ~14 native crossings, no pathological step; page
// `interpreter-i74-L4-proposal-the-jdk-only-finder-mint-cost-model-DONE-20261014`). Its answer
// is a pure function of inputs that are fixed within a collection epoch:
// the `Lookup`'s `lookupClass`, `prevLookupClass` and `allowedModes` (all the
// access check reads), and the `refc`, `name` and `type` objects. So behind the
// switch, in a `--real-jdk` VM over a real JDK WITHOUT a JIT (wave 75: an
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
pub const FINDER_MINT_LOOKUP: &str = "java/lang/invoke/MethodHandles$Lookup";
/// The four field finders' shared descriptor.
pub const FINDER_MINT_DESC: &str =
    "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";
/// Lines of a thread's finder memo (a power of two, direct-mapped).
pub const FINDER_MINT_MEMO_SLOTS: usize = 64;
/// The words of a resolved `MemberName` (JDK 25; `vmindex` / `vmtarget` are
/// HotSpot-injected and absent from this layout).
pub const FINDER_MINT_MEMBER_WORDS: &[&str] =
    &["clazz", "name", "type", "flags", "method", "resolution"];
/// The words a `DirectMethodHandle$Accessor` copies from the filed handle
/// (`member` is the fresh `MemberName`).
pub const FINDER_MINT_ACCESSOR_WORDS: &[&str] =
    &["type", "form", "crackable", "fieldType", "fieldOffset"];
/// The words a `DirectMethodHandle$StaticAccessor` copies.
pub const FINDER_MINT_STATIC_ACCESSOR_WORDS: &[&str] = &[
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
pub fn mh_finder_mint_memo_enabled() -> bool {
    crate::nbflags().mh_finder_mint_memo
}

/// Does `r`'s VM get the four finder `Intrinsic`s? The switch, `--real-jdk`
/// over a real JDK, and no JIT -- or, wave 80, a JIT VM under
/// `CRATONVM_INTRINSIC_BODY_COMPILES` or the `finder-mint` row of
/// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (that row default ON since wave 81;
/// `interpreted_intrinsic_registers_row`), where [`register_finder_mint_memo`]
/// marks the four rows body-compilable so the finder bodies still compile.
pub fn mh_finder_mint_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_finder_mint_memo_enabled()
        && finder_mint_mode_admits(r)
        && r.interpreted_intrinsic_registers_row(FINDER_MINT_BODY_ROW)
}

/// The VMs whose `Lookup` finders run the JDK's own bodies and so may carry
/// the finder mint memos: `--real-jdk` over a real JDK, and (interpreter
/// round i5 wave 109, lane L4, `CRATONVM_MH_COMPAT_FINDER_MINT`, default off)
/// `--compatible` over a real JDK with the `JdkInvoke` row in force, where the
/// finder carriers are not registered (`compat_row_of_shadow` drops them) and
/// the same bodies run (page `i89-L4-method-finder-per-class-and-per-handle-cost`,
/// wave 109). The memos read no mode-specific state: the key, the filing rule
/// and the mint look at the `Lookup`, the arguments and the body's own answer.
/// The registrars register inside a `set_admitting_family_intrinsic` scope,
/// which the family drop keeps (any other registration of these triples is
/// refused there).
pub fn finder_mint_mode_admits(r: &NativeMethodRegistry) -> bool {
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
pub const FINDER_MINT_BODY_ROW: &str = "finder-mint";

/// One of the four `Lookup` field finders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldFinder {
    Getter,
    Setter,
    StaticGetter,
    StaticSetter,
}

impl FieldFinder {
    pub const ALL: [FieldFinder; 4] = [
        FieldFinder::Getter,
        FieldFinder::Setter,
        FieldFinder::StaticGetter,
        FieldFinder::StaticSetter,
    ];

    pub fn method_name(self) -> &'static str {
        match self {
            FieldFinder::Getter => "findGetter",
            FieldFinder::Setter => "findSetter",
            FieldFinder::StaticGetter => "findStaticGetter",
            FieldFinder::StaticSetter => "findStaticSetter",
        }
    }

    pub fn is_static(self) -> bool {
        matches!(self, FieldFinder::StaticGetter | FieldFinder::StaticSetter)
    }

    /// The class the body's answer must be, exactly, to be filed.
    pub fn accessor_class(self) -> &'static str {
        if self.is_static() {
            "java/lang/invoke/DirectMethodHandle$StaticAccessor"
        } else {
            "java/lang/invoke/DirectMethodHandle$Accessor"
        }
    }

    pub fn tag(self) -> u8 {
        match self {
            FieldFinder::Getter => 1,
            FieldFinder::Setter => 2,
            FieldFinder::StaticGetter => 3,
            FieldFinder::StaticSetter => 4,
        }
    }

    pub fn callback(self) -> fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult {
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
pub struct FinderMintKey {
    pub finder: u8,
    pub lookup_class: usize,
    pub prev_lookup_class: usize,
    pub modes: i32,
    pub refc: usize,
    pub name: usize,
    pub ty: usize,
}

/// One filed answer: valid for VM `vm` in collection epoch `epoch` only (the
/// two objects are held by address, which no collection has moved or freed
/// while the epoch holds).
#[derive(Clone, Copy)]
pub struct FinderMintLine {
    pub vm: usize,
    pub epoch: u64,
    pub key: FinderMintKey,
    pub proto: ObjectRef,
    pub proto_member: ObjectRef,
    pub accessor_class: ClassId,
    pub member_class: ClassId,
    /// The body's `form` at filing time (wave 81): a static form of
    /// `DirectMethodHandle.ACCESSOR_FORMS`, which never changes a filled slot.
    pub form: Value,
    /// A static finder's field holder and whether it was initialized when the
    /// answer was filed (wave 81); `None` for the instance finders.
    pub holder: Option<ClassId>,
    pub holder_initialized: bool,
}

thread_local! {
    /// Per-thread memo of field-finder answers (see the section comment).
    pub static FINDER_MINT_MEMO: std::cell::RefCell<Vec<Option<FinderMintLine>>> =
        std::cell::RefCell::new(vec![None; FINDER_MINT_MEMO_SLOTS]);
}

/// The memo key of a finder call (`args` = the `Lookup`, `refc`, `name`,
/// `type`), or `None` when an argument is null or the `Lookup`'s words do not
/// read as expected. Pure reads; no GC point.
pub fn finder_mint_key(
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
pub fn finder_mint_slot(key: &FinderMintKey) -> usize {
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
pub fn finder_mint_file(
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
pub fn finder_mint(
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
pub fn mh_finder_mint_intrinsic(
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
pub fn finder_mint_static_holder(ctx: &dyn NativeContext, args: &[Value]) -> (Option<ClassId>, bool) {
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
pub fn mh_find_getter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::Getter, args)
}

/// `Lookup.findSetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub fn mh_find_setter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::Setter, args)
}

/// `Lookup.findStaticGetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub fn mh_find_static_getter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::StaticGetter, args)
}

/// `Lookup.findStaticSetter` under `CRATONVM_MH_FINDER_MINT_MEMO`.
pub fn mh_find_static_setter_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_finder_mint_intrinsic(ctx, FieldFinder::StaticSetter, args)
}

/// Register the four finder `Intrinsic`s when `r`'s VM admits them
/// ([`mh_finder_mint_memo_registers`]); a no-op otherwise. States each
/// registration's kind (`register_with_kind`), so the ambient category is
/// neither read nor changed.
pub fn register_finder_mint_memo(r: &mut NativeMethodRegistry) {
    if !mh_finder_mint_memo_registers(r) {
        return;
    }
    // Wave 109 (`finder_mint_mode_admits`): a `--compatible` `JdkInvoke`
    // family drop keeps these stated `Intrinsic`s (closed after the loop).
    let admitting = r.set_admitting_family_intrinsic(true);
    for finder in FieldFinder::ALL {
        r.register(
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            FINDER_MINT_DESC,
            finder.callback(),
            cratonvm_native_api::NativeKind::Intrinsic,
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
// Interpreter round i3 wave 82, lane L4: a repeat `--real-jdk` METHOD finder
// is minted from its first answer (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`,
// default off; proposal
// `i81-L4-proposal-a-repeat-method-finder-is-minted-from-its-first-answer`)
// ---------------------------------------------------------------------------
//
// The field mint above, per method finder. `findVirtual` / `findStatic` /
// `findSpecial` / `findConstructor` / `unreflect` run the JDK's finder
// bytecode on every call (host, wave 81, JIT: ~6.5-9.6 us; `find-getter`
// under the field mint ~1.75 us). Behind the switch, in a `--real-jdk` VM
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
pub const METHOD_FINDER_MINT_BODY_ROW: &str = "method-finder-mint";
/// Lines of a thread's method finder memo (a power of two, direct-mapped).
pub const METHOD_FINDER_MINT_MEMO_SLOTS: usize = 64;
/// `findVirtual` / `findStatic`.
pub const METHOD_FINDER_DESC_NAMED: &str =
    "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// `findSpecial`.
pub const METHOD_FINDER_DESC_SPECIAL: &str = "(Ljava/lang/Class;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";
/// `findConstructor`.
pub const METHOD_FINDER_DESC_CONSTRUCTOR: &str =
    "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
/// `unreflect`.
pub const METHOD_FINDER_DESC_UNREFLECT: &str =
    "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;";

/// `CRATONVM_MH_METHOD_FINDER_MINT_MEMO` -- performance, default OFF,
/// `truthy_word`. Read at registration; see the section comment above.
pub fn mh_method_finder_mint_memo_enabled() -> bool {
    crate::nbflags().mh_method_finder_mint_memo
}

/// Does `r`'s VM get the five method finder `Intrinsic`s? The switch,
/// `--real-jdk` over a real JDK, and no JIT -- or a JIT VM under
/// `CRATONVM_INTRINSIC_BODY_COMPILES` or the `method-finder-mint` row, where
/// [`register_method_finder_mint_memo`] marks the rows body-compilable.
pub fn mh_method_finder_mint_memo_registers(r: &NativeMethodRegistry) -> bool {
    mh_method_finder_mint_memo_enabled()
        && finder_mint_mode_admits(r)
        && r.interpreted_intrinsic_registers_row(METHOD_FINDER_MINT_BODY_ROW)
}

/// One of the five `Lookup` method finders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MethodFinder {
    Virtual,
    Static,
    Special,
    Constructor,
    Unreflect,
}

impl MethodFinder {
    pub const ALL: [MethodFinder; 5] = [
        MethodFinder::Virtual,
        MethodFinder::Static,
        MethodFinder::Special,
        MethodFinder::Constructor,
        MethodFinder::Unreflect,
    ];

    pub fn method_name(self) -> &'static str {
        match self {
            MethodFinder::Virtual => "findVirtual",
            MethodFinder::Static => "findStatic",
            MethodFinder::Special => "findSpecial",
            MethodFinder::Constructor => "findConstructor",
            MethodFinder::Unreflect => "unreflect",
        }
    }

    pub fn descriptor(self) -> &'static str {
        match self {
            MethodFinder::Virtual | MethodFinder::Static => METHOD_FINDER_DESC_NAMED,
            MethodFinder::Special => METHOD_FINDER_DESC_SPECIAL,
            MethodFinder::Constructor => METHOD_FINDER_DESC_CONSTRUCTOR,
            MethodFinder::Unreflect => METHOD_FINDER_DESC_UNREFLECT,
        }
    }

    /// Distinct from the field finders' tags (1-4).
    pub fn tag(self) -> u8 {
        match self {
            MethodFinder::Virtual => 11,
            MethodFinder::Static => 12,
            MethodFinder::Special => 13,
            MethodFinder::Constructor => 14,
            MethodFinder::Unreflect => 15,
        }
    }

    pub fn callback(self) -> fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult {
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
pub enum DirectSpecies {
    Plain,
    Special,
    Interface,
    Constructor,
}

impl DirectSpecies {
    pub fn of_class_name(name: &str) -> Option<DirectSpecies> {
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
    pub fn words(self) -> &'static [&'static str] {
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
pub struct MethodFinderMintKey {
    pub finder: u8,
    pub lookup_class: usize,
    pub prev_lookup_class: usize,
    pub modes: i32,
    pub a: usize,
    pub b: usize,
    pub c: usize,
    pub d: usize,
}

/// One filed answer, valid for VM `vm` in collection epoch `epoch` only.
#[derive(Clone, Copy)]
pub struct MethodFinderMintLine {
    pub vm: usize,
    pub epoch: u64,
    pub key: MethodFinderMintKey,
    pub proto: ObjectRef,
    pub proto_member: ObjectRef,
    /// A `$Constructor`'s `initMethod`.
    pub proto_init: Option<ObjectRef>,
    pub handle_class: ClassId,
    pub member_class: ClassId,
    pub species: DirectSpecies,
    /// The body's `form` at filing time.
    pub form: Value,
    /// A static method's holder and whether it was initialized when filed.
    pub holder: Option<ClassId>,
    pub holder_initialized: bool,
}

thread_local! {
    /// Per-thread memo of method-finder answers (see the section comment).
    pub static METHOD_FINDER_MINT_MEMO: std::cell::RefCell<Vec<Option<MethodFinderMintLine>>> =
        std::cell::RefCell::new(vec![None; METHOD_FINDER_MINT_MEMO_SLOTS]);
}

/// The memo key of a method finder call (`args` = the `Lookup`, then the
/// finder's own arguments), or the reason there is none (an argument is null
/// or a word does not read as expected; the reason is counted under
/// `CRATONVM_DBG=mh-finder-census`). Pure reads; no GC point.
pub fn method_finder_mint_key(
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
pub fn method_finder_mint_slot(key: &MethodFinderMintKey) -> usize {
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
pub fn method_finder_mint_init_probe(
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
                _ => return None,            }        }
        _ => return None,
    };
    let class = ctx.class_id_from_mirror(mirror)?;
    Some((class, ctx.is_class_initialized(class)))
}

/// Is `target` `from` or one of its superclasses? Bounded walk; reads only.
pub fn method_finder_mint_is_self_or_super(
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
pub fn method_finder_mint_is_polymorphic_class(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("java/lang/invoke/MethodHandle") | Some("java/lang/invoke/VarHandle")
    )
}

