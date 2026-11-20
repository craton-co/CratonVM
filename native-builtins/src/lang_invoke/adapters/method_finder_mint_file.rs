#[allow(unused_imports)]
use super::*;

/// File `handle`, the body's answer to `key`, when it is admissible (see the
/// section comment). The caller has checked that no collection ran since the
/// key was read. Answers what happened, as a `CRATONVM_DBG=mh-finder-census`
/// event label: `method-finder.file.filed`, or the refusal's reason.
#[allow(clippy::too_many_arguments)]
pub fn method_finder_mint_file(
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
pub fn method_finder_form_kind_name(ctx: &dyn NativeContext, form: ObjectRef) -> Option<String> {
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
pub fn method_finder_mint_init_refile(
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
pub fn method_finder_mint_member(
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
pub fn method_finder_mint(
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
pub const METHOD_FINDER_SHAPE_SLOTS: usize = 16;

/// One filed shape: the line of the class that filed it (`line.key.a` is its
/// `refc` mirror), that class, and what its member resolved to.
#[derive(Clone)]
pub struct MethodFinderShapeLine {
    pub line: MethodFinderMintLine,
    pub refc: ClassId,
    pub name: std::sync::Arc<str>,
    pub descriptor: std::sync::Arc<str>,
    /// The member's declared access flags.
    pub access: u16,
}

thread_local! {
    /// Per-thread shape memo (see the section comment).
    pub static METHOD_FINDER_SHAPE_MEMO: std::cell::RefCell<Vec<Option<MethodFinderShapeLine>>> =
        std::cell::RefCell::new(vec![None; METHOD_FINDER_SHAPE_SLOTS]);
}

/// `key` with its `refc` word zeroed: the shape.
pub fn method_finder_shape_key(key: &MethodFinderMintKey) -> MethodFinderMintKey {
    MethodFinderMintKey { a: 0, ..*key }
}

pub fn method_finder_shape_slot(shape: &MethodFinderMintKey) -> usize {
    method_finder_mint_slot(shape) & (METHOD_FINDER_SHAPE_SLOTS - 1)
}

/// The class facts the shape guards compare: the defining loader (definite,
/// a user or application loader), the package name, the raw access flags and
/// the class's own `InnerClasses` flags. `None` when the class may not take
/// part (unknown loader, boot or platform loader, hidden, array, no name).
pub fn method_finder_shape_class_facts(
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
pub const SHAPE_ACC_PRIVATE_OR_PROTECTED: u16 = 0x0002 | 0x0004;

/// File `line` (just built by [`method_finder_mint_file`]) as its shape's
/// line, when it may serve another class (see the section comment). Answers a
/// census label. Reads only, no GC point.
pub fn method_finder_shape_file(
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
pub fn method_finder_shape_member(
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
pub fn method_finder_shape_mint(
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
            None => return Some(Err(shape_mint_failed("initMethod"))),        },
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
pub fn shape_mint_failed(what: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
        message: format!("Java heap space (method finder shape mint: {what})"),
    }))
}

/// A method finder under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`: a minted
/// repeat answer, else the JDK's own body (its answer filed when admissible).
pub fn mh_method_finder_mint_intrinsic(
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
pub fn mh_find_virtual_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Virtual, args)
}

/// `Lookup.findStatic` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub fn mh_find_static_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Static, args)
}

/// `Lookup.findSpecial` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub fn mh_find_special_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Special, args)
}

/// `Lookup.findConstructor` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub fn mh_find_constructor_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Constructor, args)
}

/// `Lookup.unreflect` under `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
pub fn mh_unreflect_mint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    mh_method_finder_mint_intrinsic(ctx, MethodFinder::Unreflect, args)
}

/// Register the five method finder `Intrinsic`s when `r`'s VM admits them
/// ([`mh_method_finder_mint_memo_registers`]); a no-op otherwise. States each
/// registration's kind, so the ambient category is neither read nor changed.
pub fn register_method_finder_mint_memo(r: &mut NativeMethodRegistry) {
    if !mh_method_finder_mint_memo_registers(r) {
        return;
    }
    // Wave 109 (`finder_mint_mode_admits`): a `--compatible` `JdkInvoke`
    // family drop keeps these stated `Intrinsic`s (closed after the loop).
    let admitting = r.set_admitting_family_intrinsic(true);
    for finder in MethodFinder::ALL {
        r.register(
            FINDER_MINT_LOOKUP,
            finder.method_name(),
            finder.descriptor(),
            finder.callback(),
            cratonvm_native_api::NativeKind::Intrinsic,
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
pub fn jr21_reflect2_linker_key(site: &str, caller: u32) -> String {
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
    Some(entry)}

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
    Some((member, appendix, null_first_ok))}

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
            inner        }
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
pub fn mh_user_site_lane_serves(class: &str) -> bool {
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
    pub static USER_SITE_LANE_CLASSES: std::cell::Cell<[(usize, u32); 4]> =
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
pub fn mh_user_site_lane_serves_handle(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
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
pub fn mh_compiled_linker_user_site(
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
    pub static BMH_SPECIES_LL_ID: std::cell::Cell<(usize, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// Whether `handle` is a `BoundMethodHandle$Species_LL`: by class id after the
/// first positive answer, else by name. Pure reads. Interpreter round i8
/// (page i147-orch).
pub fn bound_species_ll(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
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
pub fn bound_species_may_be_field_lane(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
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
pub fn linker_census(name: &str, what: &str, start: Option<std::time::Instant>) {
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
pub fn mh_site_type_row_for(
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
pub fn linker_basic_descriptor(desc: &str) -> Option<String> {
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
pub fn mh_real_field_before_linker(
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
    // bench `L4W100FieldSiteMemoBench.java`): a
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
pub fn real_field_raw_return(field: &RealFieldTarget, site: &str) -> bool {
    crate::nbflags().mh_real_field_raw_return
        && matches!(field.ref_kind, 1 | 2)
        && field.fdesc.len() == 1
        && return_type_desc(site) == field.fdesc
}

/// A raw getter answer of [`real_field_raw_return`], kept raw only when its
/// `Value` variant is the descriptor's own (as `mh_direct_lane_return` keeps
/// it), else boxed as the lane did before.
pub fn real_field_raw_answer(
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
pub fn mh_real_direct_before_linker(
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
pub enum RealDirectCustomization {
    /// The form is customized: nothing to count.
    Customized,
    /// Not yet due: the current count, which the call raises by one.
    Count(i32),
    /// Due (or unreadable): the lane declines and the linker customizes.
    Due,
}

/// `CUSTOMIZE_THRESHOLD`'s JDK 25 default (`java.lang.invoke.MethodHandleStatics`).
pub const MH_CUSTOMIZE_THRESHOLD: i32 = 127;

/// [`RealDirectCustomization`] of `mh` (pure reads).
pub fn real_direct_lane_customization(ctx: &dyn NativeContext, mh: ObjectRef) -> RealDirectCustomization {
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

pub const LANE_DUE_MEMO_SLOTS: usize = 16;

thread_local! {
    /// `(vm, address epoch, handle address)` of the handles whose due call a
    /// lane already declined to the linker on this thread ([`lane_due_once`]).
    pub static LANE_DUE_MEMO: std::cell::Cell<[(usize, u64, usize); LANE_DUE_MEMO_SLOTS]> =
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
pub fn lane_due_once(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<Option<i32>> {
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
pub fn lane_note_due_linker_ran(ctx: &dyn NativeContext, mh: ObjectRef) {
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
pub fn lane_uncount_customization(ctx: &mut dyn NativeContext, mh: ObjectRef, previous: Option<i32>) {
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
pub struct LaneCustomizeSlots {
    pub form: usize,
    pub count: usize,
    pub customized: usize,
}

thread_local! {
    /// `(vm_identity, slots)` of [`lane_customize_slots`]: resolved once per VM
    /// (the JDK's own `MethodHandle` / `LambdaForm` layout, fixed for the VM's
    /// life), as [`CUSTOMIZE_THRESHOLD_SETTLED`] keys its fact.
    pub static LANE_CUSTOMIZE_SLOTS: std::cell::Cell<(usize, Option<LaneCustomizeSlots>)> =
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
/// INVOKE; page `docs/feature-designs/native-dispatch-architecture.md`,
/// `bind-each`): the compiled constant bind counts `Invokers.checkCustomized` for
/// an uncustomized real handle from raw slots ([`lane_raw_count_slots`]) instead of
/// building a context and paying the native door's descriptor-aware field read
/// four times (`lane_check_customized`). A shape hit (`bindTo` in a loop: a fresh,
/// never customized handle every call) took that context path on EVERY call.
/// Memoised (asked per counted call).
pub fn mh_bind_raw_count_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
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
pub fn lane_customize_slots(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<LaneCustomizeSlots> {
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
pub fn lane_customization(
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
pub fn lane_write_count(
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
    pub static CUSTOMIZE_THRESHOLD_SETTLED: std::cell::Cell<(usize, i32)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// A threshold no `Integer.parseInt` of the property gives the JDK: a value
/// `MethodHandleStatics.<clinit>` refuses (unparseable, or outside `[-1, 127]`).
pub const MH_CUSTOMIZE_THRESHOLD_UNUSABLE: i32 = i32::MIN;

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
pub fn customize_threshold(ctx: &dyn NativeContext) -> i32 {
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
pub fn customize_threshold_is_default(ctx: &dyn NativeContext) -> bool {
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
pub const INVOKERS_CLASS: &str = "java/lang/invoke/Invokers";
/// `Invokers.checkCustomized(MethodHandle)`.
pub const INVOKERS_CHECK_CUSTOMIZED_DESC: &str = "(Ljava/lang/invoke/MethodHandle;)V";
/// `Invokers.checkExactType(MethodHandle, MethodType)`.
pub const INVOKERS_CHECK_EXACT_TYPE_DESC: &str =
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V";
/// The rows' token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (as `method-type-intern`).
pub const INVOKERS_CHECKS_BODY_ROW: &str = "invoker-checks";

/// Does `r`'s VM get the two invoker-check `Intrinsic`s? The switch, and no JIT
/// (`interpreted_intrinsic_registers_row`, as [`mh_method_type_intern_memo_registers`]).
pub fn mh_invoker_checks_registers(r: &NativeMethodRegistry) -> bool {
    crate::nbflags().mh_invoker_checks_intrinsic
        && r.interpreted_intrinsic_registers_row(INVOKERS_CHECKS_BODY_ROW)
}

/// The JDK's own `Invokers.<name>` body (static), for every call the two
/// `Intrinsic`s do not answer themselves.
pub fn invokers_check_jdk_body(
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
pub fn invokers_check_customized_intrinsic(
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
pub fn invokers_check_exact_type_intrinsic(
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
    pub static INVOKERS_MH_TYPE_SLOT: std::cell::Cell<(usize, Option<usize>)> =
        const { std::cell::Cell::new((0, None)) };
}

/// `MethodHandle.type`'s slot for `mh`, or `None` (a mock VM, an unresolved
/// layout, a slot past `mh`'s fields): the caller then reads by name.
pub fn invokers_mh_type_slot(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<usize> {
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
pub const LF_LEAF_MHI_CLASS: &str = "java/lang/invoke/MethodHandleImpl";
/// `MethodHandleImpl.checkSpreadArgument(Object, int)`.
pub const LF_LEAF_CHECK_SPREAD_DESC: &str = "(Ljava/lang/Object;I)V";
/// `java/lang/invoke/DirectMethodHandle`.
pub const LF_LEAF_DMH_CLASS: &str = "java/lang/invoke/DirectMethodHandle";
/// `DirectMethodHandle.internalMemberName(Object)` (static, JDK 25).
pub const LF_LEAF_INTERNAL_MEMBER_DESC: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
/// The rows' token in `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`.
pub const LF_LEAF_BODY_ROW: &str = "lf-leaf-helpers";

/// Does `r`'s VM get the two leaf-helper `Intrinsic`s? The switch, and no JIT.
pub fn mh_lf_leaf_registers(r: &NativeMethodRegistry) -> bool {
    crate::nbflags().mh_lf_leaf_intrinsics && r.interpreted_intrinsic_registers_row(LF_LEAF_BODY_ROW)
}

/// `MethodHandleImpl.checkSpreadArgument(Object av, int n)` (JDK 25): returns
/// for `av == null && n == 0`, and for an array of length `n` (an `Object[]`
/// by `arraylength`, any other array by `Array.getLength`: the same answer).
/// Everything else throws in the body (`NullPointerException("null array
/// reference")`, `Array.getLength`'s `IllegalArgumentException` for a
/// non-array, `newIllegalArgumentException("array is not of length n")`).
pub fn mhi_check_spread_argument_intrinsic(
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
    pub static LF_LEAF_DMH_MEMBER: std::cell::Cell<(usize, u32, usize, u32)> =
        const { std::cell::Cell::new((0, 0, 0, 0)) };
}

/// `DirectMethodHandle.member`'s slot when `mh` is a `DirectMethodHandle`
/// (or a subclass: the checkcast's question), else `None` (a mock VM, an
/// unresolved layout, a carrier or any other object: the body answers).
pub fn dmh_member_slot(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<usize> {
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
pub fn dmh_internal_member_name_intrinsic(
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
pub fn real_field_call_admits(
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
pub const BMH_SPECIES_LL: &str = "java/lang/invoke/BoundMethodHandle$Species_LL";

/// Does the call site `site` name the type of `declared` with its first
/// parameter dropped (what `bindTo` leaves of an instance accessor's type)?
/// Pure, no allocation.
pub fn bound_site_matches(declared: &str, site: &str) -> bool {
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
pub struct BoundFieldLine {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub accessor: Option<ObjectRef>,
    pub base: Option<ObjectRef>,
}

pub const BOUND_FIELD_MEMO_SLOTS: usize = 64;

thread_local! {
    /// Per-thread record of bound handles proven to be `bindTo` of a real
    /// instance field accessor (interpreter round i5 wave 101, lane L4;
    /// `CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER`, default ON, `=0` off). Keyed as
    /// [`REAL_HANDLE_MEMO`] is: nothing moves or is freed within one collection
    /// epoch, so an equal address in an equal epoch under the same VM is the
    /// same handle, and its `argL0` / `argL1` / `form` / `type` are final.
    pub static BOUND_FIELD_MEMO: std::cell::RefCell<Vec<BoundFieldLine>> =
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
pub const BOUND_FIELD_MEMO_WAYS: usize = 4;

/// The first line of `addr`'s set.
pub fn bound_field_memo_set(addr: usize) -> usize {
    let sets = BOUND_FIELD_MEMO_SLOTS / BOUND_FIELD_MEMO_WAYS;
    let mixed = ((addr as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((mixed >> 32) as usize & (sets - 1)) * BOUND_FIELD_MEMO_WAYS
}

/// The memo line of the handle at `addr` in `vm` and collection `epoch`
/// (moved to the front of its set), or `None`.
pub fn bound_field_memo_find(vm: usize, epoch: u64, addr: usize) -> Option<BoundFieldLine> {
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
pub fn bound_field_memo_file(line: BoundFieldLine) {
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
pub fn mh_real_bound_field_before_linker(
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
pub fn real_bound_field_serve(
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
            2        }
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
            2        }
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
pub fn real_field_warm_answer_needs_no_box(field: &RealFieldTarget, raw: bool) -> bool {
    match field.ref_kind {
        3 | 4 => true,
        _ => field.fdesc.len() != 1 || raw,
    }
}

/// [`real_lane_site_admits`] answered from the site memo under the armed
/// caller only: no frame walk, no site-type row, no refusal classification.
/// `false` when that line is not on file (the native route then runs the full
/// checks and files it).
pub fn real_lane_site_hint_admits(
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
pub fn real_lane_site_admits(
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
pub struct RealFieldSiteKey {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub caller: u32,
}

pub struct RealFieldSiteLine {
    pub key: Option<RealFieldSiteKey>,
    pub site: Box<str>,
}

pub const REAL_FIELD_SITE_MEMO_SLOTS: usize = 64;

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
    pub static REAL_FIELD_SITE_MEMO: std::cell::RefCell<Vec<RealFieldSiteLine>> =
        std::cell::RefCell::new(
            (0..REAL_FIELD_SITE_MEMO_SLOTS)
                .map(|_| RealFieldSiteLine { key: None, site: Box::from("") })
                .collect(),
        );
}

/// The memo key of `this` called from `caller`; `None` for a context that
/// reports VM identity `0` (the mocks), which is never memoised.
pub fn real_field_site_memo_key(
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

pub fn real_field_site_memo_slot(key: RealFieldSiteKey) -> usize {
    let mixed = (key.addr >> 3) ^ (key.caller as usize).wrapping_mul(0x9E37);
    mixed & (REAL_FIELD_SITE_MEMO_SLOTS - 1)
}

/// Did `key` pass its site checks at the call site `site` in this epoch?
pub fn real_field_site_memo_hit(key: RealFieldSiteKey, site: &str) -> bool {
    let slot = real_field_site_memo_slot(key);
    REAL_FIELD_SITE_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        line.key == Some(key) && &*line.site == site
    })
}

/// Remember that `key` passed its site checks at `site`.
pub fn real_field_site_memo_fill(key: RealFieldSiteKey, site: &str) {
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
pub fn mh_linked_invoker(
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
                )            })        }
        _ => None,
    };
    let census = crate::mh_finder_census::phase_start();
    let mut outcome: &'static str = if key.is_some() {
        "miss-shareable"
    } else {
        "miss-user-site"
    };
    let budget = ctx.rebase_compiled_stack_budget(crate::lang_reflect::reflective_stack_bounds());
    let result = Some({
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
    });
    ctx.restore_compiled_stack_budget(budget);
    linker_census(name, outcome, census);
    result
}

/// The `CRATONVM_DBG=mh-finder-census` clock of one `invoke` / `invokeExact`
/// native call on `this` (`None`, one flag read, unless the census is on).
/// Reads only. Interpreter round i4 wave 89, lane L4.
pub fn mh_invoke_census_clock(
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
pub fn with_invoke_with_arguments_frames(
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
/// `L4W109InvokeWithArgumentsBodyBench.java` measures it
/// now). `None`: the served door as before.
pub fn mh_iwa_jdk_body(
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
pub const MH_IWA_ARRAY_DESC: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";

/// The call-site type of the array body's `spreadInvoker(0).invokeExact(adapted, arguments)`.
pub const MH_IWA_SPREAD_SITE: &str = "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;";

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
pub fn mh_iwa_native_steps(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<MethodCallResult> {
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
pub fn iwa_link_memo_enabled(ctx: &dyn NativeContext) -> bool {
    let flags = crate::nbflags();
    flags.mh_iwa_link_memo
        && flags.mh_iwa_spreader_memo
        && !flags.dbg_mh_dispatch
        && ctx.vm_identity() != 0
        && !crate::mh_finder_census::enabled()
}

/// Lines of [`IWA_LINK_MEMO`] (each half).
pub const IWA_LINK_MEMO_SLOTS: usize = 4;

/// One line of [`IWA_LINK_MEMO`]: `key` is `(vm identity, address epoch,
/// arity n | caller class id)`; `a` / `b` are the spreader (and nothing) or
/// the linker's `(MemberName, appendix)`.
#[derive(Clone, Copy)]
pub struct IwaLinkLine {
    pub vm: usize,
    pub epoch: u64,
    pub key: u64,
    pub a: Option<ObjectRef>,
    pub b: Option<ObjectRef>,
}

impl IwaLinkLine {
    pub const EMPTY: IwaLinkLine = IwaLinkLine {
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
    pub static IWA_LINK_MEMO: std::cell::Cell<IwaLinkLines> = const {
        std::cell::Cell::new((
            [IwaLinkLine::EMPTY; IWA_LINK_MEMO_SLOTS],
            [IwaLinkLine::EMPTY; IWA_LINK_MEMO_SLOTS],
        ))
    };
}

/// [`IWA_LINK_MEMO`]'s two halves: spreaders by arity, linker rows by caller.
pub type IwaLinkLines = (
    [IwaLinkLine; IWA_LINK_MEMO_SLOTS],
    [IwaLinkLine; IWA_LINK_MEMO_SLOTS],
);

pub fn iwa_link_line_get(
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

pub fn iwa_link_line_put(
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
pub fn iwa_spreader_line_get(ctx: &dyn NativeContext, n: usize) -> Option<ObjectRef> {
    iwa_link_line_get(ctx, true, n as u64).map(|(s, _)| s) // Widening: usize -> u64
}

pub fn iwa_spreader_line_put(ctx: &dyn NativeContext, n: usize, spreader: ObjectRef) {
    iwa_link_line_put(ctx, true, n as u64, spreader, None); // Widening: usize -> u64
}

/// The class id the door keys its linker row by (`mh_linked_invoker`'s `near`).
pub fn iwa_link_caller(ctx: &dyn NativeContext) -> Option<u32> {
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
pub fn iwa_link_line_fill(ctx: &dyn NativeContext) {
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
    iwa_link_line_put(ctx, false, u64::from(caller), member, Some(appendix));}

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
pub fn iwa_link_memo_call(
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
pub fn mh_iwa_array_intrinsic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub fn iwa_spreader_reset_count(ctx: &dyn NativeContext, spreader: ObjectRef) {
    lane_write_count(ctx, spreader, lane_customize_slots(ctx, spreader), 0);
}

/// [`mh_real_iwa_adapted`]'s memo line for (`this`, `n`) in this VM and address epoch, or
/// `None`. Pure reads (interpreter round i7 wave 138, lane L4).
pub fn iwa_adapted_memo_get(ctx: &dyn NativeContext, this: ObjectRef, n: usize) -> Option<ObjectRef> {
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
pub fn iwa_spreader_key(ctx: &dyn NativeContext, n: usize) -> Option<String> {
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
pub fn mh_iwa_spreader(
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
pub fn mh_invoke_with_arguments_array_door(
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
        return mh_real_invoke_with_arguments_door(ctx, this, array, None);    }
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

