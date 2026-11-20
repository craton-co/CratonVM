#[allow(unused_imports)]
use super::*;

pub fn native_field_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field.set: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let new_value = args.get(2).copied().unwrap_or(Value::Object(None));

    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control: read modifiers from JDK layout, accessible from
    // the CratonVM extra slot.
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let accessible = read_field_accessible(ctx, this);
    // A private field's nest hosts are loaded before the access check judges
    // them, as `Field.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `getNestHost` does (JIT round 20 wave 16, lane nest;
    // `settle_reflective_nest_hosts`): a GC point only when a host is
    // loaded, and every reference this door keeps is refreshed there.
    let (mut this, mut receiver) = (this, receiver);
    let mut new_value_ref = match new_value {
        Value::Object(obj) => obj,
        _ => None,
    };
    if !accessible && modifiers & SA_ACC_PRIVATE != 0 {
        if let Some(caller) = resolve_caller_class_id(ctx) {
            settle_reflective_nest_hosts(
                ctx,
                caller,
                class_id,
                modifiers,
                &mut [&mut this],
                &mut [&mut receiver, &mut new_value_ref],
                &[],
            );
        }
    }
    let new_value = match new_value {
        Value::Object(_) => Value::Object(new_value_ref),
        other => other,
    };
    // Read once, then threaded down every rank: what the refusal NAMES cannot
    // drift between ranks if they all print the same struct.
    let ident = field_ident(ctx, this, class_id, slot, &descriptor, modifiers);
    // RANKS 1-2. The operation label is read only by a refusal, which the
    // override never reaches (`field_access_phase` returns at once), so an
    // accessible field formats none (interpreter round i3 wave 79, lane L3:
    // the static-set door's per-call strings).
    let operation = if accessible {
        String::new()
    } else {
        format!("Field.set({})", descriptor)
    };
    field_access_phase(
        ctx,
        this,
        modifiers,
        accessible,
        is_static,
        class_id,
        receiver,
        &operation,
        Some(FIELD_SET),
    )?;
    // Wave 58, lane L3: building the accessor initializes the class.
    field_door_initialize(ctx, class_id, Some(FIELD_SET), accessible)?;
    // RANK 4. `set(Object,Object)` has no RANK 3 — it accepts every descriptor
    // and decides the value's type at rank 6 instead, which is exactly what
    // separates it from the typed setters:
    //   finalIntField.setLong(obj, 1L)                IllegalArgumentException
    //   finalIntField.set(obj, Integer.valueOf(1))    IllegalAccessException
    //
    // Only the override reaches here with a null receiver (ranks 1-2 raise it
    // first otherwise), and HotSpot raises it deep in the accessor
    // (`MethodHandleFieldAccessorImpl.ensureObj`). Built here rather than
    // returned as an error: the funnel's mark would place the same
    // `obj.getClass()` action at `Field.checkAccess`, which the override never
    // runs (interpreter round i2 wave 53, lane L3;
    // `stackwalker::served_raise_frames`).
    //
    // Interpreter round i2 wave 54, lane L3: with the accessor's chain noted
    // (`ensureObj` < `Impl.set` < `Field.set`), the error is returned for the
    // funnel's mark to carry the chain; without it, built here as before.
    if !is_static && receiver.is_none() {
        if note_field_raise_chain(
            ctx,
            FIELD_SET,
            true,
            &descriptor,
            accessible,
            FieldRaise::NullReceiver,
        ) {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(ENSURE_OBJ_NPE.to_string()),
            }
            .into());
        }
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            ENSURE_OBJ_NPE,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }
    // NOTE the `"to"`: see the quirk in the reflection-message block. Every
    // other rank-4 caller passes `"on"`.
    field_receiver_rank(
        ctx,
        FIELD_SET,
        true,
        &descriptor,
        accessible,
        is_static,
        class_id,
        receiver,
        &ident,
        "set",
        "to",
    )?;
    // RANK 5 вЂ” WP2.1-field final-field write check. It must run AFTER the
    // access checks so the more specific error message wins on a public-final
    // field, and BEFORE the coercion below: measured on Temurin 25.0.3,
    // `finalIntField.set(obj, null)` and
    // `finalStringField.set(obj, Integer.valueOf(1))` are both
    // `IllegalAccessException`, while the same two writes to the same fields
    // without `final` are `IllegalArgumentException`.
    // The generic setter names the VALUE's class (or `null value`) -- not a
    // `(type)value` rendering, which is the typed setters' spelling.
    //
    // Wave 79, lane L3: the value's name and the refusal sentence are built
    // only for a refusal -- a final field (rank 5), or a value the coercion
    // refuses (rank 6, asked again with the sentence) -- not per call.
    if ident.is_final {
        let attempted = attempted_value_name(ctx, new_value);
        // Wave 57, lane L3: each value refusal notes the JDK accessor's frames.
        check_final_for_set(&ident, accessible, &attempted).inspect_err(|_| {
            note_field_raise_chain(
                ctx,
                FIELD_SET,
                true,
                &descriptor,
                accessible,
                FieldRaise::FinalField,
            );
        })?;
    }

    // RANK 6 вЂ” strictly coerce the value if the field expects a primitive
    // (including widening); this raises IllegalArgumentException if the wrapper
    // type cannot be narrowed/widened to the target primitive per JLS В§5.1.2.
    let coerced = match coerce_arg_strict_msg(
        ctx,
        new_value,
        &descriptor,
        "Field.set",
        Some(class_id),
        None,
    )
    .or_else(|_| {
        // The refusal again, with `Field.set`'s own sentence (the coercion
        // reads and decides only; it allocates nothing on the heap).
        let attempted = attempted_value_name(&*ctx, new_value);
        coerce_arg_strict_msg(
            &*ctx,
            new_value,
            &descriptor,
            "Field.set",
            Some(class_id),
            Some(&ident.set_to(&attempted)),
        )
    }) {
        Ok(v) => v,
        Err(e) => {
            // DIAG (`CRATONVM_DBG=coerce`): the refusal itself names the FIELD's
            // declaring class and the VALUE's class. On a loader split those two
            // are the same name under different loaders, and the pair alone
            // cannot say which side picked the wrong copy. The RECEIVER is what
            // decides: a receiver from the value's world means the Field object
            // came from the wrong copy, a receiver from the declaring class's
            // world means the value did.
            if crate::nbflags().dbg_coerce {
                if let Some(recv) = receiver {
                    let recv_cid = ctx.class_id_of_object(recv);
                    eprintln!(
                        "[DBG_COERCE] Field.set: receiver={} (cid={recv_cid:?}, loader={}) \
declaring={} (cid={class_id:?}, loader={})",
                        ctx.class_name_of_id(recv_cid).unwrap_or_default(),
                        ctx.loader_id_of_class(recv_cid),
                        ctx.class_name_of_id(class_id).unwrap_or_default(),
                        ctx.loader_id_of_class(class_id),
                    );
                }
            }
            note_field_raise_chain(
                ctx,
                FIELD_SET,
                true,
                &descriptor,
                accessible,
                FieldRaise::SetValueType {
                    null_value: matches!(new_value, Value::Object(None)),
                },
            );
            return Err(e);
        }
    };

    // WP2.1-field вЂ” volatile-aware write fences (no-op for non-volatile).
    volatile_store_fence_pre(modifiers);
    dbg_field_get(
        ctx,
        "Field.set",
        this,
        is_static,
        class_id,
        slot,
        &descriptor,
        receiver,
    );

    if is_static {
        ensure_static_field_declaring_class_initialized(ctx, class_id)?;
        ctx.set_static_field(class_id, slot, coerced);
    } else {
        let recv =
            receiver.ok_or_else(
                || cratonvm_types::error::RuntimeError::NullPointerException { message: None },
            )?;
        reject_array_field_receiver(ctx, recv, "Field.set")?;
        ctx.set_field(recv, slot, coerced);
    }
    volatile_store_fence_post(modifiers);
    Ok(None)
}

// --- Typed Field getters (getInt, getLong, getFloat, getDouble, getBoolean) ---

/// Round-9 native-builtins HIGH-7 fix: enforce JDK descriptor compatibility
/// matrix for `Field.getInt/getLong/getFloat/getDouble/getByte/getShort/getChar`.
///
/// Per `java.lang.reflect.Field` javadoc the typed getters apply the JLS
/// "widening primitive conversion" matrix. The previous implementation only
/// inspected the underlying `Value` variant, which silently let boolean fields
/// (stored as `Value::Int(0|1)`) slip through `Field.getInt`, diverging from
/// the JDK which throws `IllegalArgumentException` with the canonical
/// "Attempt to get <T> field with illegal data type conversion" message.
///
/// `accepted` is the set of descriptor characters that the caller's typed
/// getter is allowed to widen from. e.g. for `getInt`: `&['B','S','C','I']`.
/// Reference / array descriptors (descriptors starting with `L` or `[`) are
/// always rejected вЂ” typed primitive getters can never read a reference.
pub fn validate_field_descriptor(
    ident: &FieldIdent,
    descriptor: &str,
    accepted: &[u8],
    target: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Empty descriptor means the meta lookup failed; fall back to the value-
    // variant check downstream rather than throwing here.
    if descriptor.is_empty() {
        return Ok(());
    }
    let first = descriptor.as_bytes()[0];
    if accepted.contains(&first) {
        return Ok(());
    }
    // JDK-canonical message: "Attempt to get <prim> field with illegal data
    // type conversion" вЂ” the JDK actually emits a slightly different phrasing
    // depending on the source/target pair, but every variant is an IAE and
    // mentions both the Field method and the underlying type. Match closely.
    // GRAMMAR 4 -- the one that quotes the field name and omits the modifiers.
    Err(illegal_arg_exc(ident.get_conversion(target)))
}

/// The rank-3 gate for the typed SETTERS, the mirror of
/// [`validate_field_descriptor`].
///
/// It is a separate matrix, not the transpose of the getter one: `setByte`
/// accepts a `short` field while `getShort` accepts a `byte` field, and
/// `setChar` refuses an `int` field while `getInt` accepts a `char` one. See the
/// precedence-lattice banner near the top of this file for both, measured.
///
/// Until this existed the typed setters had no descriptor gate at all and leant
/// on `coerce_arg_strict` at rank 6, which cannot see the difference: a
/// `boolean` is a `Value::Int(0|1)` on our stack, so `Field.setBoolean` on an
/// `int` field coerced cleanly and silently wrote 1 where HotSpot throws. The
/// same blindness let `setInt` truncate into a `byte`/`char`/`short` field.
///
/// Nothing this gate refuses can be something a corpus depends on: the corpora
/// run on HotSpot too, and every row it refuses is already an
/// `IllegalArgumentException` there.
pub fn validate_set_descriptor(
    ident: &FieldIdent,
    descriptor: &str,
    accepted: &[u8],
    target: &str,
    value: Value,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Empty descriptor means the meta lookup failed; fall back to the
    // coercion check downstream rather than inventing a refusal here.
    if descriptor.is_empty() {
        return Ok(());
    }
    let first = descriptor.as_bytes()[0];
    if accepted.contains(&first) {
        return Ok(());
    }
    // GRAMMAR 1, rendered at the SETTER's type: this rank refuses before any
    // conversion happens, so `setLong(9L)` into an `int` field prints
    // `(long)9`, not `(int)9`. The legal-widening case at rank 5 prints the
    // field's type instead. Measured both ways.
    Err(illegal_arg_exc(ident.set_to_prim(target, value)))
}

/// A typed accessor's rank-3 descriptor gate, threaded into
/// [`field_get_raw`]/[`field_set_raw`] rather than run at the call site.
///
/// The threading IS the fix. Running it at the call site put it at rank 0, ahead
/// of the access check, so `String.hash.getBoolean("q")` -- denied AND the wrong
/// accessor -- answered `IllegalArgumentException` where HotSpot answers
/// `IllegalAccessException`. Only the raw helpers know where rank 3 falls
/// relative to the override guard, so only they can apply it.
#[derive(Clone, Copy)]
pub struct TypedAccessorGate {
    /// Field descriptor first bytes this accessor may widen from / narrow into.
    pub accepted: &'static [u8],
    /// `int`, `long`, ... -- the accessor's own primitive type, as the message
    /// spells it. This used to be the METHOD name (`getInt`), which is what the
    /// old messages printed and what no JDK message mentions.
    pub prim: &'static str,
}

/// Ranks 1 and 2 of the precedence lattice: `Field.checkAccess`, in full,
/// including the `NullPointerException` that falls out of building its argument
/// list -- and the `if (!override)` that skips the whole thing.
///
/// Skipping is not an optimisation. `check_field_access` and
/// `enforce_module_check_on_field` both return `Ok` on their own when
/// `accessible` is set, but `null_receiver_on_instance_field` does NOT, and
/// HotSpot's rank 1 lives inside `checkAccess`'s call, not before it. Measured:
/// with the override set, `ownPrivateIntField.getBoolean(null)` is an
/// `IllegalArgumentException` (rank 3 wins) while `ownPrivateIntField.get(null)`
/// is still a `NullPointerException` -- raised at rank 4 by `ensureObj`, which
/// the callers below run after the gate.
pub fn field_access_phase(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    modifiers: i32,
    accessible: bool,
    is_static: bool,
    class_id: ClassId,
    receiver: Option<ObjectRef>,
    operation: &str,
    method: Option<FieldAccessorMethod>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if accessible {
        return Ok(());
    }
    // Every accessor: `Field.checkAccess`'s `obj.getClass()` (interpreter
    // round i2 wave 53, lane L3; measured on JDK 25.0.3, probe
    // `L3W53FieldAccessorNull`). Only rank 4's `ensureObj` -- the generic
    // `set` with the override -- says `"o"`.
    null_receiver_on_instance_field(is_static, receiver, Some(CHECK_ACCESS_NPE))?;
    // Bound before the call, not inline: `check_field_access` takes `ctx`
    // mutably and this takes it shared, and the two cannot be live at once.
    let target_cid = reflective_target_class_id(ctx, modifiers, receiver);
    // Interpreter round i3 wave 73, lane L3: every refusal of this phase is
    // HotSpot's `AccessibleObject.checkAccess` -> `Reflection.newIllegalAccessException`,
    // so it notes those frames (`field_access_refusal_chain`) as the JDK's
    // own `Field` bytecode lists them under `--real-jdk`.
    check_field_access(ctx, modifiers, accessible, class_id, target_cid, operation)
        .inspect_err(|e| note_field_access_refusal_chain(ctx, method, e))?;
    // NEW-19: module half of `Reflection.verifyMemberAccess` — the `exports`
    // edge, for public and non-public fields alike. `opens` is the
    // `setAccessible` gate and does not decide a plain read.
    enforce_module_check_on_field(ctx, this, accessible, operation)
        .inspect_err(|e| note_field_access_refusal_chain(ctx, method, e))
}

/// The JDK frames HotSpot lists for a `Field` accessor's ACCESS refusal,
/// OUTERMOST first: `Field.<method>` -> `Field.checkAccess(Class, Object)` ->
/// `AccessibleObject.checkAccess(Class, Class, Class, int)` ->
/// `Reflection.newIllegalAccessException(Class, Class, Class, int)`, which
/// builds the throwable (JDK 25 source; the private-member row of
/// `L3W73JdkOnlyFieldDoors.java` is SAME against HotSpot
/// 25.0.4 under `--real-jdk` with the JDK's `Field` bytecode). The module and
/// class-accessibility refusals come out of the same `checkAccess` call.
/// Interpreter round i3 wave 73, lane L3.
pub fn field_access_refusal_chain(
    method: FieldAccessorMethod,
) -> cratonvm_native_api::ServedRaiseChain {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const FIELD_CHECK: FieldAccessorMethod =
        ("checkAccess", "(Ljava/lang/Class;Ljava/lang/Object;)V");
    const AO_CHECK: FieldAccessorMethod = (
        "checkAccess",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I)V",
    );
    const NEW_IAE: FieldAccessorMethod = (
        "newIllegalAccessException",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I)Ljava/lang/IllegalAccessException;",
    );
    let hop = |class, (name, descriptor): FieldAccessorMethod, site| ServedRaiseHop {
        class,
        name,
        descriptor,
        site,
    };
    let last = |(name, descriptor): FieldAccessorMethod| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    ServedRaiseChain {
        throwable_class: "java/lang/IllegalAccessException",
        hops: vec![
            hop(FIELD_CLASS, method, last(FIELD_CHECK)),
            hop(FIELD_CLASS, FIELD_CHECK, last(AO_CHECK)),
            hop(
                "java/lang/reflect/AccessibleObject",
                AO_CHECK,
                last(NEW_IAE),
            ),
            hop(
                "jdk/internal/reflect/Reflection",
                NEW_IAE,
                ServedRaiseHopSite::Raise,
            ),
        ],
    }
}

/// Note [`field_access_refusal_chain`] for `error` when it is the
/// `IllegalAccessException` the access phase refuses with (nothing else may
/// carry the chain: it would wait for the next one).
#[cold]
#[inline(never)]
pub fn note_field_access_refusal_chain(
    ctx: &mut dyn NativeContext,
    method: Option<FieldAccessorMethod>,
    error: &cratonvm_types::error::MethodCallFailed,
) {
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
    let Some(method) = method else {
        return;
    };
    if matches!(
        error,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalAccessException { .. }
        ))
    ) {
        ctx.note_served_raise_chain(field_access_refusal_chain(method));
    }
}

// ---------------------------------------------------------------------------
// The JDK field accessor's frames of a `Field` raise (interpreter round i2
// wave 54, lane L3)
// ---------------------------------------------------------------------------
//
// HotSpot's `Field` accessors raise a null or wrong receiver inside the JDK's
// `MethodHandle<Type>FieldAccessorImpl` (JDK 25), two to five frames below the
// `Field` method this VM serves -- measured, JDK 25.0.3,
// `L3W54FieldOverrideAccessorFrames.java`:
//
// ```text
// override, null, getter    Objects.requireNonNull < DirectMethodHandle.checkBase
//                           < Impl.<primary getter> [< Impl.<widening getter>] < Field.<getter>
// override, null, generic set  MethodHandleFieldAccessorImpl.ensureObj < Impl.set < Field.set
// wrong receiver, getter    MethodHandleFieldAccessorImpl.newGetIllegalArgumentException
//                           < Impl.<primary getter> [< Impl.<widening getter>] < Field.<getter>
// wrong receiver, set       FieldAccessorImpl.throwSetIllegalArgumentException(String,String)
//                           < ...(Object) < MethodHandleFieldAccessorImpl.ensureObj < Impl.set < Field.set
// ```
//
// The one-hop rule of the funnel's mark cannot reach them, so the natives
// note the chain (`NativeExceptionAccess::note_served_raise_chain`) and the
// capture rebuilds each hop from the real class bytes.

pub const FIELD_CLASS: &str = "java/lang/reflect/Field";
pub const MH_FIELD_ACCESSOR_IMPL: &str = "jdk/internal/reflect/MethodHandleFieldAccessorImpl";
pub const FIELD_ACCESSOR_IMPL: &str = "jdk/internal/reflect/FieldAccessorImpl";

/// A `Field` accessor method `(name, descriptor)` -- also the name and
/// descriptor of the `FieldAccessor` method it calls.
pub type FieldAccessorMethod = (&'static str, &'static str);

pub const FIELD_GET: FieldAccessorMethod = ("get", "(Ljava/lang/Object;)Ljava/lang/Object;");
pub const FIELD_SET: FieldAccessorMethod = ("set", "(Ljava/lang/Object;Ljava/lang/Object;)V");

/// The typed getter of primitive type `prim` (`"int"`, ...).
pub fn field_typed_getter(prim: &str) -> Option<FieldAccessorMethod> {
    Some(match prim {
        "boolean" => ("getBoolean", "(Ljava/lang/Object;)Z"),
        "byte" => ("getByte", "(Ljava/lang/Object;)B"),
        "char" => ("getChar", "(Ljava/lang/Object;)C"),
        "short" => ("getShort", "(Ljava/lang/Object;)S"),
        "int" => ("getInt", "(Ljava/lang/Object;)I"),
        "long" => ("getLong", "(Ljava/lang/Object;)J"),
        "float" => ("getFloat", "(Ljava/lang/Object;)F"),
        "double" => ("getDouble", "(Ljava/lang/Object;)D"),
        _ => return None,
    })
}

/// The typed setter of primitive type `prim`.
pub fn field_typed_setter(prim: &str) -> Option<FieldAccessorMethod> {
    Some(match prim {
        "boolean" => ("setBoolean", "(Ljava/lang/Object;Z)V"),
        "byte" => ("setByte", "(Ljava/lang/Object;B)V"),
        "char" => ("setChar", "(Ljava/lang/Object;C)V"),
        "short" => ("setShort", "(Ljava/lang/Object;S)V"),
        "int" => ("setInt", "(Ljava/lang/Object;I)V"),
        "long" => ("setLong", "(Ljava/lang/Object;J)V"),
        "float" => ("setFloat", "(Ljava/lang/Object;F)V"),
        "double" => ("setDouble", "(Ljava/lang/Object;D)V"),
        _ => return None,
    })
}

/// The JDK 25 accessor class `ReflectionFactory` builds for an instance field
/// of `descriptor`, with its primary getter and setter (the ones that run
/// the method handle; every other accessor of the class widens through them).
pub fn field_accessor_impl(
    descriptor: &str,
) -> Option<(&'static str, FieldAccessorMethod, FieldAccessorMethod)> {
    let (class, prim) = match descriptor.as_bytes().first()? {
        b'Z' => (
            "jdk/internal/reflect/MethodHandleBooleanFieldAccessorImpl",
            "boolean",
        ),
        b'B' => (
            "jdk/internal/reflect/MethodHandleByteFieldAccessorImpl",
            "byte",
        ),
        b'C' => (
            "jdk/internal/reflect/MethodHandleCharacterFieldAccessorImpl",
            "char",
        ),
        b'S' => (
            "jdk/internal/reflect/MethodHandleShortFieldAccessorImpl",
            "short",
        ),
        b'I' => (
            "jdk/internal/reflect/MethodHandleIntegerFieldAccessorImpl",
            "int",
        ),
        b'J' => (
            "jdk/internal/reflect/MethodHandleLongFieldAccessorImpl",
            "long",
        ),
        b'F' => (
            "jdk/internal/reflect/MethodHandleFloatFieldAccessorImpl",
            "float",
        ),
        b'D' => (
            "jdk/internal/reflect/MethodHandleDoubleFieldAccessorImpl",
            "double",
        ),
        b'L' | b'[' => {
            return Some((
                "jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl",
                FIELD_GET,
                FIELD_SET,
            ))
        }
        _ => return None,
    };
    Some((class, field_typed_getter(prim)?, field_typed_setter(prim)?))}

/// Which raise a [`field_raise_chain`] is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldRaise {
    /// A null receiver of an instance field, with the override (without it,
    /// `Field.checkAccess` raises it, one hop down: the mark's own rule).
    NullReceiver,
    /// A receiver that is not an instance of the declaring class.
    WrongReceiver,
    /// A typed setter's null receiver of a READ-ONLY instance field (a
    /// record's or hidden class's final, with the override): `ensureObj`,
    /// before the method handle (interpreter round i2 wave 57, lane L3).
    ReadOnlyNullReceiver,
    /// A typed setter's wrong receiver of a read-only instance field:
    /// `ensureObj` -> `throwSetIllegalArgumentException(Object)` (wave 57).
    ReadOnlyWrongReceiver,
    /// A final field's write: `throwFinalFieldIllegalAccessException`
    /// (interpreter round i2 wave 57, lane L3).
    FinalField,
    /// A value the field's type refuses: the generic `set`'s `null`
    /// (`null_value`, its first `throwSetIllegalArgumentException(Object)`) or
    /// other value (its last), or a typed setter's own type (wave 57).
    SetValueType { null_value: bool },
    /// A typed getter's illegal conversion: `newGet<Type>IllegalArgumentException`
    /// (wave 57).
    GetConversion,
}

/// `(<c>)V`, the descriptor of a `FieldAccessorImpl` raise helper's typed
/// overload (`throwSetIllegalArgumentException(int)`, ...).
pub fn field_prim_helper_descriptor(c: u8) -> Option<&'static str> {
    Some(match c {
        b'Z' => "(Z)V",
        b'B' => "(B)V",
        b'C' => "(C)V",
        b'S' => "(S)V",
        b'I' => "(I)V",
        b'J' => "(J)V",
        b'F' => "(F)V",
        b'D' => "(D)V",
        _ => return None,
    })
}

/// The primitive descriptor character of a typed setter (`setInt` -> `I`).
pub fn field_setter_prim(method: FieldAccessorMethod) -> Option<u8> {
    Some(match method.0 {
        "setBoolean" => b'Z',
        "setByte" => b'B',
        "setChar" => b'C',
        "setShort" => b'S',
        "setInt" => b'I',
        "setLong" => b'J',
        "setFloat" => b'F',
        "setDouble" => b'D',
        _ => return None,
    })
}

/// `FieldAccessorImpl.newGet<Type>IllegalArgumentException()` of a typed getter.
pub fn field_get_conversion_helper(method: FieldAccessorMethod) -> Option<FieldAccessorMethod> {
    const D: &str = "()Ljava/lang/IllegalArgumentException;";
    Some(match method.0 {
        "getBoolean" => ("newGetBooleanIllegalArgumentException", D),
        "getByte" => ("newGetByteIllegalArgumentException", D),
        "getChar" => ("newGetCharIllegalArgumentException", D),
        "getShort" => ("newGetShortIllegalArgumentException", D),
        "getInt" => ("newGetIntIllegalArgumentException", D),
        "getLong" => ("newGetLongIllegalArgumentException", D),
        "getFloat" => ("newGetFloatIllegalArgumentException", D),
        "getDouble" => ("newGetDoubleIllegalArgumentException", D),
        _ => return None,
    })
}

/// The JDK frames HotSpot lists for `raise` in the `Field` method `method`
/// (`is_set`: a setter) of an instance field of `descriptor`, `accessible`
/// the override; OUTERMOST first. `None` for a shape not measured.
pub fn field_raise_chain(
    method: FieldAccessorMethod,
    is_set: bool,
    descriptor: &str,
    accessible: bool,
    raise: FieldRaise,
) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    let call = |(name, descriptor): FieldAccessorMethod, first: bool| {
        if first {
            ServedRaiseHopSite::FirstCall {
                name,
                descriptor: Some(descriptor),
            }
        } else {
            ServedRaiseHopSite::LastCall {
                name,
                descriptor: Some(descriptor),
            }
        }
    };
    let hop = |class, (name, descriptor): FieldAccessorMethod, site| ServedRaiseHop {
        class,
        name,
        descriptor,
        site,
    };
    let (impl_class, primary_get, primary_set) = field_accessor_impl(descriptor)?;
    // `Field.<accessor>`'s call of the accessor: the override branch is the
    // later of its two calls.
    let mut hops = vec![hop(FIELD_CLASS, method, call(method, !accessible))];
    let generic_set = method == FIELD_SET;
    // Wave 57 (measured, JDK 25.0.3, `L3W57FieldRefusalFrames`): the
    // refusals of the value, not the receiver. Each ends in a
    // `FieldAccessorImpl` helper's `(String,String)` overload, or
    // `newGetIllegalArgumentException(String)`, which builds the throwable.
    match raise {
        FieldRaise::FinalField => {
            const STRINGS: FieldAccessorMethod = (
                "throwFinalFieldIllegalAccessException",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            );
            // The generic `set` refuses before it widens: the `Object`
            // overload. A typed setter widens to the primary one first, which
            // refuses at the FIELD's type.
            let (refuser, overload): (FieldAccessorMethod, &'static str) = if generic_set {
                (FIELD_SET, "(Ljava/lang/Object;)V")
            } else {
                if matches!(descriptor.as_bytes().first(), Some(b'L' | b'[')) {
                    return None;
                }
                if method != primary_set {
                    hops.push(hop(impl_class, method, call(primary_set, false)));
                }
                (
                    primary_set,
                    field_prim_helper_descriptor(descriptor.as_bytes()[0])?,
                )
            };
            let one: FieldAccessorMethod = ("throwFinalFieldIllegalAccessException", overload);
            hops.push(hop(impl_class, refuser, call(one, false)));
            hops.push(hop(FIELD_ACCESSOR_IMPL, one, call(STRINGS, false)));
            hops.push(hop(FIELD_ACCESSOR_IMPL, STRINGS, ServedRaiseHopSite::Raise));
            return Some(ServedRaiseChain {
                throwable_class: "java/lang/IllegalAccessException",
                hops,
            });
        }
        FieldRaise::SetValueType { null_value } => {
            const STRINGS: FieldAccessorMethod = (
                "throwSetIllegalArgumentException",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            );
            let overload = if generic_set {
                "(Ljava/lang/Object;)V"
            } else {
                field_prim_helper_descriptor(field_setter_prim(method)?)?
            };
            let one: FieldAccessorMethod = ("throwSetIllegalArgumentException", overload);
            // The generic `set` of a primitive field calls it twice: for a
            // `null`, then for a value of no accepted wrapper.
            hops.push(hop(
                impl_class,
                method,
                call(one, generic_set && null_value),
            ));
            hops.push(hop(FIELD_ACCESSOR_IMPL, one, call(STRINGS, false)));
            hops.push(hop(FIELD_ACCESSOR_IMPL, STRINGS, ServedRaiseHopSite::Raise));
            return Some(ServedRaiseChain {
                throwable_class: "java/lang/IllegalArgumentException",
                hops,
            });
        }
        FieldRaise::GetConversion => {
            const STRING: FieldAccessorMethod = (
                "newGetIllegalArgumentException",
                "(Ljava/lang/String;)Ljava/lang/IllegalArgumentException;",
            );
            let typed = field_get_conversion_helper(method)?;
            hops.push(hop(impl_class, method, call(typed, false)));
            hops.push(hop(FIELD_ACCESSOR_IMPL, typed, call(STRING, false)));
            hops.push(hop(FIELD_ACCESSOR_IMPL, STRING, ServedRaiseHopSite::Raise));
            return Some(ServedRaiseChain {
                throwable_class: "java/lang/IllegalArgumentException",
                hops,
            });
        }
        FieldRaise::NullReceiver
        | FieldRaise::WrongReceiver
        | FieldRaise::ReadOnlyNullReceiver
        | FieldRaise::ReadOnlyWrongReceiver => {}
    }
    let null_receiver = matches!(
        raise,
        FieldRaise::NullReceiver | FieldRaise::ReadOnlyNullReceiver
    );
    let throwable_class = if null_receiver {
        "java/lang/NullPointerException"
    } else {
        "java/lang/IllegalArgumentException"
    };
    let read_only_typed = matches!(
        raise,
        FieldRaise::ReadOnlyNullReceiver | FieldRaise::ReadOnlyWrongReceiver
    );
    if read_only_typed
        && (generic_set || matches!(descriptor.as_bytes().first(), Some(b'L' | b'[')))
    {
        return None;
    }
    if generic_set || read_only_typed {
        // `Impl.set` checks its receiver first: `ensureObj(obj)`; so does a
        // read-only field's primary typed setter, before its final refusal
        // (wave 57), a widening setter first calling it.
        const ENSURE_OBJ: FieldAccessorMethod = ("ensureObj", "(Ljava/lang/Object;)V");
        let checker = if generic_set {
            FIELD_SET
        } else {
            if method != primary_set {
                hops.push(hop(impl_class, method, call(primary_set, false)));
            }
            primary_set
        };
        hops.push(hop(impl_class, checker, call(ENSURE_OBJ, false)));
        match raise {
            // `o.getClass()`: the implicit NPE's action names the site.
            _ if null_receiver => {
                hops.push(hop(
                    MH_FIELD_ACCESSOR_IMPL,
                    ENSURE_OBJ,
                    ServedRaiseHopSite::Raise,
                ));
            }
            _ => {
                const THROW_OBJECT: FieldAccessorMethod =
                    ("throwSetIllegalArgumentException", "(Ljava/lang/Object;)V");
                const THROW_STRINGS: FieldAccessorMethod = (
                    "throwSetIllegalArgumentException",
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                );
                hops.push(hop(
                    MH_FIELD_ACCESSOR_IMPL,
                    ENSURE_OBJ,
                    call(THROW_OBJECT, false),
                ));
                hops.push(hop(
                    FIELD_ACCESSOR_IMPL,
                    THROW_OBJECT,
                    call(THROW_STRINGS, false),
                ));
                hops.push(hop(
                    FIELD_ACCESSOR_IMPL,
                    THROW_STRINGS,
                    ServedRaiseHopSite::Raise,
                ));
            }
        }
        return Some(ServedRaiseChain {
            throwable_class,
            hops,
        });
    }
    let primary = if is_set { primary_set } else { primary_get };
    if method != primary {
        // A widening accessor calls the primary one.
        hops.push(hop(impl_class, method, call(primary, false)));
    }
    match raise {
        FieldRaise::NullReceiver => {
            // The method handle's hidden frames call `checkBase`.
            const CHECK_BASE: FieldAccessorMethod =
                ("checkBase", "(Ljava/lang/Object;)Ljava/lang/Object;");
            const REQUIRE_NON_NULL: FieldAccessorMethod =
                ("requireNonNull", "(Ljava/lang/Object;)Ljava/lang/Object;");
            hops.push(hop(
                impl_class,
                primary,
                ServedRaiseHopSite::LastCall {
                    name: "invokeExact",
                    descriptor: None,
                },
            ));
            hops.push(hop(
                "java/lang/invoke/DirectMethodHandle",
                CHECK_BASE,
                call(REQUIRE_NON_NULL, false),
            ));
            hops.push(hop(
                "java/util/Objects",
                REQUIRE_NON_NULL,
                ServedRaiseHopSite::Raise,
            ));
        }
        _ => {
            // The method handle's `ClassCastException`, caught.
            let new_iae: FieldAccessorMethod = if is_set {
                (
                    "newSetIllegalArgumentException",
                    "(Ljava/lang/Object;)Ljava/lang/IllegalArgumentException;",
                )
            } else {
                (
                    "newGetIllegalArgumentException",
                    "(Ljava/lang/Object;)Ljava/lang/IllegalArgumentException;",
                )
            };
            hops.push(hop(impl_class, primary, call(new_iae, false)));
            hops.push(hop(
                MH_FIELD_ACCESSOR_IMPL,
                new_iae,
                ServedRaiseHopSite::Raise,
            ));
        }
    }
    Some(ServedRaiseChain {
        throwable_class,
        hops,
    })
}

/// `Constructor.newInstance` of an enum's constructor: JDK 25 raises its
/// `IllegalArgumentException` in `acquireConstructorAccessor`, under
/// `newInstanceWithCaller`, under `newInstance` (measured, JDK 25.0.3;
/// `L3W54ReflectiveRaiseChains.java`). Interpreter
/// round i2 wave 54, lane L3.
#[cold]
#[inline(never)]
pub fn note_enum_constructor_chain(ctx: &mut dyn NativeContext) {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const CONSTRUCTOR: &str = "java/lang/reflect/Constructor";
    const WITH_CALLER: &str = "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;";
    const ACQUIRE: &str = "()Ljdk/internal/reflect/ConstructorAccessor;";
    ctx.note_served_raise_chain(ServedRaiseChain {
        throwable_class: "java/lang/IllegalArgumentException",
        hops: vec![
            ServedRaiseHop {
                class: CONSTRUCTOR,
                name: "newInstance",
                descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
                site: ServedRaiseHopSite::LastCall {
                    name: "newInstanceWithCaller",
                    descriptor: Some(WITH_CALLER),
                },
            },
            ServedRaiseHop {
                class: CONSTRUCTOR,
                name: "newInstanceWithCaller",
                descriptor: WITH_CALLER,
                site: ServedRaiseHopSite::LastCall {
                    name: "acquireConstructorAccessor",
                    descriptor: Some(ACQUIRE),
                },
            },
            ServedRaiseHop {
                class: CONSTRUCTOR,
                name: "acquireConstructorAccessor",
                descriptor: ACQUIRE,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    });
}

/// The JDK frames HotSpot lists for the declaring class's initialization
/// failure under the `Field` method `method` (`accessible`: the override's
/// accessor), OUTERMOST first: `Field.<method>` -> `get[Override]FieldAccessor`
/// -> `acquire[Override]FieldAccessor` -> `ReflectionFactory.newFieldAccessor`
/// -> `MethodHandleAccessorFactory.newFieldAccessor` -> its
/// `ensureClassInitialized` -> `Unsafe.ensureClassInitialized` ->
/// `ensureClassInitialized0 (Native Method)` (measured, JDK 25.0.3,
/// `L3W58ReflectiveClassInit.java`). The throwable
/// class is the VM's to pick. Interpreter round i2 wave 58, lane L3.
pub fn field_class_init_chain(
    method: FieldAccessorMethod,
    accessible: bool,
) -> cratonvm_native_api::ServedRaiseChain {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const ACCESSOR: &str = "()Ljdk/internal/reflect/FieldAccessor;";
    let (get, acquire) = if accessible {
        ("getOverrideFieldAccessor", "acquireOverrideFieldAccessor")
    } else {
        ("getFieldAccessor", "acquireFieldAccessor")
    };
    let last = |name: &'static str, descriptor: Option<&'static str>| {
        ServedRaiseHopSite::LastCall { name, descriptor }
    };
    let hop = |class, name, descriptor, site| ServedRaiseHop {
        class,
        name,
        descriptor,
        site,
    };
    let mut hops = vec![
        hop(FIELD_CLASS, method.0, method.1, last(get, Some(ACCESSOR))),
        hop(FIELD_CLASS, get, ACCESSOR, last(acquire, Some(ACCESSOR))),
        hop(
            FIELD_CLASS,
            acquire,
            ACCESSOR,
            last(
                "newFieldAccessor",
                Some("(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessor;"),
            ),
        ),
        hop(
            "jdk/internal/reflect/ReflectionFactory",
            "newFieldAccessor",
            "(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessor;",
            last(
                "newFieldAccessor",
                Some("(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessorImpl;"),
            ),
        ),
        hop(
            "jdk/internal/reflect/MethodHandleAccessorFactory",
            "newFieldAccessor",
            "(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessorImpl;",
            last("ensureClassInitialized", Some("(Ljava/lang/Class;)V")),
        ),
    ];
    hops.extend(class_init_chain_tail(
        "jdk/internal/reflect/MethodHandleAccessorFactory",
    ));
    ServedRaiseChain {
        throwable_class: "java/lang/ExceptionInInitializerError",
        hops,
    }
}

/// The hops of a class initialization below `entry`'s `ensureClassInitialized
/// (Class)` (`MethodHandleAccessorFactory`'s static one, or the `Unsafe`
/// instance method itself), ending in `Unsafe.ensureClassInitialized0`'s raise.
pub fn class_init_chain_tail(
    entry: &'static str,
) -> Vec<cratonvm_native_api::ServedRaiseHop> {
    use cratonvm_native_api::{ServedRaiseHop, ServedRaiseHopSite};
    const UNSAFE: &str = "jdk/internal/misc/Unsafe";
    const CLASS_V: &str = "(Ljava/lang/Class;)V";
    let mut hops = Vec::with_capacity(3);
    if entry != UNSAFE {
        hops.push(ServedRaiseHop {
            class: entry,
            name: "ensureClassInitialized",
            descriptor: CLASS_V,
            site: ServedRaiseHopSite::LastCall {
                name: "ensureClassInitialized",
                descriptor: Some(CLASS_V),
            },
        });
    }
    hops.push(ServedRaiseHop {
        class: UNSAFE,
        name: "ensureClassInitialized",
        descriptor: CLASS_V,
        site: ServedRaiseHopSite::LastCall {
            name: "ensureClassInitialized0",
            descriptor: Some(CLASS_V),
        },
    });
    hops.push(ServedRaiseHop {
        class: UNSAFE,
        name: "ensureClassInitialized0",
        descriptor: CLASS_V,
        site: ServedRaiseHopSite::Raise,
    });
    hops
}

/// `MethodHandles.Lookup.ensureInitialized(Class)`'s initialization of
/// `class_id`, served by a native (`--compatible`): HotSpot raises its failure
/// under `Unsafe.ensureClassInitialized0 (Native Method)` <
/// `Unsafe.ensureClassInitialized` < `Lookup.ensureInitialized` (measured, JDK
/// 25.0.3, `L3W58ReflectiveClassInit`). Interpreter round i2 wave 58, lane L3.
pub fn lookup_ensure_initialized_class(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    if ctx.is_class_initialized(class_id) {
        return Ok(());
    }
    let mut hops = vec![ServedRaiseHop {
        class: "java/lang/invoke/MethodHandles$Lookup",
        name: "ensureInitialized",
        descriptor: "(Ljava/lang/Class;)Ljava/lang/Class;",
        site: ServedRaiseHopSite::LastCall {
            name: "ensureClassInitialized",
            descriptor: Some("(Ljava/lang/Class;)V"),
        },
    }];
    hops.extend(class_init_chain_tail("jdk/internal/misc/Unsafe"));
    ctx.initialize_class_served(
        class_id,
        ServedRaiseChain {
            throwable_class: "java/lang/ExceptionInInitializerError",
            hops,
        },
    )
}

/// A `Field` accessor native's declaring-class initialization, where JDK 25
/// runs it: building the accessor (`getFieldAccessor`), after the access
/// check and before the accessor's own receiver and value checks, for an
/// instance field as for a static one. The failure lists
/// [`field_class_init_chain`]'s frames (`method`, the `Field` method served;
/// `None`: none). An initialized class pays one atomic load. Interpreter round
/// i2 wave 58, lane L3.
#[inline]
pub fn field_door_initialize(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    method: Option<FieldAccessorMethod>,
    accessible: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if ctx.is_class_initialized(class_id) {
        return Ok(());
    }
    field_door_initialize_cold(ctx, class_id, method, accessible)
}

#[cold]
#[inline(never)]
pub fn field_door_initialize_cold(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    method: Option<FieldAccessorMethod>,
    accessible: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    match method {
        Some(method) => {
            ctx.initialize_class_served(class_id, field_class_init_chain(method, accessible))
        }
        None => ctx.initialize_class(class_id),
    }
}

/// Note [`field_raise_chain`]'s chain for the raise the caller is about to
/// return; `true` when the VM took it.
#[cold]
#[inline(never)]
pub fn note_field_raise_chain(
    ctx: &mut dyn NativeContext,
    method: FieldAccessorMethod,
    is_set: bool,
    descriptor: &str,
    accessible: bool,
    raise: FieldRaise,
) -> bool {
    match field_raise_chain(method, is_set, descriptor, accessible, raise) {
        Some(chain) => ctx.note_served_raise_chain(chain),
        None => false,
    }
}

/// RANK 4 of a `Field` accessor (`method`, `is_set`): the null receiver the
/// override skipped past, then the receiver-type test (`verb`, `prep` as
/// [`reflective_receiver_type_check`] takes them). Each raise notes the JDK
/// accessor's chain first (interpreter round i2 wave 54, lane L3).
#[allow(clippy::too_many_arguments)]
pub fn field_receiver_rank(
    ctx: &mut dyn NativeContext,
    method: FieldAccessorMethod,
    is_set: bool,
    descriptor: &str,
    accessible: bool,
    is_static: bool,
    class_id: ClassId,
    receiver: Option<ObjectRef>,
    ident: &FieldIdent,
    verb: &str,
    prep: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if !is_static && receiver.is_none() {
        note_field_raise_chain(
            ctx,
            method,
            is_set,
            descriptor,
            accessible,
            FieldRaise::NullReceiver,
        );
    }
    null_receiver_on_instance_field(is_static, receiver, None)?;
    reflective_receiver_type_check(ctx, is_static, class_id, receiver, ident, verb, prep)
        .inspect_err(|_| {
            note_field_raise_chain(
                ctx,
                method,
                is_set,
                descriptor,
                accessible,
                FieldRaise::WrongReceiver,
            );
        })
}

/// RANK 4 of a typed setter on a READ-ONLY instance field: the JDK's primary
/// setter runs `ensureObj(obj)` before its final refusal, so a null receiver
/// (reachable only with the override, on a record's or hidden class's final)
/// is `ensureObj`'s implicit NPE and a wrong one its
/// `throwSetIllegalArgumentException(Object)` -- `Can not set final <type>
/// field <Q> to <class>`, the generic `set`'s sentence -- not the method
/// handle's `checkBase` / `on <class>` (measured, JDK 25.0.3,
/// `L3W57FieldRefusalFrames` `ro-*` / `rec-acc-*`). Interpreter round i2 wave
/// 57, lane L3.
#[cold]
#[inline(never)]
pub fn read_only_typed_setter_receiver_rank(
    ctx: &mut dyn NativeContext,
    method: FieldAccessorMethod,
    descriptor: &str,
    accessible: bool,
    class_id: ClassId,
    receiver: Option<ObjectRef>,
    ident: &FieldIdent,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if receiver.is_none() {
        let raise = FieldRaise::ReadOnlyNullReceiver;
        if note_field_raise_chain(ctx, method, true, descriptor, accessible, raise) {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(ENSURE_OBJ_NPE.to_string()),
            }
            .into());
        }
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            ENSURE_OBJ_NPE,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }
    reflective_receiver_type_check(ctx, false, class_id, receiver, ident, "set", "to").inspect_err(
        |_| {
            let raise = FieldRaise::ReadOnlyWrongReceiver;
            note_field_raise_chain(ctx, method, true, descriptor, accessible, raise);
        },
    )
}

pub fn field_get_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    gate: TypedAccessorGate,
) -> Result<Value, cratonvm_types::error::MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field typed getter: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let accessible = read_field_accessible(ctx, this);
    // A private field's nest hosts are loaded before the access check judges
    // them, as `Field.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `getNestHost` does (JIT round 20 wave 16, lane nest;
    // `settle_reflective_nest_hosts`): a GC point only when a host is
    // loaded, and every reference this door keeps is refreshed there.
    let (mut this, mut receiver) = (this, receiver);
    if !accessible && modifiers & SA_ACC_PRIVATE != 0 {
        if let Some(caller) = resolve_caller_class_id(ctx) {
            settle_reflective_nest_hosts(
                ctx,
                caller,
                class_id,
                modifiers,
                &mut [&mut this],
                &mut [&mut receiver],
                &[],
            );
        }
    }
    // Read once, then threaded down every rank: what the refusal NAMES cannot
    // drift between ranks if they all print the same struct.
    let ident = field_ident(ctx, this, class_id, slot, &descriptor, modifiers);

    // RANKS 1-2. The whole typed family — getInt/getLong/getFloat/getDouble/
    // getBoolean/getByte/getShort/getChar — funnels through here, so the rule
    // cannot drift between `Field.get` and its primitive twins.
    field_access_phase(
        ctx,
        this,
        modifiers,
        accessible,
        is_static,
        class_id,
        receiver,
        "Field typed getter",
        field_typed_getter(gate.prim),
    )?;
    // Wave 58, lane L3: building the accessor initializes the class, before
    // the accessor's conversion (rank 3) and receiver (rank 4) checks.
    field_door_initialize(ctx, class_id, field_typed_getter(gate.prim), accessible)?;
    // RANK 3. Below the access refusal — `String.hash.getBoolean("q")` is denied
    // AND the wrong accessor, and HotSpot answers `IllegalAccessException` —
    // and above the receiver test, so with the override set a null receiver
    // still loses to it.
    // Wave 57, lane L3: the refusal notes the JDK accessor's frames.
    validate_field_descriptor(&ident, &descriptor, gate.accepted, gate.prim).inspect_err(|_| {
        if let Some(method) = field_typed_getter(gate.prim) {
            note_field_raise_chain(
                ctx,
                method,
                false,
                &descriptor,
                accessible,
                FieldRaise::GetConversion,
            );
        }
    })?;
    // RANK 4, in its two halves: the null receiver the override skipped past
    // above, then the receiver-type test (with the JDK accessor's frames of
    // either raise, wave 54).
    match field_typed_getter(gate.prim) {
        Some(method) => field_receiver_rank(
            ctx,
            method,
            false,
            &descriptor,
            accessible,
            is_static,
            class_id,
            receiver,
            &ident,
            "get",
            "on",
        )?,
        None => {
            null_receiver_on_instance_field(is_static, receiver, None)?;
            reflective_receiver_type_check(
                ctx, is_static, class_id, receiver, &ident, "get", "on",
            )?;
        }
    }

    // WP2.1-field вЂ” volatile-aware read fence (no-op for non-volatile).
    volatile_load_fence(modifiers);

    dbg_field_get(
        ctx,
        "Field typed getter",
        this,
        is_static,
        class_id,
        slot,
        descriptor.as_str(),
        receiver,
    );

    if is_static {
        ensure_static_field_declaring_class_initialized(ctx, class_id)?;
        Ok(ctx.get_static_field(class_id, slot))
    } else {
        let recv =
            receiver.ok_or_else(
                || cratonvm_types::error::RuntimeError::NullPointerException { message: None },
            )?;
        reject_array_field_receiver(ctx, recv, "Field typed getter")?;
        Ok(ctx.get_field(recv, slot))
    }
}

pub fn native_field_get_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Field.getInt accepts byte/short/char/int fields (all represented as
    // Value::Int on our stack) вЂ” but NOT long, float, or double, per
    // `java.lang.reflect.Field.getInt` javadoc (IllegalArgumentException on
    // non-int-compatible types).
    //
    // Round-9 native-builtins HIGH-7 fix: also reject boolean (`Z`) fields,
    // which are stored as `Value::Int(0|1)` and previously slipped through
    // the variant-only check. The descriptor gate runs at RANK 3, inside
    // `field_get_raw` — see `TypedAccessorGate`.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"BSCI",
            prim: "int",
        },
    )?;
    match val {
        Value::Int(_) => Ok(Some(val)),
        _ => Err(illegal_arg_exc(
            "Field.getInt: field type is not int-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Accepts byte/short/char/int/long (widening). Rejects boolean/float/double/refs.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"BSCIJ",
            prim: "long",
        },
    )?;
    match val {
        Value::Long(_) => Ok(Some(val)),
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Err(illegal_arg_exc(
            "Field.getLong: field type is not long-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Accepts byte/short/char/int/long/float (widening). Rejects boolean/double/refs.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"BSCIJF",
            prim: "float",
        },
    )?;
    match val {
        Value::Float(_) => Ok(Some(val)),
        Value::Int(v) => Ok(Some(Value::Float(v as f32))),
        Value::Long(v) => Ok(Some(Value::Float(v as f32))),
        _ => Err(illegal_arg_exc(
            "Field.getFloat: field type is not float-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Accepts every numeric primitive (widening to double). Rejects boolean/refs.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"BSCIJFD",
            prim: "double",
        },
    )?;
    match val {
        Value::Double(_) => Ok(Some(val)),
        Value::Float(v) => Ok(Some(Value::Double(v as f64))),
        Value::Int(v) => Ok(Some(Value::Double(v as f64))),
        Value::Long(v) => Ok(Some(Value::Double(v as f64))),
        _ => Err(illegal_arg_exc(
            "Field.getDouble: field type is not double-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // getBoolean only accepts boolean fields (per javadoc вЂ” no widening).
    // We detect non-boolean fields by reading the descriptor alongside.
    //
    // This gate used to run HERE, ahead of `field_get_raw` and so ahead of the
    // access check, which is what made `String.hash.getBoolean("q")` — private,
    // cross-module, AND read through the wrong accessor — answer
    // `IllegalArgumentException` where HotSpot answers `IllegalAccessException`.
    // It is RANK 3 and belongs inside; see `TypedAccessorGate`.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"Z",
            prim: "boolean",
        },
    )?;
    match val {
        // Round-7 MED-11 fix: validate the underlying int is strictly 0 or 1.
        // Previously any non-zero int was silently coerced to `true`, which
        // masks heap-corruption bugs and diverges from the JDK contract вЂ”
        // `Field.getBoolean` on a non-boolean storage value throws IAE with
        // exactly this message ("Argument is not of type Boolean").
        Value::Int(0) => Ok(Some(Value::Int(0))),
        Value::Int(1) => Ok(Some(Value::Int(1))),
        Value::Int(_) => Err(illegal_arg_exc(
            "Argument is not of type Boolean".to_string(),
        )),
        _ => Err(illegal_arg_exc(
            "Field.getBoolean: field type is not boolean".to_string(),
        )),
    }
}

// --- Typed Field setters (setInt, setLong, setFloat, setDouble, setBoolean) ---

pub fn field_set_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    new_value: Value,
    gate: TypedAccessorGate,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field typed setter: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let accessible = read_field_accessible(ctx, this);
    // A private field's nest hosts are loaded before the access check judges
    // them, as `Field.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `getNestHost` does (JIT round 20 wave 16, lane nest;
    // `settle_reflective_nest_hosts`): a GC point only when a host is
    // loaded, and every reference this door keeps is refreshed there.
    let (mut this, mut receiver) = (this, receiver);
    if !accessible && modifiers & SA_ACC_PRIVATE != 0 {
        if let Some(caller) = resolve_caller_class_id(ctx) {
            settle_reflective_nest_hosts(
                ctx,
                caller,
                class_id,
                modifiers,
                &mut [&mut this],
                &mut [&mut receiver],
                &[],
            );
        }
    }
    // Read once, then threaded down every rank: what the refusal NAMES cannot
    // drift between ranks if they all print the same struct.
    let ident = field_ident(ctx, this, class_id, slot, &descriptor, modifiers);

    // RANKS 1-2. setInt/setLong/setFloat/setDouble/setBoolean/setByte/setShort/
    // setChar all funnel through here, so the rule cannot drift between
    // `Field.set` and its primitive twins.
    field_access_phase(
        ctx,
        this,
        modifiers,
        accessible,
        is_static,
        class_id,
        receiver,
        "Field typed setter",
        field_typed_setter(gate.prim),
    )?;
    // Wave 58, lane L3: building the accessor initializes the class.
    let typed_setter = field_typed_setter(gate.prim);
    field_door_initialize(ctx, class_id, typed_setter, accessible)?;
    // RANK 3, and it is ABOVE the final-write refusal on this path — measured:
    // `Integer.MAX_VALUE.setInt(null, 1)` is `IllegalAccessException` but
    // `Integer.MAX_VALUE.setBoolean(null, true)` is `IllegalArgumentException`,
    // on the same `public static final int`. `coerce_arg_strict` at rank 6
    // cannot stand in for this: a `boolean` is a `Value::Int(0|1)` here, so it
    // coerces cleanly into an `int` field and the row silently succeeds.
    // Wave 57, lane L3: each value refusal notes the JDK accessor's frames.
    validate_set_descriptor(&ident, &descriptor, gate.accepted, gate.prim, new_value).inspect_err(
        |_| {
            if let Some(method) = typed_setter {
                let raise = FieldRaise::SetValueType { null_value: false };
                note_field_raise_chain(ctx, method, true, &descriptor, accessible, raise);
            }
        },
    )?;
    // RANK 4, in its two halves: the null receiver the override skipped past
    // above, then the receiver-type test (with the JDK accessor's frames of
    // either raise, wave 54). A READ-ONLY instance field's setter checks its
    // receiver in `ensureObj` instead, before the method handle (wave 57):
    // the generic `set`'s NPE and its `to <class>` sentence.
    let read_only = !is_static && ident.is_final && (ident.trusted_final || !accessible);
    match typed_setter {
        Some(method) if read_only => read_only_typed_setter_receiver_rank(
            ctx,
            method,
            &descriptor,
            accessible,
            class_id,
            receiver,
            &ident,
        )?,
        Some(method) => field_receiver_rank(
            ctx,
            method,
            true,
            &descriptor,
            accessible,
            is_static,
            class_id,
            receiver,
            &ident,
            "set",
            "on",
        )?,
        None => {
            null_receiver_on_instance_field(is_static, receiver, None)?;
            reflective_receiver_type_check(
                ctx, is_static, class_id, receiver, &ident, "set", "on",
            )?;
        }
    }
    // RANK 5 вЂ” WP2.1-field final-field write check (matches Field.set on the
    // generic `set(Object,Object)` path).
    // Rank 5 is BELOW rank 3, so the widening is already known to be legal --
    // which is why this renders at the FIELD's type and the value it prints is
    // the widened one: `setChar('z')` into an `int` field says `(int)122`.
    //
    // Interpreter round i3 wave 79, lane L3: the refusal texts of ranks 5 and
    // 6 are built only for a refusal. They were formatted on every call (five
    // `String`s per `setInt`: `L3W73FieldDoorBench` `field-set-static` read
    // ~450 ns slower than the JDK body while the getter, which builds none,
    // was faster). `check_final_for_set` answers `Ok` for a non-final field
    // without reading its text, and the coercion is asked again with its
    // sentence only when it refuses (it reads and decides; nothing on the
    // heap).
    if ident.is_final {
        check_final_for_set(
            &ident,
            accessible,
            &format!("({}){}", ident.ty(), render_prim_as(ident.ty(), new_value)),
        )
        .inspect_err(|_| {
            if let Some(method) = typed_setter {
                note_field_raise_chain(
                    ctx,
                    method,
                    true,
                    &descriptor,
                    accessible,
                    FieldRaise::FinalField,
                );
            }
        })?;
    }

    // RANK 6, and unreachable as a refusal now that rank 3 exists: whatever the
    // descriptor gate admitted always fits. Kept because it is what actually
    // performs the widening/narrowing conversion into the field's storage shape.
    let coerced = coerce_arg_strict_msg(
        &*ctx,
        new_value,
        &descriptor,
        "Field typed setter",
        Some(class_id),
        None,
    )
    .or_else(|_| {
        coerce_arg_strict_msg(
            &*ctx,
            new_value,
            &descriptor,
            "Field typed setter",
            Some(class_id),
            Some(&ident.set_to_prim(ident.ty(), new_value)),
        )
    })?;

    // WP2.1-field вЂ” volatile-aware write fences (no-op for non-volatile).
    volatile_store_fence_pre(modifiers);
    if is_static {
        ensure_static_field_declaring_class_initialized(ctx, class_id)?;
        ctx.set_static_field(class_id, slot, coerced);
    } else {
        let recv =
            receiver.ok_or_else(
                || cratonvm_types::error::RuntimeError::NullPointerException { message: None },
            )?;
        ctx.set_field(recv, slot, coerced);
    }
    volatile_store_fence_post(modifiers);
    Ok(())
}

pub fn native_field_set_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Int(0));
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"IJFD",
            prim: "int",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Long(0));
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"JFD",
            prim: "long",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Float(0.0));
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"FD",
            prim: "float",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Double(0.0));
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"D",
            prim: "double",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Int(0));
    // `Z` only, and this is the row that most needed a descriptor gate: a
    // boolean is a `Value::Int(0|1)` on our stack, so before this existed
    // `setBoolean` on an `int` field coerced cleanly and wrote 1.
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"Z",
            prim: "boolean",
        },
    )?;
    Ok(None)
}

// --- Remaining typed Field getters: byte / short / char ---

pub fn native_field_get_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Field.getByte: only `B` is JLS-legal (no widening from C/S/I вЂ” those throw IAE).
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"B",
            prim: "byte",
        },
    )?;
    match val {
        Value::Int(v) => Ok(Some(Value::Int((v as i8) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getByte: field type is not byte-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Field.getShort: accepts byte (widening) or short. Rejects char (JLS forbids
    // charв†’short narrowing without explicit cast), int, long, boolean, refs.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"BS",
            prim: "short",
        },
    )?;
    match val {
        Value::Int(v) => Ok(Some(Value::Int((v as i16) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getShort: field type is not short-compatible".to_string(),
        )),
    }
}

pub fn native_field_get_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Field.getChar: only `C` is legal вЂ” char is unsigned 16-bit and JLS does
    // not permit widening into it from byte/short/int.
    let val = field_get_raw(
        ctx,
        args,
        TypedAccessorGate {
            accepted: b"C",
            prim: "char",
        },
    )?;
    match val {
        // char is an unsigned 16-bit type stored in an int slot; clamp to u16.
        Value::Int(v) => Ok(Some(Value::Int((v as u16) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getChar: field type is not char-compatible".to_string(),
        )),
    }
}

pub fn native_field_set_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as i8) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setByte: value is not byte-compatible".to_string(),
            ))
        }
    };
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"BSIJFD",
            prim: "byte",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as i16) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setShort: value is not short-compatible".to_string(),
            ))
        }
    };
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"SIJFD",
            prim: "short",
        },
    )?;
    Ok(None)
}

pub fn native_field_set_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as u16) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setChar: value is not char-compatible".to_string(),
            ))
        }
    };
    field_set_raw(
        ctx,
        args,
        val,
        TypedAccessorGate {
            accepted: b"CIJFD",
            prim: "char",
        },
    )?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Class.getDeclaredFields / getDeclaredField
// ---------------------------------------------------------------------------

pub fn synthetic_declared_field_alias(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Option<FieldMetadata> {
    match ctx.class_name_arc_of_id(class_id).as_deref() {
        Some("java/util/Collections$UnmodifiableMap" | "cratonvm/internal/UnmodifiableMap") => {
            Some(FieldMetadata {
                name: "m".to_string(),
                descriptor: "Ljava/util/Map;".to_string(),
                access_flags: 0x0012, // ACC_PRIVATE | ACC_FINAL
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: false,
            })
        }
        _ => None,
    }
}

/// Map a `LambdaSerialMetadata::capture_types` char (JVMS field-descriptor
/// first byte, or `'L'` standing in for both reference and array captures —
/// `parse_descriptor_args` erases the specific class/array-component name)
/// to a field descriptor string. Reference captures get a generic
/// `Ljava/lang/Object;` descriptor since the real one isn't retained; this
/// only needs to be reflection-readable, not exact — real getters
/// (`Field.get`) don't re-validate against the declared type on read.
pub fn capture_field_descriptor(type_char: char) -> String {
    match type_char {
        'B' | 'C' | 'I' | 'S' | 'Z' | 'J' | 'F' | 'D' => type_char.to_string(),
        _ => "Ljava/lang/Object;".to_string(),
    }
}

/// The declared type of each capture of a lambda proxy, as HotSpot's spun
/// class types its `arg$N` fields: the factory type's parameters (interpreter
/// round i1 wave 32; item 3 of
/// `i29-L4-native-indy-linkage-skips-the-jdks-validation`).
///
/// Those are the implementation method's leading parameters, after its
/// receiver for an instance method (`REF_invokeVirtual`, `REF_invokeSpecial`,
/// `REF_invokeInterface`: the bound receiver is the first capture, typed by the
/// method's class). A capture whose derived descriptor does not agree with its
/// recorded type character (a primitive where a reference was captured, or
/// the reverse) is `None`, and the caller keeps its `Object` spelling.
pub fn capture_declared_descriptors(
    meta: &cratonvm_native_api::LambdaSerialMetadata,
) -> Vec<Option<String>> {
    let receiver_first = matches!(meta.impl_ref_kind, 5 | 7 | 9);
    let mut declared: Vec<String> = Vec::new();
    if receiver_first {
        declared.push(format!("L{};", meta.impl_class));
    }
    declared.extend(
        parse_method_param_descriptors(&meta.impl_descriptor)
            .into_iter()
            .map(str::to_string),
    );
    meta.capture_types
        .chars()
        .enumerate()
        .map(|(i, ty)| {
            let d = declared.get(i)?;
            let is_ref = d.starts_with('L') || d.starts_with('[');
            let agrees = if matches!(ty, 'L' | '[') {
                is_ref
            } else {
                d.len() == 1 && d.starts_with(ty)
            };
            agrees.then(|| d.clone())
        })
        .collect()
}

/// The parameter descriptors of a method descriptor, one token each
/// (`(I[JLjava/lang/String;)V` is `I`, `[J`, `Ljava/lang/String;`); empty for
/// a malformed one.
pub fn parse_method_param_descriptors(desc: &str) -> Vec<&str> {
    let Some(params) = desc.strip_prefix('(').and_then(|d| d.split(')').next()) else {
        return Vec::new();
    };
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'L') => match params[i..].find(';') {
                Some(end) => i += end + 1,
                None => return Vec::new(),
            },
            Some(_) => i += 1,
            None => return Vec::new(),
        }
        out.push(&params[start..i]);
    }
    out
}

/// Synthesize the captured-variable fields (`arg$1`, `arg$2`, ...) that
/// real HotSpot's `LambdaMetafactory`-spun proxy classes carry, for a
/// CratonVM lambda-proxy `class_id`. Lambda proxies are never registered in
/// `class_manager` (they live in `shared.classes.lambda_proxies` instead — see
/// `comparable-classcast-lambda-proxy-unknown-class-RESOLVED.md`
/// for the same root cause in a sibling `Comparable` check), so
/// `NativeContext::declared_fields` can't see them and reflective access to
/// a lambda's captured field (e.g. AssertJ's `extracting("arg$1")` fallback,
/// or a test's own `.extracting("jwtProcessor.arg$1...")`) fails with
/// `NoSuchFieldException` even though the value is right there on the proxy
/// object at field slot `i` (`allocate_lambda_proxy` in
/// `vm/src/runtime/invokedynamic.rs` sets captures at fields `0..n` in
/// factory-argument order, matching `capture_types`' order).
///
/// Field *naming* is 1-based (`arg$1` is the first capture, slot 0) —
/// verified empirically against real HotSpot's `InnerClassLambdaMetafactory`
/// output: there is no `arg$0`. The heap *slot index* stays 0-based, since
/// `allocate_lambda_proxy` writes captures starting at field 0.
pub fn lambda_proxy_captured_fields(ctx: &dyn NativeContext, class_id: ClassId) -> Vec<FieldMetadata> {
    let Some(meta) = ctx.lambda_proxy_serial_metadata(class_id) else {
        return Vec::new();
    };
    // A reference type is named only when it is loaded: the field's `Class`
    // is resolved from this descriptor, and a proxy has no loader of its own
    // to load one through.
    let declared: Vec<Option<String>> = capture_declared_descriptors(&meta)
        .into_iter()
        .map(|d| {
            d.filter(|d| {
                let base = d.trim_start_matches('[');
                match base.strip_prefix('L').and_then(|b| b.strip_suffix(';')) {
                    Some(name) => ctx.class_id_by_name(name).is_some(),
                    None => true,
                }
            })
        })
        .collect();
    meta.capture_types
        .chars()
        .enumerate()
        .map(|(i, ty)| FieldMetadata {
            name: format!("arg${}", i + 1),
            descriptor: declared
                .get(i)
                .cloned()
                .flatten()
                .unwrap_or_else(|| capture_field_descriptor(ty)),
            // ACC_PRIVATE | ACC_FINAL, deliberately WITHOUT ACC_SYNTHETIC:
            // real HotSpot's `InnerClassLambdaMetafactory` adds these fields
            // as plain private-final, not synthetic (verified empirically —
            // AssertJ's `FieldUtils.readField` hard-rejects synthetic fields
            // with `IllegalArgumentException: Reading synthetic field is not
            // supported`, yet real JDK reflection over `arg$N` is exactly
            // the pattern these tests rely on).
            access_flags: 0x0012,
            slot_index: i,
            declaring_class_id: class_id,
            is_static: false,
        })
        .collect()
}

/// The heap slot of the captured-variable field `name` (`arg$N`) of
/// `receiver` when it is a lambda proxy, else `None`.
pub fn lambda_capture_slot(
    ctx: &dyn NativeContext,
    receiver: cratonvm_types::ObjectRef,
    name: &str,
) -> Option<usize> {
    if !name.starts_with("arg$") {
        return None;
    }
    let class_id = ctx.class_id_of_object(receiver);
    lambda_proxy_captured_fields(ctx, class_id)
        .into_iter()
        .find(|f| f.name == name)
        .map(|f| f.slot_index)
}

pub fn declared_fields_with_aliases(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Vec<FieldMetadata> {
    let mut fields = ctx.declared_fields(class_id);
    if fields.is_empty() {
        fields = lambda_proxy_captured_fields(ctx, class_id);
    }
    if let Some(alias) = synthetic_declared_field_alias(ctx, class_id) {
        if fields.iter().all(|field| field.name != alias.name) {
            fields.push(alias);
        }
    }
    fields
}

pub fn native_class_get_declared_fields(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredFields on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // Primitive or array type вЂ” no declared fields
            let arr = empty_listing_array(ctx, "java/lang/reflect/Field");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // HotSpot `JVM_GetClassDeclaredFields` links the class first (the VM's
    // policy decides; interpreter round i1 wave 17). `this` is not read again.
    ctx.link_class_for_reflection(class_id)?;
    // 2026-10-07 (item 3 of the i47-L5 page): called with no argument, this
    // serves the JDK's `getDeclaredFields()` (`--compatible`), whose
    // `privateGetDeclaredFields` frames the missing-type raise is marked by
    // (`served_listing_prepass`); `getDeclaredFields0(publicOnly)` takes two.
    if args.len() == 1 {
        served_listing_prepass(
            ctx,
            class_id,
            ListingDoor::DeclaredFields,
            ListingLookup::All,
        )?;
    }

    // `getDeclaredFields0(boolean publicOnly)` (JVMS-internal). The real JDK
    // `Class.getFields()` в†’ `privateGetPublicFields()` calls this with
    // publicOnly=true and TRUSTS it to return only public fields (it does not
    // re-filter by access). Honour the flag, else private fields leak into
    // `getFields()` вЂ” e.g. ES `Version.<clinit>` iterates `getFields()` and
    // does `field.get(null)` on every Version-typed field, which NPEs in
    // `Field.checkAccess` (`obj.getClass()`) on the private instance field
    // `minCompatVersion`. `getDeclaredFields()` passes publicOnly=false.
    let public_only = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    // `Reflection::new_field` resolves each field's type through the class's
    // loader (interpreter round i1 wave 47, lane L5; see
    // `note_reflected_members`).
    if ctx.jdk_names_initiated_armed() {
        ctx.note_reflected_members(class_id, 2, public_only);
    }
    let fields = declared_fields_with_aliases(ctx, class_id);
    // `Reflection::new_field` loads each field's type (interpreter round i2
    // wave 54, lane L3; `resolve_reflected_signature_types`): every field the
    // native lists, before the JDK's own filter runs in Java.
    if ctx.reflection_loads_signature_types() {
        resolve_reflected_signature_types(
            ctx,
            class_id,
            fields
                .iter()
                .filter(|m| !public_only || (m.access_flags & (ACC_PUBLIC as u16)) != 0)
                .map(|m| (m.descriptor.as_str(), &[][..])),
        )?;
    }
    let filtered_names = crate::reflect_filter::filtered_field_names(ctx, class_id);
    let selected: Vec<&FieldMetadata> = fields
        .iter()
        .filter(|m| !public_only || (m.access_flags & (ACC_PUBLIC as u16)) != 0)
        .filter(|m| !crate::reflect_filter::is_filtered(&filtered_names, &m.name))
        .collect();
    // GC-safe: `create_field_object` allocates, so the array must be pinned
    // across the fill loop (see `build_mirror_array` вЂ” WildFly bug-06).
    // zs s3/nd3: a loading lookup (`listing_component_id`); nothing raw is
    // live here (`this` is not read after the link).
    let field_component = listing_component_id(ctx, "java/lang/reflect/Field");
    let arr = build_mirror_array_comp(ctx, field_component, selected.len(), |ctx, i| {
        Ok(create_field_object(ctx, selected[i]))
    })?;
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_class_get_declared_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredField on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // JDK 25's `Objects.requireNonNull(name)`: no message (interpreter
            // round i2 wave 53, lane L3; its frames are the funnel's mark's,
            // `stackwalker::served_raise_frames`).
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();
    if crate::vmflags().loader.dbg_fbcglib && target_name.starts_with("CGLIB$") {
        eprintln!("[FBCGLIB-DBG] Class.getDeclaredField({target_name})");
    }

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchFieldException {
                field_name: target_name,
            }
            .into())
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(
        ctx,
        class_id,
        ListingDoor::DeclaredField,
        ListingLookup::All,
    )?;

    // Core reflection member filter (see reflect_filter.rs): a filtered
    // field is not merely inaccessible, it does not exist as far as
    // `getDeclaredField` is concerned — checked before the cache probe and
    // the cold walk so the single-member form agrees with the array form
    // in `native_class_get_declared_fields` on every row.
    if crate::reflect_filter::is_filtered(
        &crate::reflect_filter::filtered_field_names(ctx, class_id),
        &target_name,
    ) {
        return Err(cratonvm_types::error::RuntimeError::NoSuchFieldException {
            field_name: target_name,
        }
        .into());
    }

    // Round 9 audit fix (HIGH #7): probe the per-VM `LinkResolver` cache
    // for `(class_id, name, "")`. `getDeclaredField` takes no descriptor
    // argument so the cache key matches the `JNI GetFieldID` `""` shape
    // when the JNI caller omits the signature.
    if let Some((decl, abs_idx, is_static)) =
        ctx.link_resolver_get_field(class_id, &target_name, "")
    {
        // `decl == class_id` — the guard the method twin
        // (`native_class_get_declared_method`) already carries at its own
        // LinkResolver probe, and the field side did not.
        //
        // The cache is ONE flat map keyed by `(ClassId, name, descriptor)`, and
        // `native_class_get_field` writes into it under the class the caller
        // QUERIED with the class that actually DECLARES the field as the value.
        // So `Derived.class.getField("x")` on an `x` inherited from `Base`
        // leaves `(Derived,"x","") -> Base`, and this probe then answered
        // `getDeclaredField("x")` from `declared_fields_with_aliases(Base)` —
        // returning `Base.x` where the JDK throws `NoSuchFieldException`.
        // The cold walk below is correct (it only ever looks at `class_id`'s
        // own fields), so falling through on a foreign `decl` is both safe and
        // sufficient.
        //
        // Every framework that separates declared from inherited members by
        // catching `NoSuchFieldException` while climbing `getSuperclass()`
        // (Jackson's `AnnotatedFieldCollector`, Spring's
        // `ReflectionUtils.findField`, Hibernate's `ReflectHelper`) attributed
        // the field to the subclass and then processed it AGAIN at the
        // superclass rung. And because the entry is subject to CLOCK eviction,
        // it did so intermittently.
        if decl == class_id {
            // Cache hit вЂ” re-fetch the metadata to build the Field mirror.
            // We still walk `declared_fields(decl)` (small per-class vec) so
            // the mirror's `create_field_object` payload (descriptor, mods,
            // signature) matches what a cold miss would have produced.
            let fields = declared_fields_with_aliases(ctx, decl);
            for meta in &fields {
                if meta.name == target_name
                    && meta.slot_index == abs_idx as usize
                    && meta.is_static == is_static
                {
                    let field_obj = create_field_object(ctx, meta);
                    return Ok(Some(Value::Object(Some(field_obj))));
                }
            }
        }
        // Fall through to the cold-miss walk if the cached entry no
        // longer matches (declared_fields shape mutated under us; very
        // rare) or was written by `getField` for an inherited field.
    }

    let fields = declared_fields_with_aliases(ctx, class_id);
    for meta in &fields {
        if meta.name == target_name {
            // Round 9 audit fix (HIGH #7): cache the cold-miss result so
            // subsequent probes short-circuit.
            ctx.link_resolver_insert_field(
                class_id,
                &target_name,
                "",
                meta.declaring_class_id,
                meta.slot_index as u32,
                meta.is_static,
            );
            let field_obj = create_field_object(ctx, meta);
            return Ok(Some(Value::Object(Some(field_obj))));
        }
    }

    Err(cratonvm_types::error::RuntimeError::NoSuchFieldException {
        field_name: target_name,
    }
    .into())
}

pub const METHOD_EXTRA_SLOTS: usize = 4;
pub const METHOD_EXTRA_OFFSET_DESC: usize = 0;
pub const METHOD_EXTRA_OFFSET_PARAM_COUNT: usize = 1;
pub const METHOD_EXTRA_OFFSET_ACCESSIBLE: usize = 2;
pub const METHOD_EXTRA_OFFSET_TRUSTED: usize = 3;
// Marker for Method mirrors constructed by `create_method_object`. Real JDK
// Method objects do not reserve this CratonVM tail slot, so their descriptor
// still takes the defensive field-reconstruction path below.
pub const METHOD_EXTRA_TRUSTED_MARKER: i32 = 0x4d45_5448; // "METH"

// Legacy synthetic Method mirror slots used when `java/lang/reflect/Method`
// has no real JDK field metadata (synthetic-JDK mode). These mirror the
// MockNativeContext mapping in `test_utils.rs`.
//
// W7-77-guarded-slot-maps.md re-derived the real layout and confirms 11 of the
// 12 constants below name a different field than JDK 25.0.3.9 has at that
// index. `javap -p` over the chain `AccessibleObject` -> `Executable` ->
// `Method`, static excluded, superclass-first:
//
//      0 override             1 accessCheckCache      2 parameterData
//      3 declaredAnnotations  4 clazz                 5 slot
//      6 name                 7 returnType            8 parameterTypes
//      9 exceptionTypes      10 modifiers            11 signature
//     12 annotations         13 parameterAnnotations 14 annotationDefault
//     15 root                16 genericInfo          17 methodAccessor
//     18 hash                19 callerSensitive
//
// Only `EXCEPTION_TYPES` (9) agrees. `CLAZZ`(0) is `override`, a boolean;
// `NAME`(1) is `accessCheckCache`; `SLOT`(4) is `clazz`; `OVERRIDE`(6) is
// `name`. So on a real `Method` this map would put the return type where the
// declaring class belongs -- the exact Byte Buddy failure the `has_named_layout`
// gate exists for, whose story is told in `create_method_object` below.
//
// THE GUARD IS CORRECT AND IS THE STANDING REMEDY.
// `method_class_has_named_layout` asks
// `resolve_field_index_by_class_id(class_id, "clazz")` -- a CLASS-side witness,
// and the right question ("does this class HAVE a named field table") rather
// than the wrong one ("did the named writes land"), which its own comment
// records as having been a corruption bug. Every write is inside
// `if !has_named_layout`; every read goes through
// `method_object_field_value_or_legacy` / `method_int_field_value_or_legacy`,
// which consult the same witness before falling back.
//
// NOTHING HERE IS RENUMBERED, deliberately. A renumber fixes nothing reachable
// -- the guard means these indices are only ever applied to the fabricated
// mirror, where they ARE the layout -- and it would break `test_utils.rs`'s
// `MockNativeContext`, which maps field names onto exactly these slots and is
// the oracle for the synthetic-mode tests. That is the failure mode W7-77 was
// told to avoid: a renumber that fixes one reader and breaks another that
// agreed with the old map. The row leaves the census when the legacy mirror
// does, not before.
pub const METHOD_LEGACY_SLOT_CLAZZ: usize = 0;
pub const METHOD_LEGACY_SLOT_NAME: usize = 1;
pub const METHOD_LEGACY_SLOT_RETURN_TYPE: usize = 2;
pub const METHOD_LEGACY_SLOT_MODIFIERS: usize = 3;
pub const METHOD_LEGACY_SLOT_SLOT: usize = 4;
pub const METHOD_LEGACY_SLOT_OVERRIDE: usize = 6;
pub const METHOD_LEGACY_SLOT_PARAMETER_TYPES: usize = 7;
pub const METHOD_LEGACY_SLOT_CALLER_SENSITIVE: usize = 8;
pub const METHOD_LEGACY_SLOT_EXCEPTION_TYPES: usize = 9;
pub const METHOD_LEGACY_SLOT_ANNOTATIONS: usize = 10;
pub const METHOD_LEGACY_SLOT_PARAMETER_ANNOTATIONS: usize = 11;
pub const METHOD_LEGACY_SLOT_ANNOTATION_DEFAULT: usize = 12;

/// What the legacy Method mirror believes, published for
/// `read_alias::verify_declared_slot_maps` (W7-77).
///
/// States the BELIEF, not JDK 25's layout. Declared from
/// `lang_reflect::register_wp2_1_natives`, which
/// `register_annotation_overrides` -> `register_essential_natives_with_shims`
/// reaches in BOTH modes, so the sweep has this map whichever arm booted.
pub static METHOD_LEGACY_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/lang/reflect/Method",
        slots: &[
            (METHOD_LEGACY_SLOT_CLAZZ, "clazz"),
            (METHOD_LEGACY_SLOT_NAME, "name"),
            (METHOD_LEGACY_SLOT_RETURN_TYPE, "returnType"),
            (METHOD_LEGACY_SLOT_MODIFIERS, "modifiers"),
            (METHOD_LEGACY_SLOT_SLOT, "slot"),
            (METHOD_LEGACY_SLOT_OVERRIDE, "override"),
            (METHOD_LEGACY_SLOT_PARAMETER_TYPES, "parameterTypes"),
            (METHOD_LEGACY_SLOT_CALLER_SENSITIVE, "callerSensitive"),
            (METHOD_LEGACY_SLOT_EXCEPTION_TYPES, "exceptionTypes"),
            (METHOD_LEGACY_SLOT_ANNOTATIONS, "annotations"),
            (
                METHOD_LEGACY_SLOT_PARAMETER_ANNOTATIONS,
                "parameterAnnotations",
            ),
            (METHOD_LEGACY_SLOT_ANNOTATION_DEFAULT, "annotationDefault"),
        ],
        origin: "native-builtins/src/lang_class.rs METHOD_LEGACY_SLOT_*",
    };

// Legacy synthetic Method width вЂ” kept as a floor so the allocated
// object is always large enough to host the synthetic writes made by
// tests (via MockNativeContext which maps field names to these slots)
// and to avoid underallocation when `class_num_total_fields` returns 0
// (class not loaded yet).
// G2: bumped from 8 в†’ 12 to accommodate the 4 additional non-null array
// fields populated by `create_method_object` (`exceptionTypes`,
// `annotations`, `parameterAnnotations`, `annotationDefault`). The mock
// `MockNativeContext` maps these names to slots 9..=12 (see
// `mock_jdk_field_slot`); the floor must be в‰Ґ those slots so the heap
// allocation reserves room.
pub const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;

/// Helper: absolute slot for the first extra-metadata slot on a Method obj.
pub fn method_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(
        METHOD_NUM_FIELDS_LEGACY_FLOOR,
        ctx.class_num_total_fields(class_id),
    )
}

/// C6 fix: Previously the Method object was allocated with only 8 slots
/// and the synthetic layout was written at slots 0..=7 by index. But real
/// JDK `java.lang.reflect.Method` inherits from
/// `java.lang.reflect.Executable` в†’ `AccessibleObject`, so its declared
/// instance-field layout puts `clazz` at a higher absolute slot (not 0).
/// When real-JDK bytecode did `Getfield clazz` / `Getfield modifiers`
/// (e.g. `Method.isCallerSensitive` в†’ `getDeclaringClass`), the
/// interpreter resolved those real JDK offsets and read our synthetic
/// values вЂ” so a `GETFIELD clazz` landed on a slot holding an
/// `Int(1)` residue, producing the
/// `expected object reference, got int(1)` crash observed in stream +
/// enum reflection paths (EnumMap.<init> в†’ getEnumConstantsShared в†’
/// Method.invoke в†’ isCallerSensitive в†’ getDeclaringClass).
///
/// Fix: populate the real JDK-named fields via `set_field_by_name` and
/// store CratonVM-specific metadata (raw descriptor, cached parameter
/// count, accessible flag) in extra slots appended *after* the JDK
/// layout. All native readers go via `get_field_by_name` for JDK fields
/// and via the extra-slot helpers for our metadata.
pub fn create_method_object(
    ctx: &mut dyn NativeContext,
    meta: &MethodMetadata,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let mut __lap = gdmp::Lap::new();
    let class_id = ctx
        .ensure_class_initialized("java/lang/reflect/Method")
        .unwrap_or(ClassId::new(0));

    // Allocate JDK-layout width + our extra metadata slots.
    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + METHOD_EXTRA_SLOTS;
    let obj = ctx.alloc_object(class_id, num_fields);
    // GC-safety: every reflective mirror/array/string built below can trigger
    // classloading (and therefore GC) via `get_class_mirror` /
    // `descriptor_to_class_mirror[_via_loader]` / `build_mirror_array_comp` /
    // `create_string`. A moving collection between `alloc_object` and the
    // `set_field_by_name` block leaves these raw `ObjectRef`s stale — per
    // `pin_native_root`'s contract, a stale ref then resolves to whatever now
    // occupies the reused slot (typically a bare `java.lang.Object`), so the
    // Method silently gets a wrong/missing `clazz`/`name`/etc. Under light
    // allocation pressure this is rare enough to go unnoticed; under the heavy
    // classloading churn of WildFly's ServiceLoader-based extension bootstrap
    // (500+ module jars) it reproduces deterministically as
    // "Constructor.newInstance: no declaring class" (the analogous
    // `create_constructor_object` bug it shares this pattern with — see
    // wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md).
    // Pin everything now and re-read the forwarded reference right before use.
    let obj_pin = ctx.pin_native_root(obj);

    __lap.mark(0);
    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);
    let class_mirror_pin = ctx.pin_native_root(class_mirror);
    let name_str = ctx.intern_string(&meta.name);
    let name_str_pin = ctx.pin_native_root(name_str);

    // Parse descriptor for param types and return type. Resolve reference types
    // through the declaring class's own loader (loader-faithful) so a method on a
    // bytecode-enhanced / child-loader class reports that loader's copy of the
    // return / parameter types (gated; see `descriptor_to_class_mirror_via_loader`).
    __lap.mark(1);
    let (param_descs, ret_desc) = parse_descriptor_param_and_return(&meta.descriptor);
    let ret_mirror = descriptor_to_class_mirror_via_loader(ctx, &ret_desc, meta.declaring_class_id);
    let ret_mirror_pin = ctx.pin_native_root(ret_mirror);

    // Parameter type mirrors array. GC-safe: `descriptor_to_class_mirror`
    // allocates/loads classes, so the array is pinned across the fill loop
    // (see `build_mirror_array` вЂ” WildFly bug-06).
    __lap.mark(2);
    let class_comp = class_component_id(ctx);
    let decl_cid = meta.declaring_class_id;
    let param_arr = build_mirror_array_comp(ctx, class_comp, param_descs.len(), |ctx, i| {
        Ok(descriptor_to_class_mirror_via_loader(
            ctx,
            &param_descs[i],
            decl_cid,
        ))
    })?;
    let param_arr_pin = ctx.pin_native_root(param_arr);

    // G2: Always allocate non-null array fields. JDK 25 `Method` and its
    // parent `Executable` declare `exceptionTypes` (Class[]) and several
    // byte-array fields (`annotations`, `parameterAnnotations`,
    // `annotationDefault`). If these are left as the default `null`,
    // any caller doing `arr.length` or `arr.clone()` or even just
    // iterating will NPE вЂ” most notably ByteBuddy's
    // `JavaDispatcher.<clinit>` which does `arraylength` on a `Method`-
    // returned array. The real JDK guarantees these fields are non-null
    // (initialised by the `Method` constructor); we mirror that.
    //
    // WP2.1 вЂ” populate `exceptionTypes` from the JVMS В§4.7.5 `Exceptions`
    // attribute when present, so `Method.getExceptionTypes()` (which the
    // JDK Java code implements by `return exceptionTypes.clone();`)
    // returns the actual throws-clause types instead of always-empty.
    __lap.mark(3);
    // Already resolved by whoever built this metadata (see
    // `MethodMetadata::exceptions`). Re-deriving it here meant an O(methods)
    // search of the declaring class's method table per mirror.
    let exception_names = meta.exceptions.clone();
    // GC-safe (see `build_mirror_array`): each `descriptor_to_class_mirror`
    // allocates/loads classes, so the array is pinned across the fill loop.
    // Build a Class<T> mirror for each thrown checked exception via the L-form
    // so class loading + caching go through the same path as elsewhere.
    __lap.mark(4);
    // Resolve through the declaring class's own loader, exactly as the return
    // and parameter types above do: a throws-clause type must be the copy the
    // declaring class can see, or `java.lang.reflect.Proxy`'s `ensureVisible`
    // (`Class.forName(c.getName(), false, loader) != c`) rejects the interface
    // (`ApplicationContext.getMessage(..) throws NoSuchMessageException` under
    // Spring Boot's `ModifiedClassPathClassLoader`).
    let exc_decl_cid = meta.declaring_class_id;
    let exception_arr =
        build_mirror_array_comp(ctx, class_comp, exception_names.len(), |ctx, i| {
            let desc = format!("L{};", exception_names[i]);
            Ok(descriptor_to_class_mirror_via_loader(
                ctx,
                &desc,
                exc_decl_cid,
            ))
        })?;
    let exception_arr_pin = ctx.pin_native_root(exception_arr);
    let desc_str = ctx.create_string(&meta.descriptor);
    let desc_str_pin = ctx.pin_native_root(desc_str);
    // SBR wave 5 (reflmeta): the three annotation byte fields, as HotSpot
    // fills them (`member_annotation_bytes`; `None` = HotSpot's `null`).
    let ann_arrs = member_annotation_bytes(
        ctx,
        meta.declaring_class_id,
        &meta.name,
        &meta.descriptor,
        true,
        &[
            cratonvm_native_api::RawAnnotationAttr::Annotations,
            cratonvm_native_api::RawAnnotationAttr::ParameterAnnotations,
            cratonvm_native_api::RawAnnotationAttr::AnnotationDefault,
        ],
    );

    // Re-read every pinned local's forwarded reference now that all the
    // classloading/allocation above has settled.
    __lap.mark(5);
    let obj = ctx.read_native_pin(obj_pin, obj);
    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
    let name_str = ctx.read_native_pin(name_str_pin, name_str);
    let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
    let param_arr = ctx.read_native_pin(param_arr_pin, param_arr);
    let exception_arr = ctx.read_native_pin(exception_arr_pin, exception_arr);
    let desc_str = ctx.read_native_pin(desc_str_pin, desc_str);

    // --- Real JDK Method layout (visible to Java bytecode via Getfield) ---
    __lap.mark(6);
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(obj, "returnType", Value::Object(Some(ret_mirror)));
    ctx.set_field_by_name(obj, "parameterTypes", Value::Object(Some(param_arr)));
    // G2 + WP2.1: exceptionTypes is a non-null Class[] populated from the
    // `Exceptions` class-file attribute (or empty if no throws clause).
    // `Method.getExceptionTypes()` does `exceptionTypes.clone()` вЂ” if this
    // were null, ByteBuddy / Mockito / Spring AOP clinit paths would NPE.
    ctx.set_field_by_name(obj, "exceptionTypes", Value::Object(Some(exception_arr)));
    // `JVM_RECOGNIZED_METHOD_MODIFIERS` (see `create_field_object`).
    ctx.set_field_by_name(
        obj,
        "modifiers",
        Value::Int(meta.access_flags as i32 & RECOGNIZED_METHOD_MODIFIERS),
    );
    // JDK's `slot` is an opaque vmindex we don't populate; default 0. Under
    // `CRATONVM_REFLECT_INVOKE_PLAN` (interpreter round i3 wave 74, lane L3) it
    // names the method's invocation plan ([`MethodPlan`]), which a real
    // `Method.copy()` copies with it.
    let plan_slot = method_plan_slot(&*ctx, meta);
    ctx.set_field_by_name(obj, "slot", Value::Int(plan_slot));
    // `callerSensitive` is a byte cache; 0 means "not yet computed".
    ctx.set_field_by_name(obj, "callerSensitive", Value::Int(0));
    // Annotation raw-byte fields (`annotations`, `parameterAnnotations`,
    // `annotationDefault`) вЂ” all `byte[]` on JDK 25. These MUST be `null`
    // when no annotation bytes are present, NOT empty arrays.
    //
    // CratonVM serves method-annotation reflection via dedicated natives
    // (`native_method_get_annotations` etc.), so the raw bytes are usually
    // never consumed. But once any class is redefined (Mockito/ByteBuddy
    // inline mock of a JDK class such as `java.lang.reflect.Method`), the
    // native shadow on `java/lang/reflect/Method.getDeclaredAnnotations`
    // is suppressed and the real JDK bytecode runs instead:
    //   `Executable.declaredAnnotations()`
    //     -> `AnnotationParser.parseAnnotations(getAnnotationBytes(), вЂ¦)`.
    // `parseAnnotations` short-circuits only on a `null` byte[]; a non-null
    // EMPTY array flows into `parseAnnotations2`, whose very first read is
    // `buf.getShort()` (the 2-byte annotation count) вЂ” a 0-length buffer
    // underflows, surfacing as `AnnotationFormatError: Unexpected end of
    // annotations.` That aborts `Mockito.mock(Method.class)` and any test
    // class (e.g. spring-aop `MethodMatchersTests`) that mocks a reflection
    // type in `<clinit>`. The class-level `getRawAnnotations` native already
    // returns `null` for the same reason, and `create_field_object` /
    // `create_constructor_object` leave these fields `null` вЂ” matching
    // HotSpot, where an unannotated member's annotation bytes are `null`.
    // The earlier "empty, not null" rationale was mistaken:
    // `AnnotationParser` does not read `arr.length`, and the JDK getters for
    // all three fields (`Executable.sharedGetParameterAnnotations`,
    // `Method.getDefaultValue`) explicitly null-check before parsing.
    ctx.set_field_by_name(obj, "annotations", Value::Object(None));
    ctx.set_field_by_name(obj, "parameterAnnotations", Value::Object(None));
    ctx.set_field_by_name(obj, "annotationDefault", Value::Object(None));
    for (slot, field) in ["annotations", "parameterAnnotations", "annotationDefault"]
        .into_iter()
        .enumerate()
    {
        if let Some((a, pin)) = ann_arrs[slot] {
            let a = ctx.read_native_pin(pin, a);
            ctx.set_field_by_name(obj, field, Value::Object(Some(a)));
        }
    }

    // Synthetic-JDK mode can load `java/lang/reflect/Method` without any real
    // field table. Production `set_field_by_name` silently skips those writes,
    // while Method.getName()/getReturnType()/toString() still need readable
    // mirror state.
    //
    // The gate is "does this class HAVE a named field table", NOT "did the
    // named writes land". Those are different questions, and answering the
    // second one was a corruption bug: on a real-layout `Method` the flat
    // `METHOD_LEGACY_SLOT_*` indices are other live fields, so a failed named
    // write turned into `clazz` being overwritten with the RETURN TYPE (slot 2)
    // — Byte Buddy then reported `public abstract int int.value() does not
    // represent interface …Argument`. See `method_class_has_named_layout`.
    __lap.mark(7);
    let has_named_layout = method_class_has_named_layout(ctx, class_id);
    let named_method_layout_landed = matches!(
        ctx.get_field_by_name(obj, "name"),
        Value::Object(Some(actual)) if actual == name_str
    ) && matches!(
        ctx.get_field_by_name(obj, "clazz"),
        Value::Object(Some(actual)) if actual == class_mirror
    );
    if has_named_layout && !named_method_layout_landed && crate::nbflags().iae_trace_ok {
        eprintln!(
            "METHOD-NAMED-WRITE-MISSED class={} name={} — real layout present but \
             clazz/name did not read back; leaving the JDK fields alone",
            ctx.class_name_of_id(meta.declaring_class_id)
                .unwrap_or_default(),
            meta.name
        );
    }
    if !has_named_layout {
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_CLAZZ,
            Value::Object(Some(class_mirror)),
        );
        ctx.set_field(obj, METHOD_LEGACY_SLOT_NAME, Value::Object(Some(name_str)));
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_RETURN_TYPE,
            Value::Object(Some(ret_mirror)),
        );
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_PARAMETER_TYPES,
            Value::Object(Some(param_arr)),
        );
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_EXCEPTION_TYPES,
            Value::Object(Some(exception_arr)),
        );
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_MODIFIERS,
            Value::Int(meta.access_flags as i32),
        );
        ctx.set_field(obj, METHOD_LEGACY_SLOT_SLOT, Value::Int(0));
        ctx.set_field(obj, METHOD_LEGACY_SLOT_CALLER_SENSITIVE, Value::Int(0));
        ctx.set_field(obj, METHOD_LEGACY_SLOT_OVERRIDE, Value::Int(0));
        ctx.set_field(obj, METHOD_LEGACY_SLOT_ANNOTATIONS, Value::Object(None));
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_PARAMETER_ANNOTATIONS,
            Value::Object(None),
        );
        ctx.set_field(
            obj,
            METHOD_LEGACY_SLOT_ANNOTATION_DEFAULT,
            Value::Object(None),
        );
    }

    // WP2.1: populate the JDK `signature` field from the JVMS В§4.7.9 Signature
    // attribute when the method is generic. The real JDK `Method.getGenericReturnType()`
    // / `getGenericParameterTypes()` are pure-Java methods that read this field
    // via `getGenericSignature()`; when it is null they fall back to the erased
    // `returnType` / `parameterTypes`. CratonVM dispatches those reflective
    // getters to the JDK bytecode (not the registered natives), so leaving
    // `signature` null made every generic method type come back erased вЂ” which
    // breaks Jackson bean-property type resolution (`Map<String,Foo>` getter в†’
    // `LinkedHashMap` values в†’ ClassCastException). Setting it here makes the
    // JDK Java code recover the full `ParameterizedType`, matching real-JVM
    // behaviour. Mirrors how `create_field_object` relies on the field
    // Signature attribute for `Field.getGenericType()`.
    __lap.mark(8);
    if let Some(sig) = meta.signature.clone() {
        let sig_obj = ctx.create_string(&sig);
        // `create_string` above can allocate/classload —
        // re-read `obj` before writing into it.
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field_by_name(obj, "signature", Value::Object(Some(sig_obj)));
    }

    __lap.mark(9);
    // --- CratonVM extra metadata (append after JDK layout) ---
    let obj = ctx.read_native_pin(obj_pin, obj);
    let desc_str = ctx.read_native_pin(desc_str_pin, desc_str);
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_DESC,
        Value::Object(Some(desc_str)),
    );
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_PARAM_COUNT,
        Value::Int(param_descs.len() as i32),
    );
    ctx.set_field(obj, base + METHOD_EXTRA_OFFSET_ACCESSIBLE, Value::Int(0));
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_TRUSTED,
        Value::Int(METHOD_EXTRA_TRUSTED_MARKER),
    );

    __lap.mark(10);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

/// Read the CratonVM-specific raw descriptor extra slot from a Method object.
pub fn read_method_descriptor(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    let idx = base.saturating_add(METHOD_EXTRA_OFFSET_DESC);
    if ctx.object_num_fields(method_obj) <= idx {
        // SBR wave 5 (reflmeta): a real `copy()` has no tail. Its member is
        // its `root`'s, so a root mint's trusted descriptor answers; with no
        // such root, the JDK `parameterTypes`/`returnType` do.
        if let Some(root) = reflect_root(ctx, method_obj) {
            if method_has_trusted_metadata(ctx, root) {
                return read_method_descriptor(ctx, root);
            }
        }
        if !reflect_object_has_tail(ctx, method_obj, class_id)
            && method_object_has_named_layout(ctx, method_obj)
        {
            return Some(compose_method_descriptor_from_type_fields(ctx, method_obj));
        }
        return None;
    }
    match ctx.get_field(method_obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Whether this Method mirror was constructed by CratonVM with a dedicated,
/// post-layout metadata tail. Unmarked real-JDK mirrors can have an unrelated
/// field at the calculated tail offset and must not skip descriptor validation.
#[inline]
pub fn method_has_trusted_metadata(ctx: &dyn NativeContext, method_obj: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    let marker = base + METHOD_EXTRA_OFFSET_TRUSTED;
    ctx.object_num_fields(method_obj) > marker
        && matches!(
            ctx.get_field(method_obj, marker),
            Value::Int(METHOD_EXTRA_TRUSTED_MARKER)
        )
}

/// Single type token for a `java.lang.Class` mirror (`java/lang/String` в†’
/// `Ljava/lang/String;`, `[I` в†’ `[I`, primitives в†’ `I`/`J`/вЂ¦).
pub fn mirror_to_jvm_descriptor_token(ctx: &dyn NativeContext, mirror: ObjectRef) -> String {
    let name = mirror_class_name(ctx, mirror).unwrap_or_default();
    if name.is_empty() {
        return "Ljava/lang/Object;".to_string();
    }
    match name.as_str() {
        "void" => "V".to_string(),
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        s if s.starts_with('[') => s.to_string(),
        s => format!("L{s};"),
    }
}

/// Real JDK `java.lang.reflect.Method` instances do not populate our extra
/// descriptor slot (`METHOD_EXTRA_OFFSET_DESC`). Without a descriptor,
/// `Method.invoke` used to assume `()V` when `returnType` could not be read,
/// and [`box_value`] turned every reference return into `null` вЂ” breaking
/// Surefire's `LazyLauncher` (`getLauncher()` в†’ null). Missing `returnType`
/// now falls back to `Ljava/lang/Object;` while still honouring
/// `parameterTypes`.
pub fn compose_method_descriptor_from_type_fields(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> String {
    // When `returnType` is missing (layout mismatch on some `Method`
    // mirrors), returning an empty string made `parse_descriptor_param_and_return`
    // treat the return as `void`, so `Method.invoke` boxed real reference
    // results as `null` вЂ” Surefire `LazyLauncher` then kept a null delegate
    // (`Cannot invoke discover on null`). Default missing return to
    // `java.lang.Object` (still correct for `void`: the invoke path returns
    // `None` before boxing).
    let ret_token = match method_return_type_value(ctx, method_obj) {
        Value::Object(Some(m)) => mirror_to_jvm_descriptor_token(ctx, m),
        _ => "Ljava/lang/Object;".to_string(),
    };

    let params_arr = match method_parameter_types_value(ctx, method_obj) {
        Value::Object(Some(arr)) => arr,
        _ => return format!("(){ret_token}"),
    };
    let len = ctx.array_length(params_arr);
    let mut out = String::with_capacity(8 + len * 16);
    out.push('(');
    for i in 0..len {
        if let Value::Object(Some(pm)) = ctx.get_array_element(params_arr, i) {
            out.push_str(&mirror_to_jvm_descriptor_token(ctx, pm));
        }
    }
    out.push(')');
    out.push_str(&ret_token);
    out
}

/// Descriptor for `Method.invoke`: CratonVM extra slot if present and
/// well-formed; otherwise reconstruct from JDK `Executable` fields.
///
/// C6/Surefire: `method_extra_base` uses `max(LEGACY_FLOOR, class field
/// count)`. For real `java.lang.reflect.Method` instances the extra slot
/// index can still land **inside** the JDK instance layout (below the true
/// tail). That slot may hold an unrelated reference (e.g. `signature`) that
/// `read_string` turns into garbage вЂ” **not** starting with `(`. We must
/// not treat that as a JVM method descriptor or `Method.invoke` boxes
/// reference returns as `null` (wrong `()V` shape).
pub fn method_descriptor_for_invoke(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> String {
    fn looks_like_jvm_method_descriptor(d: &str) -> bool {
        let d = d.trim();
        let close = match (d.as_bytes().first(), d.find(')')) {
            (Some(b'('), Some(c)) => c,
            _ => return false,
        };
        // Must have at least one return-type token after ')'.
        close + 1 < d.len()
    }

    // Mirrors built by `create_method_object` own a post-layout descriptor
    // slot that Java code cannot mutate. Take that fast path before doing the
    // defensive descriptor scan/reconstruction required for a real JDK
    // `Method` mirror. Besides avoiding field walks, returning the stored
    // string directly avoids a second allocation from `trim().to_string()` on
    // every reflective invocation.
    if method_has_trusted_metadata(ctx, method_obj) {
        if let Some(d) = read_method_descriptor(ctx, method_obj) {
            return d;
        }
    }
    // SBR wave 5 (reflmeta): a real `copy()` (the copy door is retired under
    // `--real-jdk`) has no tail; its `root` is the mint it was copied from and
    // names the same member, so the root's trusted descriptor answers without
    // the per-call reconstruction below.
    if let Some(root) = reflect_root(ctx, method_obj) {
        if method_has_trusted_metadata(ctx, root) {
            if let Some(d) = read_method_descriptor(ctx, root) {
                return d;
            }
        }
    }

    if let Some(d) = read_method_descriptor(ctx, method_obj) {
        if looks_like_jvm_method_descriptor(&d) {
            let composed = compose_method_descriptor_from_type_fields(ctx, method_obj);
            let (slot_params, slot_ret) = parse_descriptor_param_and_return(d.trim());
            let (comp_params, comp_ret) = parse_descriptor_param_and_return(&composed);
            // Only trust the CratonVM extra-slot descriptor when it agrees with
            // the Executable mirrors (`parameterTypes` / `returnType`). If the
            // slot lands on an unrelated `String` (C6), it can look like a
            // valid JVM descriptor but disagree вЂ” e.g. bogus `()V` while the
            // method returns `Launcher` (Surefire `LazyLauncher` NPE).
            if slot_params.len() != comp_params.len() || slot_ret != comp_ret {
                return composed;
            }
            return d.trim().to_string();
        }
    }
    compose_method_descriptor_from_type_fields(ctx, method_obj)
}

/// Read the CratonVM-specific cached parameter count extra slot.
pub fn read_method_param_count(ctx: &dyn NativeContext, method_obj: cratonvm_types::ObjectRef) -> i32 {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    // SBR wave 5 (reflmeta): no tail (a real `copy()`): `parameterTypes`.
    if ctx.object_num_fields(method_obj) <= base + METHOD_EXTRA_OFFSET_PARAM_COUNT {
        return match method_parameter_types_value(ctx, method_obj) {
            Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
            _ => 0,
        };
    }
    match ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_PARAM_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    }
}

/// Read the CratonVM-specific accessible flag extra slot.
///
/// In real JDK 25, `Method.setAccessible(boolean)` is NOT a native вЂ” it's
/// inherited from `AccessibleObject.setAccessible(boolean)` which sets the
/// `override` field directly via Java putfield. When dispatch routes through
/// the Java method (or some other path that bypasses `native_method_set_accessible`),
/// the CratonVM extra slot stays at 0 even though `override == true`.
///
/// Therefore we consult BOTH:
///   1. The JDK-inherited `override` field (canonical bool, accessed via
///      `get_field_by_name` so it works whether stored as Int(0/1) or other
///      truthy encodings).
///   2. The CratonVM extra-slot fallback (kept for paths that only set the
///      extra slot вЂ” e.g. internal write helpers).
///
/// Either being truthy is enough to treat the Method as accessible.
pub fn read_method_accessible(ctx: &dyn NativeContext, method_obj: cratonvm_types::ObjectRef) -> bool {
    // Check the JDK `override` field first вЂ” this is what JDK 25's
    // AccessibleObject.setAccessible writes via Java bytecode.
    if let Value::Int(v) =
        method_int_field_value_or_legacy(ctx, method_obj, "override", METHOD_LEGACY_SLOT_OVERRIDE)
    {
        if v != 0 {
            return true;
        }
    }
    // Fall back to the CratonVM extra slot (set by our native setAccessible).
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    // SBR wave 5 (reflmeta): a real `copy()` has no tail; `override` is all.
    if ctx.object_num_fields(method_obj) <= base + METHOD_EXTRA_OFFSET_ACCESSIBLE {
        return false;
    }
    match ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_ACCESSIBLE) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

/// Write the CratonVM-specific accessible flag extra slot on a Method object.
pub fn write_method_accessible(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    // SBR wave 5 (reflmeta): a real `copy()` has no tail to write; its flag
    // is the JDK `override` field, which `read_method_accessible` reads first.
    if ctx.object_num_fields(method_obj) <= base + METHOD_EXTRA_OFFSET_ACCESSIBLE {
        if method_class_has_named_layout(ctx, class_id) {
            ctx.set_field_by_name(method_obj, "override", Value::Int(i32::from(value)));
        }
        return;
    }
    ctx.set_field(
        method_obj,
        base + METHOD_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(if value { 1 } else { 0 }),
    );
    // Same rule as `create_method_object`: the flat legacy index is only a
    // spare slot on a Method class with NO named field table. On a real layout
    // index 6 belongs to some other JDK field. See `method_class_has_named_layout`.
    if matches!(
        ctx.get_field_by_name(method_obj, "override"),
        Value::Object(None)
    ) && !method_class_has_named_layout(ctx, class_id)
    {
        ctx.set_field(
            method_obj,
            METHOD_LEGACY_SLOT_OVERRIDE,
            Value::Int(if value { 1 } else { 0 }),
        );
    }
}

/// Public wrapper used by `lang_reflect::native_method_try_set_accessible`.
pub fn write_method_accessible_external(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    write_method_accessible(ctx, method_obj, value);
}

// --- Method getters ---

/// Does this reflection object's class carry the REAL JDK named field table?
///
/// The flat `METHOD_LEGACY_SLOT_*` indices are a stand-in for synthetic-JDK
/// mode, where `java/lang/reflect/Method` is loaded with no field table at all.
/// On a REAL-layout `Method` those indices are not a "fallback" — they are
/// other, live fields. Writing `ret_mirror` at index 2 there does not supply a
/// missing return type, it OVERWRITES whatever the JDK declares at index 2; and
/// reading index 0 back for `clazz` yields that field rather than the declaring
/// class.
///
/// That is not theoretical. With Mockito's inline mock maker retransforming JDK
/// classes, Byte Buddy reported
/// `public abstract int int.value() does not represent interface
/// net.bytebuddy.implementation.bind.annotation.Argument` — the declaring type
/// of `Argument.value()` read back as `int`, which is that method's RETURN type.
/// Same family: `Method.getName()` answering something that fails Byte Buddy's
/// bean-naming check, and `getReturnType()` answering a non-`Class`.
///
/// So the legacy slots are only ever correct when there is no named layout to
/// collide with. Probe for `clazz`, which every real `Method`/`Constructor`
/// declares.
pub fn method_class_has_named_layout(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    ctx.resolve_field_index_by_class_id(class_id, "clazz")
        .is_some()
}

pub fn method_object_has_named_layout(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> bool {
    method_class_has_named_layout(ctx, ctx.class_id_of_object(method_obj))
}

pub fn method_object_field_value_or_legacy(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    field_name: &str,
    legacy_slot: usize,
) -> Value {
    let named = ctx.get_field_by_name(method_obj, field_name);
    if matches!(named, Value::Object(Some(_))) {
        return named;
    }
    // A genuinely-null named field (`signature` on a non-generic method,
    // `annotations` on an unannotated one) must stay null rather than pick up
    // whatever unrelated field shares the legacy index.
    if method_object_has_named_layout(ctx, method_obj) {
        return named;
    }
    let legacy = ctx.get_field(method_obj, legacy_slot);
    if matches!(legacy, Value::Object(Some(_))) {
        return legacy;
    }
    named
}

pub fn method_int_field_value_or_legacy(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    field_name: &str,
    legacy_slot: usize,
) -> Value {
    let named = ctx.get_field_by_name(method_obj, field_name);
    if matches!(named, Value::Int(_)) {
        return named;
    }
    if method_object_has_named_layout(ctx, method_obj) {
        return named;
    }
    let legacy = ctx.get_field(method_obj, legacy_slot);
    if matches!(legacy, Value::Int(_)) {
        return legacy;
    }
    named
}

pub fn method_clazz_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    method_object_field_value_or_legacy(ctx, method_obj, "clazz", METHOD_LEGACY_SLOT_CLAZZ)
}

pub fn method_name_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    method_object_field_value_or_legacy(ctx, method_obj, "name", METHOD_LEGACY_SLOT_NAME)
}

pub fn method_return_type_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    method_object_field_value_or_legacy(
        ctx,
        method_obj,
        "returnType",
        METHOD_LEGACY_SLOT_RETURN_TYPE,
    )
}

pub fn method_parameter_types_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    method_object_field_value_or_legacy(
        ctx,
        method_obj,
        "parameterTypes",
        METHOD_LEGACY_SLOT_PARAMETER_TYPES,
    )
}

pub fn method_exception_types_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    method_object_field_value_or_legacy(
        ctx,
        method_obj,
        "exceptionTypes",
        METHOD_LEGACY_SLOT_EXCEPTION_TYPES,
    )
}

pub fn method_modifiers_value(
    ctx: &dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Value {
    // A mirror THIS VM built already carries the declaring member's exact
    // class-file `access_flags`: `create_method_object` writes `modifiers`
    // from `MethodMetadata::access_flags`, which is the same
    // `m.access_flags.bits()` the class-table search below would find. Taking
    // the field is not a micro-optimisation — the search calls
    // `ctx.declared_methods(class_id)`, which allocates a fresh
    // `MethodMetadata` (two owned `String`s and two `Vec`s) for EVERY method
    // the class declares, per call. That made `Method.getModifiers()` cost
    // 4.2 us on a 125-method class and 39.0 us on jOOQ's 1003-method
    // `DefaultDSLContext`, against HotSpot's 1 ns — linear in the declaring
    // class's method count, for a value already sitting in a field.
    //
    // Spring's `AnnotationsScanner.isOverride` calls it once per candidate
    // method pair: a native census of one `MethodIntrospector.selectMethods`
    // pass over `DefaultDSLContext` counted 1 549 873 calls, which is the
    // whole of the 65-75 s that pass took (retired write-up:
    // `jooqautoconfigurationtests-timeout-regression-20260805`).
    //
    // The search stays for a mirror we did NOT build, which is what the
    // comment below is about: a JDK-private `Method` copy is allocated by JDK
    // bytecode at the real JDK width, so it has no post-layout metadata tail
    // and fails `method_has_trusted_metadata` — the same marker
    // `read_method_descriptor` already refuses to trust without.
    if method_has_trusted_metadata(ctx, method_obj) {
        if let v @ Value::Int(_) = method_int_field_value_or_legacy(
            ctx,
            method_obj,
            "modifiers",
            METHOD_LEGACY_SLOT_MODIFIERS,
        ) {
            return v;
        }
    }
    // JDK-private Method copies can retain a truncated `modifiers` field.
    // Prefer the loaded declaring member's exact class-file metadata.
    if let (Value::Object(Some(clazz)), Value::Object(Some(name))) = (
        method_clazz_value(ctx, method_obj),
        method_name_value(ctx, method_obj),
    ) {
        if let (Some(class_id), Some(name)) = (mirror_class_id(ctx, clazz), ctx.read_string(name)) {
            let descriptor = method_descriptor_for_invoke(ctx, method_obj);
            if let Some(method) = ctx
                .declared_methods(class_id)
                .into_iter()
                .find(|method| method.name == name && method.descriptor == descriptor)
            {
                return Value::Int(method.access_flags as i32);
            }
        }
    }
    method_int_field_value_or_legacy(ctx, method_obj, "modifiers", METHOD_LEGACY_SLOT_MODIFIERS)
}

/// `Method.getName` — `()Ljava/lang/String;`. See `native_field_get_name`:
/// a by-name read cannot return a reference-typed answer for an unwritten
/// slot, so resolve and read by index instead.
pub fn native_method_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::field_read::ref_field(ctx, this, "name")))}

pub fn native_method_get_return_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(method_return_type_value(ctx, this)))
}

pub fn native_method_get_parameter_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(method_parameter_types_value(ctx, this)))}

pub fn native_method_get_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(method_modifiers_value(ctx, this)))}

pub fn native_method_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(method_clazz_value(ctx, this)))}

pub fn native_method_get_parameter_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(read_method_param_count(ctx, this))))}

// --- Method.invoke ---

/// Fast-path predicate for `Method.invoke`: returns true iff every entry in
/// `args` already matches its target descriptor *exactly* (no widening,
/// unboxing, or subtype check needed). The common Spring-DI / Jackson hot
/// path passes call-sites whose arg types match the formal parameter types
/// 1:1; in that case the per-arg `coerce_arg_strict` work (descriptor
/// re-parse, wrapper-class lookup, widening table walk) is pure overhead.
///
/// Conservative on purpose: we only fast-path obvious matches. Anything
/// less clear-cut (boxed primitive, subtype reference, array-of-subtype)
/// falls through to the existing strict-coercion path.
pub fn args_match_descriptor_exactly(
    ctx: &dyn NativeContext,
    args: &[Value],
    param_descs: &[String],
) -> bool {
    if args.len() != param_descs.len() {
        return false;
    }
    for (arg, pdesc) in args.iter().zip(param_descs.iter()) {
        match (arg, pdesc.as_str()) {
            // Primitive tags must match the descriptor exactly. The
            // wrapper-class boxed forms always arrive as `Value::Object`
            // and fall through to coercion.
            (Value::Int(_), "I")
            | (Value::Int(_), "Z")
            | (Value::Int(_), "B")
            | (Value::Int(_), "S")
            | (Value::Int(_), "C") => {}
            (Value::Long(_), "J") => {}
            (Value::Float(_), "F") => {}
            (Value::Double(_), "D") => {}
            // Reference parameter:
            //   * null is assignable to any reference type вЂ” fast-path OK.
            //   * non-null: runtime class name must equal the descriptor's
            //     declared inner class. Subtype assignability requires the
            //     slow path's class-hierarchy walk.
            (Value::Object(None), d) if d.starts_with('L') || d.starts_with('[') => {}
            (Value::Object(Some(obj)), d) if d.starts_with('L') && d.ends_with(';') => {
                let inner = &d[1..d.len() - 1];
                let cid = ctx.class_id_of_object(*obj);
                // The interned name (a refcount bump), not a fresh `String`
                // (interpreter round i3 wave 76, lane L3; same answer).
                match ctx.class_name_arc_of_id(cid) {
                    Some(name) if &*name == inner => {}
                    _ => return false,
                }
            }
            // Array descriptors: trust only the obvious null case above; a
            // non-null array of a possibly-subtype element requires the
            // slow path's covariant assignability check.
            _ => return false,
        }
    }
    true
}

/// Build a `java.lang.invoke.SerializedLambda` standing in for the lambda
/// `proxy`, populated from its registered call-site metadata and captured
/// field values. This is the value the JVM's generated `writeReplace()` would
/// return; real `ObjectOutputStream` serializes it, and `readResolve()` on the
/// other side reconstructs the lambda via the capturing class's generated
/// `$deserializeLambda$` (which re-runs the lambda's invokedynamic).
/// What lambda proxy `proxy`'s spun `writeReplace()` answers: its
/// `SerializedLambda` ([`build_serialized_lambda`]), or `None` when `proxy` is
/// no lambda proxy. The caller decides the proxy declares one (a
/// `FLAG_SERIALIZABLE` lambda). The one builder `Method.invoke` and the VM's
/// virtual call of `writeReplace()` on a proxy (a method handle's, interpreter
/// round i2 wave 61, lane L4; `L4W61LambdaProxyMemberHandles`) share.
pub fn lambda_proxy_write_replace(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
) -> Option<MethodCallResult> {
    let proxy_cid = ctx.class_id_of_object(proxy);
    let meta = ctx.lambda_proxy_serial_metadata(proxy_cid)?;
    Some(build_serialized_lambda(ctx, proxy_cid, proxy, &meta))
}

/// What the serialization-hostile `writeObject(ObjectOutputStream)` /
/// `readObject(ObjectInputStream)` of an ACCIDENTALLY serializable lambda
/// proxy throw (JDK 25 `InnerClassLambdaMetafactory.
/// generateSerializationHostileMethods`): `NotSerializableException("Non-serializable
/// lambda")`, unwrapped (the caller wraps it as `Method.invoke` does). `None`
/// when `receiver` is no such proxy, `method_name` is neither of the two, or the
/// `Method` (`method_obj`) has another descriptor. Interpreter round i4 wave 86,
/// lane L4.
pub fn lambda_proxy_hostile_serialization(
    ctx: &mut dyn NativeContext,
    method_obj: ObjectRef,
    receiver: ObjectRef,
    method_name: &str,
) -> Option<MethodCallFailed> {
    let descriptor = method_descriptor_for_invoke(&*ctx, method_obj);
    lambda_proxy_hostile_serialization_call(ctx, receiver, method_name, &descriptor)
}

/// [`lambda_proxy_hostile_serialization`] for a call by name and descriptor:
/// also the VM's VIRTUAL call of the pair on a proxy, which is what
/// `ObjectStreamClass.invokeWriteObject`'s `Method.invoke` becomes when the
/// JDK's reflective accessor runs (a method handle on the member). Wave 86
/// rework: that door answered the serialization hooks' neutral result (no-op),
/// so `ObjectOutputStream.writeObject` of an accidentally serializable lambda
/// succeeded.
pub fn lambda_proxy_hostile_serialization_call(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> Option<MethodCallFailed> {
    let want = match method_name {
        "writeObject" => "(Ljava/io/ObjectOutputStream;)V",
        "readObject" => "(Ljava/io/ObjectInputStream;)V",
        _ => return None,
    };
    if descriptor != want {
        return None;
    }
    let proxy_cid = ctx.class_id_of_object(receiver);
    if !is_lambda_proxy_id(proxy_cid)
        || ctx.lambda_proxy_serializability(proxy_cid)
            != cratonvm_native_api::LambdaSerializability::AccidentallySerializable
    {
        return None;
    }
    let msg = ctx.create_string_uninterned("Non-serializable lambda");
    Some(
        match ctx.new_object_initialized(
            "java/io/NotSerializableException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg))],
        ) {
            Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
            _ => cratonvm_types::error::RuntimeError::IOException {
                message: "java.io.NotSerializableException: Non-serializable lambda".into(),
            }
            .into(),
        },
    )
}

pub fn build_serialized_lambda(
    ctx: &mut dyn NativeContext,
    proxy_cid: ClassId,
    proxy: ObjectRef,
    meta: &cratonvm_native_api::LambdaSerialMetadata,
) -> MethodCallResult {
    // gen r4w3/rooting: `SerializedLambda`'s <clinit> is a GC point and `proxy`
    // is both the fallback return value and read below; pin it first.
    let proxy_pin = ctx.pin_native_root(proxy);
    let sl_cid = match ctx.ensure_class_initialized("java/lang/invoke/SerializedLambda") {
        Ok(id) => id,
        // No real SerializedLambda class (e.g. synthetic-jdk mode): fall back to
        // returning the proxy unchanged so the caller's serialization proceeds
        // via its own path rather than NPE-ing here.
        Err(_) => {
            let proxy = ctx.read_native_pin(proxy_pin, proxy);
            ctx.unpin_native_roots(proxy_pin);
            return Ok(Some(Value::Object(Some(proxy))));
        }
    };

    // Capturing class = where the lambda's invokedynamic appears (the class
    // whose `$deserializeLambda$` will reconstruct it). Falls back to the impl
    // class for proxies created off the indy path (e.g. reflective factories).
    let capturing_name = ctx
        .lambda_proxy_host(proxy_cid)
        .unwrap_or_else(|| meta.impl_class.clone());
    let capturing_mirror = match ctx.class_id_by_name(&capturing_name) {
        Some(cid) => Value::Object(Some(ctx.get_class_mirror(cid))),
        None => Value::Object(None),
    };

    // GC-safety: `box_value` (per capture-arg iteration below) and every
    // `create_string`/`alloc_object` further down can trigger a moving GC;
    // `proxy`, `captured`, the ObjectRef inside `capturing_mirror`, and `sl`
    // are all reused repeatedly across these hazards, unpinned otherwise.
    // Same "Family 1" stale-ObjectRef pattern as `create_method_object`/
    // `create_constructor_object`/`create_field_object` (see
    // wildfly-parallel-boot-stale-objectref-residual.md).
    // (gen r4w3/rooting: `proxy_pin` is now taken before the <clinit> above.)
    let capturing_mirror_pin = match capturing_mirror {
        Value::Object(Some(m)) => Some(ctx.pin_native_root(m)),
        _ => None,
    };
    // capturedArgs: one (boxed) value per proxy capture field, in order.
    let capture_chars: Vec<char> = meta.capture_types.chars().collect();
    let captured = ctx.new_ref_array(ClassId::new(0), capture_chars.len());
    let captured_pin = ctx.pin_native_root(captured);
    for (i, tc) in capture_chars.iter().enumerate() {
        let proxy = ctx.read_native_pin(proxy_pin, proxy);
        let raw = ctx.get_field(proxy, i);
        // CANONICAL: measured on HotSpot, `SerializedLambda.getCapturedArg(i)`
        // for a captured `int`/`char`/`long` is identical to `X.valueOf(v)` —
        // the spun `writeReplace` boxes the capture fields with `valueOf`
        // bytecode, so this side table inherits the caches too.
        let boxed = box_value_canonical(ctx, raw, &tc.to_string());
        let captured = ctx.read_native_pin(captured_pin, captured);
        ctx.set_array_element(captured, i, boxed);
    }

    let total = ctx.class_num_total_fields(sl_cid).max(10);
    let sl = ctx.alloc_object(sl_cid, total);
    let sl_pin = ctx.pin_native_root(sl);
    let capturing_mirror = match (capturing_mirror_pin, capturing_mirror) {
        (Some(pin), Value::Object(Some(m))) => Value::Object(Some(ctx.read_native_pin(pin, m))),
        _ => capturing_mirror,
    };
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl, "capturingClass", capturing_mirror);
    // Interpreter round i4 wave 98, lane L8: the spun `writeReplace` loads
    // all six strings with `ldc`, i.e. the INTERNED instances, so equal
    // strings are one object and `ObjectOutputStream` writes the second as a
    // `TC_REFERENCE` (`functionalInterfaceMethodSignature` and
    // `instantiatedMethodType` are both "()I" for an `IntSupplier`). Fresh
    // strings were written twice: the stream was one byte longer than
    // HotSpot's (`L8W98StreamBytes` row `lambda`).
    let fic = ctx.intern_string(&meta.functional_interface);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl, "functionalInterfaceClass", Value::Object(Some(fic)));
    let fimn = ctx.intern_string(&meta.sam_method_name);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(
        sl,
        "functionalInterfaceMethodName",
        Value::Object(Some(fimn)),
    );
    let fims = ctx.intern_string(&meta.sam_descriptor);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(
        sl,
        "functionalInterfaceMethodSignature",
        Value::Object(Some(fims)),
    );
    let ic = ctx.intern_string(&meta.impl_class);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl, "implClass", Value::Object(Some(ic)));
    let imn = ctx.intern_string(&meta.impl_member);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl, "implMethodName", Value::Object(Some(imn)));
    let ims = ctx.intern_string(&meta.impl_descriptor);
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl, "implMethodSignature", Value::Object(Some(ims)));
    ctx.set_field_by_name(sl, "implMethodKind", Value::Int(meta.impl_ref_kind as i32));
    let imt = ctx.intern_string(&meta.instantiated_descriptor);
    let sl = ctx.read_native_pin(sl_pin, sl);
    let captured = ctx.read_native_pin(captured_pin, captured);
    ctx.unpin_native_roots(proxy_pin);
    ctx.set_field_by_name(sl, "instantiatedMethodType", Value::Object(Some(imt)));
    ctx.set_field_by_name(sl, "capturedArgs", Value::Object(Some(captured)));
    Ok(Some(Value::Object(Some(sl))))
}

/// Round 13 wave 1 (lane mhffm patch): does a class strictly between the
/// receiver's class and the `Method`'s declaring class redeclare the
/// signature? Only then can a class of the declaring class's run-time package
/// below it override the package-private method (JVMS 5.4.5), which the exact
/// call of the cross-package branch would skip.
pub fn redeclared_between(
    ctx: &dyn NativeContext,
    recv: cratonvm_types::ObjectRef,
    declaring: Option<cratonvm_types::ClassId>,
    name: &str,
    desc: &str,
) -> bool {
    let Some(declaring) = declaring else {
        return false;
    };
    let receiver_class = ctx.class_id_of_object(recv);
    let mut cursor = ctx.superclass_of(receiver_class);
    let mut steps = 0usize;
    while let Some(cid) = cursor {
        if cid == declaring || steps >= 256 {
            return false;
        }
        steps += 1;
        if ctx.class_declares_method(cid, name, desc) {
            return true;
        }
        cursor = ctx.superclass_of(cid);
    }
    false
}

/// The declaring class `class_id` of a reflective door's member
/// (`Method.invoke`, `Constructor.newInstance`), initialized as JDK 25's
/// accessor factory initializes it: before the accessor runs, its
/// `ExceptionInInitializerError` / `NoClassDefFoundError` returned as is.
/// Interpreter round i2 wave 58, lane L3
/// (`docs/feature-designs/native-dispatch-architecture.md`).
/// An initialized class pays one atomic load.
#[inline]
pub fn reflective_door_initialize(
    ctx: &mut dyn NativeContext,
    class_id: Option<ClassId>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    match class_id {
        Some(class_id) if !ctx.is_class_initialized(class_id) => {
            reflective_door_initialize_cold(ctx, class_id)
        }
        _ => Ok(()),
    }
}

/// [`reflective_door_initialize`]'s initialization: the door notes the check
/// first, so the failure's capture lists the accessor factory's frames down to
/// `Unsafe.ensureClassInitialized0` (`ReflectiveCheck::ClassInit`).
#[cold]
#[inline(never)]
pub fn reflective_door_initialize_cold(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    ctx.note_reflective_check(cratonvm_native_api::ReflectiveCheck::ClassInit);
    ctx.initialize_class(class_id)
}

/// `Method.invoke`'s [`reflective_door_initialize_cold`] for a class that is
/// not initialized yet: the declaring `mirror`, the `receiver` and the
/// `args_array` the body reads after the `<clinit>` (a GC point) are pinned
/// across it and returned re-read (interpreter round i3 wave 79, lane L3).
/// The pins are released on every exit.
#[cold]
#[inline(never)]
pub fn reflective_door_initialize_pinned(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    mirror: ObjectRef,
    receiver: Option<ObjectRef>,
    args_array: Option<ObjectRef>,
) -> Result<
    (ObjectRef, Option<ObjectRef>, Option<ObjectRef>),
    cratonvm_types::error::MethodCallFailed,
> {
    let mirror_pin = ctx.pin_native_root(mirror);
    let receiver_pin = receiver.map(|r| (ctx.pin_native_root(r), r));
    let args_pin = args_array.map(|a| (ctx.pin_native_root(a), a));
    let initialized = reflective_door_initialize_cold(ctx, class_id);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    let receiver = receiver_pin.map(|(pin, r)| ctx.read_native_pin(pin, r));
    let args_array = args_pin.map(|(pin, a)| ctx.read_native_pin(pin, a));
    ctx.unpin_native_roots(mirror_pin);
    initialized.map(|()| (mirror, receiver, args_array))
}

/// Interpreter round i3 wave 74, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the per-`Method` invocation plan.
///
/// Every fact `Method.invoke` re-derived per call from the `Method` OBJECT
/// that is a function of the member alone -- the declaring class's id and
/// name, the method's name, its descriptor (a fresh `String`) and the parsed
/// parameter and return tokens (a `Vec<String>` and a `String`), the
/// modifiers, the `@CallerSensitiveAdapter` lookup -- is computed once when
/// the method is minted ([`create_method_object`]) and named by the mint's
/// `slot` field, as [`CtorPlan`] is for constructors: HotSpot's `slot` is the
/// method's index in its class, which no Java code reads, and this VM's mints
/// carried a constant `0` there. A real `Method.copy()` copies `slot`, so
/// every copy shares the root's plan. A method with no plan (`slot` 0: the
/// switch off, a proxy's method the VM builds itself, a synthetic layout)
/// takes the by-name path, byte for byte.
///
/// Caller-dependent steps are NOT part of a plan: the accessible flag, the
/// member, module and class gates, the receiver test, the argument count and
/// coercion and the class-initialization test run per call, and every refusal
/// is produced by the same code as without a plan (the plan only replaces
/// reads that succeed).
pub struct MethodPlan {
    pub class_id: ClassId,
    pub class_name: Arc<str>,
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub param_descs: Vec<String>,
    pub ret_desc: String,
    pub modifiers: i32,
    /// `java/lang/Object` declares it: every receiver is an instance.
    pub declaring_is_object: bool,
    /// [`NativeContext::caller_sensitive_adapter`]'s answer, asked on the
    /// first call of a static JDK method (it depends on the member and the
    /// VM's configuration only).
    pub cs_adapter: OnceLock<Option<Arc<str>>>,
    /// Interpreter round i3 wave 75, lane L3 (`CRATONVM_REFLECT_INVOKE_SETTLED`):
    /// the owner the settling static door returned as settled for this
    /// method (its class id + 1; `0`: not settled). Initialization is
    /// monotone, so one answer serves every thread; a call still runs
    /// [`reflective_door_initialize`] first.
    pub settled: std::sync::atomic::AtomicU32,
    /// Interpreter round i3 wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`):
    /// the `Method` object class the accessible-flag fallback last read its
    /// extra-slot base for, packed as `(class id + 1) << 32 | base` (`0`:
    /// none yet). A loaded class's field count never changes, so a hit on
    /// the same class id answers [`method_extra_base`] without the
    /// class-manager read.
    pub extra_base: std::sync::atomic::AtomicU64,
    /// Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`):
    /// the declaring class's access flags (`class_access_flags`), read once
    /// at mint time -- a loaded class's flags never change -- for the class
    /// half of `verifyMemberAccess`, which read them per call. `None`: the
    /// switch was off at mint time (the switch is per VM, read once), and the
    /// call reads them as before.
    pub class_flags: Option<u16>,
}

impl MethodPlan {
    /// The return descriptor (`I`, `V`, `Ljava/lang/String;`, ...).
    pub fn ret_desc(&self) -> &str {
        &self.ret_desc
    }

    /// The method descriptor.
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// [`method_extra_base`] of `class_id`, the `Method` object's class,
    /// remembered on the plan (see [`Self::extra_base`]).
    pub fn method_extra_base_cached(&self, ctx: &dyn NativeContext, class_id: ClassId) -> usize {
        use std::sync::atomic::Ordering;
        let tag = u64::from(class_id.as_u32()) + 1;
        let packed = self.extra_base.load(Ordering::Relaxed);
        if packed >> 32 == tag {
            return (packed & 0xFFFF_FFFF) as usize;
        }
        let base = method_extra_base(ctx, class_id);
        if let Ok(low) = u32::try_from(base) {
            self.extra_base
                .store((tag << 32) | u64::from(low), Ordering::Relaxed);
        }
        base
    }

    pub fn new(
        class_id: ClassId,
        class_name: Arc<str>,
        name: &str,
        descriptor: &str,
        modifiers: i32,
        class_flags: Option<u16>,
    ) -> Self {
        let (param_descs, ret_desc) = parse_descriptor_param_and_return(descriptor);
        MethodPlan {
            class_id,
            declaring_is_object: &*class_name == "java/lang/Object",
            class_name,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            param_descs,
            ret_desc,
            modifiers,
            cs_adapter: OnceLock::new(),
            settled: std::sync::atomic::AtomicU32::new(0),
            extra_base: std::sync::atomic::AtomicU64::new(0),
            class_flags,
        }
    }
}

#[derive(Default)]
pub struct MethodPlanTable {
    /// (declaring class id, name, descriptor) -> plan id (index + 1).
    pub ids: FxHashMap<(u32, Arc<str>, Arc<str>), u32>,
    pub plans: Vec<Arc<MethodPlan>>,
}

/// This VM's plans ([`NativeContext::reflect_door_cache_slot`]): interned
/// at mint time under the write lock, read per call under the read lock
/// (released before the call runs any Java, which may mint).
pub struct MethodPlans {
    pub table: cratonvm_types::lock_order::OrderedPlRwLock<MethodPlanTable>,
}

impl MethodPlans {
    /// Plans kept; a mint past it gets no plan (the by-name path).
    pub const CAP: usize = 1 << 20;

    pub fn new() -> Self {
        MethodPlans {
            table: cratonvm_types::lock_order::OrderedPlRwLock::new(
                MethodPlanTable::default(),
                cratonvm_types::lock_order::LockLevel::Scratch,
            ),
        }
    }

    /// The id (`>= 1`) of the plan of `(class_id, name, descriptor)`, made
    /// on first use; `0` when the table is full. A row whose class name or
    /// modifiers differ from the member being minted (a class id reused after
    /// an unload) is replaced, never shared.
    pub fn intern(
        &self,
        class_id: ClassId,
        class_name: Arc<str>,
        name: &str,
        descriptor: &str,
        modifiers: i32,
    ) -> i32 {
        self.intern_with_flags(class_id, class_name, name, descriptor, modifiers, None)
    }

    /// [`Self::intern`], a new plan keeping `class_flags` (the declaring
    /// class's access flags, read by the caller before any lock:
    /// `CRATONVM_REFLECT_INVOKE_PLAN_FACTS`, interpreter round i3 wave 77,
    /// lane L3).
    pub fn intern_with_flags(
        &self,
        class_id: ClassId,
        class_name: Arc<str>,
        name: &str,
        descriptor: &str,
        modifiers: i32,
        class_flags: Option<u16>,
    ) -> i32 {
        let key_name: Arc<str> = Arc::from(name);
        let key_desc: Arc<str> = Arc::from(descriptor);
        let key = (class_id.as_u32(), key_name, key_desc);
        let same =
            |plan: &MethodPlan| *plan.class_name == *class_name && plan.modifiers == modifiers;
        {
            let t = self.table.read();
            if let Some(&id) = t.ids.get(&key) {
                if t.plans.get(id as usize - 1).is_some_and(|p| same(&**p)) {
                    return id as i32;
                }
            }
        }
        let mut t = self.table.write();
        if let Some(&id) = t.ids.get(&key) {
            if t.plans.get(id as usize - 1).is_some_and(|p| same(&**p)) {
                return id as i32;
            }
        }
        if t.plans.len() >= Self::CAP {
            return 0;
        }
        let plan = MethodPlan::new(
            class_id,
            class_name,
            name,
            descriptor,
            modifiers,
            class_flags,
        );
        t.plans.push(Arc::new(plan));
        let id = t.plans.len() as u32;
        t.ids.insert(key, id);
        id as i32
    }

    /// The plan named by a `slot` value, if it names one.
    pub fn get(&self, id: i32) -> Option<Arc<MethodPlan>> {
        if id <= 0 {
            return None;
        }
        self.table.read().plans.get(id as usize - 1).cloned()
    }
}

/// This VM's served reflective doors' caches
/// ([`NativeContext::reflect_door_cache_slot`]): the `Method.invoke` plans
/// (`CRATONVM_REFLECT_INVOKE_PLAN`) and the `Field` door identity memo
/// (`CRATONVM_FIELD_DOOR_IDENT_MEMO`). Interpreter round i3 wave 74, lane L3.
pub struct ReflectDoorCaches {
    pub method_plans: MethodPlans,
    pub field_idents: FieldIdentMemo,
    /// `Constructor.newInstance` plans (`CRATONVM_REFLECT_CTOR_PLAN_PER_VM`,
    /// wave 75, lane L3).
    pub ctor_plans: CtorPlans,
    /// The doors' per-phase census (`CRATONVM_REFLECT_DOOR_PHASE_STATS`, wave
    /// 77, lane L3); written only with the switch on.
    pub phase_stats: crate::reflect_door_phases::DoorPhaseStats,
    /// `Class.getName()`'s memo (`CRATONVM_CLASS_GET_NAME_MEMO`, JIT round 20
    /// wave 18, lane xWalk; [`ClassGetNameMemo`]).
    pub class_names: ClassGetNameMemo,
    /// JIT round 21 wave 24, lane names (`CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`,
    /// opt-in; written only with it on): the strong holds of the `getName()`
    /// answers of mirrors with no class id (primitives, arrays the reverse map
    /// does not hold) -- answer text -> a global-root handle of the interned
    /// answer ([`ClassGetNameIdlessHolds`]).
    pub class_names_idless: ClassGetNameIdlessHolds,
    /// `java/lang/reflect/Constructor`'s `ClassId` once a by-name
    /// `ensure_class_initialized` answered it (`CRATONVM_REFLECT_CTOR_MIRROR_MEMO`,
    /// interpreter round i4 wave 90, lane L3); `0` = not yet (`ClassId 0` is
    /// `java/lang/Object`, never this class). Written only with the switch on.
    pub ctor_class_id: std::sync::atomic::AtomicU32,
    /// `CRATONVM_REFLECT_CTOR_MIRROR_MEMO` for this VM, read once when the
    /// caches are made (default on since the wave-90 triage, `=0` off).
    pub ctor_mirror_memo: bool,
    /// `CRATONVM_REFLECT_CTOR_LAZY_DESC_STRING` for this VM, read once when
    /// the caches are made (default off). Was a process `OnceLock` until
    /// interpreter round i4 wave 92, lane L3 (a switch value is per VM).
    pub ctor_lazy_desc: bool,
    /// `CRATONVM_REFLECT_CTOR_MINT_LEAN` for this VM (interpreter round i4
    /// wave 92, lane L3; default off): a `Constructor` mint takes the
    /// `Class[]` / `Constructor[]` component ids from
    /// [`Self::class_component_id`] / [`Self::ctor_component_id`] instead of a
    /// by-name `class_id_by_name` per call, and builds its tail descriptor
    /// `String` uninterned.
    pub ctor_mint_lean: bool,
    /// `java/lang/Class`'s `ClassId` once a by-name lookup answered it
    /// (`ctor_mint_lean`); `0` = not yet (`ClassId 0` is `java/lang/Object`).
    pub class_component_id: std::sync::atomic::AtomicU32,
    /// `java/lang/reflect/Constructor`'s `ClassId` as the listing's array
    /// component (`ctor_mint_lean`); `0` = not yet.
    pub ctor_component_id: std::sync::atomic::AtomicU32,
    /// `CRATONVM_BOX_WRAPPER_CID_PER_VM` for this VM (interpreter round i4
    /// wave 92, lane L3; default on since the wave-92 triage, `=0` off): `lang_math::alloc_wrapper` reads the
    /// eight wrapper classes' ids from [`Self::wrapper_cids`] (one `Acquire`
    /// load) instead of the process-global `WRAPPER_CIDS` mutex.
    pub wrapper_cid_per_vm: bool,
    /// The eight wrapper classes' `ClassId`s by `lang_math::wrapper_cid_key`
    /// index, `0` = not yet resolved (never a wrapper's id: `ClassId 0` is
    /// `java/lang/Object`). Filled once each, by the first successful
    /// `ensure_class_initialized`; a class id is never reused in a VM.
    pub wrapper_cids: [std::sync::atomic::AtomicU32; 8],
    /// `CRATONVM_REFLECT_GET_INTERFACES_LEAN` for this VM (interpreter round
    /// i4 wave 95, lane L3; default off): `Class.getInterfaces0` reads the
    /// mirror's stored name once (it read it twice), builds the debug-only
    /// display name only under `CRATONVM_DBG_BB`, and takes the `Class[]`
    /// component from [`Self::class_component_id`] (the slot the mint-lean
    /// switch fills too: both hold `java/lang/Class`'s id) instead of a
    /// by-name lookup per call.
    pub interfaces_lean: bool,
    /// `CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER` for this VM (interpreter round
    /// i5 wave 101, lane L3; default off): `getDeclaredMethods0` lists every
    /// class's methods in HotSpot's sorted `methods` array order
    /// ([`hotspot_method_array_order_ranked`]), not only an annotation's.
    pub hotspot_method_order: bool,
    /// `CRATONVM_DECLARED_CLASSES_HOTSPOT_ENTRIES` for this VM (interpreter
    /// round i5 wave 101, lane L3; default on, `=0` off):
    /// `getDeclaredClasses0` takes every `InnerClasses` entry
    /// `JVM_GetDeclaredClasses` takes.
    pub declared_classes_entries: bool,
    /// `-Djdk.reflect.useNativeAccessorOnly` as the doors' first refusal read
    /// it (`door_honours_native_accessor_only`; JDK 25 reads it once, in
    /// `ReflectionFactory.config()`). JIT round 21 wave 10, lane walk.
    pub native_accessor_only: std::sync::OnceLock<bool>,
    /// `CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE` for this VM (JIT round 21
    /// wave 13, review rev-native item 5; default on, `=0` off): read once
    /// here instead of on every `lang_reflect::runtime_package_of`.
    pub hidden_runtime_package: bool,
    /// `CRATONVM_LOOKUP_IN_PACKAGE_MEMBER` for this VM (JIT round 21 wave 13,
    /// review rev-native item 5; default on, `=0` off): read once here
    /// instead of on every `Lookup.in` (`classloader::lk_class_relation`).
    pub lookup_in_package_member: bool,
    /// `CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK` for this VM (JIT round 21
    /// wave 14, lane innerclasses; default on, `=0` off):
    /// [`spoofed_member_disagreement`].
    pub inner_classes_spoof_check: bool,
    /// The spoofed-member verdicts of this VM ([`spoofed_member_disagreement`]),
    /// memoized: the pair `(inner << 32) | outer`, or `(inner << 32) |
    /// SPOOF_BY_INNER` for the bridges' declaring-class answer, to the ICCE
    /// text or `None`. JIT round 21 wave 14, lane innerclasses.
    pub spoof_verdicts: SpoofVerdictMemo,
    /// `CRATONVM_REFLECT_HIDDEN_NAME_TEXT` for this VM (JIT round 21 wave 14,
    /// lane hidden-text; default on, `=0` off): reflection message texts name
    /// a hidden class as `Class.getName()` does ([`reflect_text_class_name`]).
    pub hidden_name_text: bool,
    /// `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW` for this VM (JIT round 21
    /// wave 16, lane innerclasses; default on, `=0` off):
    /// [`declaring_member_row_outer`].
    pub declaring_class_any_outer_row: bool,
    /// `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD` for this VM (JIT
    /// round 21 wave 19, lane innerclasses; default on, `=0` off):
    /// [`class_nesting_of`].
    pub nesting_follows_enclosing_method: bool,
    /// `CRATONVM_REFLECT_INNER_TABLES_GENERATION` for this VM (wave 19; default
    /// on, `=0` off): the fills of the answers built from `InnerClasses` /
    /// `EnclosingMethod` check [`Self::inner_tables_generation`].
    pub inner_tables_generation_check: bool,
    /// Bumped by [`forget_redefined_inner_class_answers`] before it clears, so
    /// a fill that read the tables before a redefinition swapped them does not
    /// keep its answer ([`inner_tables_fill_guard`]). Wave 19.
    pub inner_tables_generation: std::sync::atomic::AtomicU64,
    /// `CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY` for this VM (wave 19; default
    /// on, `=0` off): [`class_seen_by_referencing_class_checked`].
    pub outer_resolution_failure_sticky: bool,
    /// The failed `klass_at` resolutions of `InnerClasses` / `EnclosingMethod`
    /// names: `(referencing class, name)` to the error's class and message
    /// (JVMS 5.4.3). Wave 19.
    pub outer_resolution_failures: OuterResolutionFailures,
    /// `CRATONVM_COMPAT_NAME_BRIDGE_VERDICT_MEMO` for this VM (JIT round 23
    /// wave 1, lane L6; REVIEW22-2; perf, default on, `=0` off):
    /// [`name_bridge_verdict_probe`].
    pub name_bridge_verdict_memo: bool,
    /// The `--compatible` `getSimpleName` / `getCanonicalName` bridges' own
    /// verdict per class id: the bits of [`name_bridge_verdict_bit`] for each
    /// (bridge, user-loader state) whose checks once passed. REVIEW22-2.
    pub name_bridge_verdicts: NameBridgeVerdicts,
    /// `CRATONVM_STATIC_RMW_VM_LOCK` for this VM (JIT round 23 wave 2, lane
    /// L6; correctness, default on, `=0` off): [`static_rmw_stripes`].
    pub static_rmw_vm_lock: bool,
    /// The VM-private locks of the static read-modify-write fallback
    /// ([`StaticRmwStripes`]). An `Arc` so a holder never borrows the context.
    pub static_rmw_stripes: Arc<StaticRmwStripes>,
}

/// [`ReflectDoorCaches::name_bridge_verdicts`].
pub type NameBridgeVerdicts = cratonvm_types::lock_order::OrderedPlRwLock<FxHashMap<u32, u8>>;

/// [`ReflectDoorCaches::outer_resolution_failures`].
pub type OuterResolutionFailures = cratonvm_types::lock_order::OrderedPlRwLock<
    FxHashMap<(u32, Box<str>), RecordedResolutionFailure>,
>;

/// One row of [`OuterResolutionFailures`]: what HotSpot's resolution-error
/// table keeps (`ResolutionErrorEntry`: the error's class and message, and its
/// cause's class and message), so the rethrow is a new error of the same class
/// and message caused by a new throwable of the cause's class and message, as
/// `ConstantPool::throw_resolution_error` builds it (wave 21, item 6c).
pub struct RecordedResolutionFailure {
    pub class: Box<str>,
    pub message: Option<Box<str>>,
    pub cause: Option<(Box<str>, Option<Box<str>>)>,
}

/// This VM's [`ReflectDoorCaches`], made on first use; `None` for a context
/// with no slot (mock contexts).
pub fn reflect_door_caches(ctx: &dyn NativeContext) -> Option<&ReflectDoorCaches> {
    let slot = ctx.reflect_door_cache_slot()?;
    let boxed = slot.get_or_init(new_reflect_door_caches);
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<ReflectDoorCaches>()
}

/// A fresh [`ReflectDoorCaches`] for a VM's slot (split out of
/// [`reflect_door_caches`] by JIT round 21 wave 21, so the unload-prune test
/// can fill a slot without a VM), boxed for the slot; the caches themselves
/// come from [`fresh_reflect_door_caches`].
pub fn new_reflect_door_caches() -> Box<dyn std::any::Any + Send + Sync> {
    Box::new(fresh_reflect_door_caches())
}

/// Stripe count of [`StaticRmwStripes`].
pub const STATIC_RMW_STRIPES: usize = 64;

/// JIT round 23 wave 2, lane L6 (page
/// `r23-w2-review2-unsafe-static-rmw-takes-class-monitor-FIXED-20261021.md`): the
/// VM-private locks the static read-modify-write fallback holds
/// (`lang_invoke::vh_with_static_lock`, which the static `VarHandle` RMWs and
/// `unsafe_natives_ext::unsafe_static_rmw` / `synthetic_cas` share), in place
/// of the class mirror's Java monitor. HotSpot's `Unsafe` / `VarHandle` static
/// RMWs are one locked instruction: they never block on, own, or show up as
/// `Foo.class`'s monitor (`synchronized (Foo.class)`, `static synchronized`,
/// thread dumps, `Object.wait` ownership). A stripe per class id, per VM (hung
/// off this VM's door caches, never a process global).
///
/// Plain atomic flags, not a `Mutex`: the holder runs a few static-slot reads
/// and writes and never reaches a GC point; a waiter spins briefly and then
/// waits inside a blocking region, so a collection never waits for it.
pub struct StaticRmwStripes(pub [std::sync::atomic::AtomicBool; STATIC_RMW_STRIPES]);

impl StaticRmwStripes {
    pub fn new() -> Self {
        Self(std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)))
    }

    pub fn stripe_of(cid: ClassId) -> usize {
        // Fibonacci hashing, top bits (see `gc::collector::volatile_stripe_index`).
        ((u64::from(cid.as_u32()).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize)
            & (STATIC_RMW_STRIPES - 1)
    }

    pub fn try_take(&self, stripe: usize) -> bool {
        self.0[stripe]
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }
}

/// A held stripe of [`StaticRmwStripes`]; dropping it releases the stripe.
pub struct StaticRmwGuard {
    pub stripes: Arc<StaticRmwStripes>,
    pub stripe: usize,
}

impl Drop for StaticRmwGuard {
    fn drop(&mut self) {
        self.stripes.0[self.stripe].store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Take `cid`'s stripe of this VM's [`StaticRmwStripes`]; `None` when the
/// context has no cache slot (mock contexts) or `CRATONVM_STATIC_RMW_VM_LOCK=0`:
/// the caller keeps the class-monitor path. Spins a little, then waits inside
/// a blocking region (GC-safe), yielding the CPU between attempts.
pub fn static_rmw_lock(
    ctx: &mut dyn NativeContext,
    cid: ClassId,
) -> Option<StaticRmwGuard> {
    let stripes = {
        let caches = reflect_door_caches(&*ctx)?;
        if !caches.static_rmw_vm_lock {
            return None;
        }
        Arc::clone(&caches.static_rmw_stripes)
    };
    Some(static_rmw_take(ctx, stripes, StaticRmwStripes::stripe_of(cid)))
}

pub fn static_rmw_take(
    ctx: &mut dyn NativeContext,
    stripes: Arc<StaticRmwStripes>,
    stripe: usize,
) -> StaticRmwGuard {
    let mut spins = 0u32;
    while !stripes.try_take(stripe) {
        if spins < 256 {
            spins += 1;
            std::hint::spin_loop();
            continue;
        }
        // A VM-internal wait (HotSpot's RMW is one locked instruction): the
        // thread stays RUNNABLE and a pause need not wait for it.
        ctx.begin_vm_wait_region();
        while !stripes.try_take(stripe) {
            std::thread::yield_now();
        }
        ctx.end_blocking_region();
        break;
    }
    StaticRmwGuard { stripes, stripe }
}

/// `CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE` for this VM, read once
/// ([`ReflectDoorCaches::hidden_runtime_package`]); a context with no cache
/// slot (mock contexts) reads the flag table. JIT round 21 wave 13, review
/// rev-native item 5.
#[inline]
pub fn reflect_hidden_runtime_package_per_vm(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.hidden_runtime_package,
        None => {
            crate::nbflags().mh_hidden_class_runtime_package
        }
    }
}

/// `CRATONVM_REFLECT_HIDDEN_NAME_TEXT` for this VM, read once
/// ([`ReflectDoorCaches::hidden_name_text`]); a context with no cache slot
/// (mock contexts) reads the flag table.
#[inline]
pub fn reflect_hidden_name_text_per_vm(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.hidden_name_text,
        None => true,
    }
}

/// An internal class name as a reflection message text names it: the JDK
/// builds those texts from `Class.getName()` (`FieldAccessorImpl
/// .getQualifiedFieldName`, `getSetMessage`'s `o.getClass().getName()`,
/// `Class.methodToString`), which keeps a hidden class's `/0x<hex>` tail
/// verbatim ([`dotted_binary_name`]). The plain `/`->`.` rewrite printed
/// `R21Fz3HiddenAccess1Payload.0x0000000000000002.TAG` where HotSpot 25 prints
/// `R21Fz3HiddenAccess1Payload/0x0000020001040c00.TAG` (JIT round 21 wave 14,
/// lane hidden-text; probe `R21Fz3HiddenAccess1`, fact
/// `outer.setAccessibleFinal`). `CRATONVM_REFLECT_HIDDEN_NAME_TEXT=0` restores
/// the plain rewrite.
pub fn reflect_text_class_name(ctx: &dyn NativeContext, internal: &str) -> String {
    if reflect_hidden_name_text_per_vm(ctx) {
        dotted_binary_name(internal)
    } else {
        internal.replace('/', ".")
    }
}

/// [`reflect_text_class_name`] of the class `class_id` names (JIT round 21
/// wave 25, lane hexname; `CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS`): the
/// `/0x<hex>` tail kept only when the (bottom) class is hidden
/// ([`dotted_binary_name_of_class`]), as `Class.getName()` spells it.
pub fn reflect_text_class_name_of_class(
    ctx: &dyn NativeContext,
    class_id: Option<ClassId>,
    internal: &str,
) -> String {
    if reflect_hidden_name_text_per_vm(ctx) {
        dotted_binary_name_of_class(ctx, class_id, internal)
    } else {
        internal.replace('/', ".")
    }
}

/// Method-chain spellings of the class-name renderings for message texts
/// (JIT round 21 wave 14, lane hidden-text): a drop-in for the plain
/// `.replace('/', ".")` at a site that names a class (an internal name or an
/// array descriptor) the way the JDK's `Class.getName()` /
/// `Klass::external_name()` does. Both keep a hidden class's `/0x<hex>` tail
/// (shape-judged by [`hidden_name_suffix_split`], so any other name renders
/// exactly as before).
pub trait ClassTextName {
    /// [`reflect_text_class_name`]: honours `CRATONVM_REFLECT_HIDDEN_NAME_TEXT`.
    fn class_text(&self, ctx: &dyn NativeContext) -> String;
    /// [`dotted_binary_name`], for a pure renderer with no context to read the
    /// switch from.
    fn hidden_dotted(&self) -> String;
}

impl ClassTextName for str {
    fn class_text(&self, ctx: &dyn NativeContext) -> String {
        reflect_text_class_name(ctx, self)
    }

    fn hidden_dotted(&self) -> String {
        dotted_binary_name(self)
    }
}

/// `CRATONVM_LOOKUP_IN_PACKAGE_MEMBER` for this VM, read once
/// ([`ReflectDoorCaches::lookup_in_package_member`]); a context with no cache
/// slot (mock contexts) reads the flag table. JIT round 21 wave 13, review
/// rev-native item 5.
#[inline]
pub fn lookup_in_package_member_per_vm(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.lookup_in_package_member,
        None => true,
    }
}

/// A fresh [`ReflectDoorCaches`], its switches read now (zt t9/cl5: split out
/// of [`reflect_door_caches`] so a test can fill a slot without a context;
/// dev merge: the one field list, also behind [`new_reflect_door_caches`]).
pub fn fresh_reflect_door_caches() -> ReflectDoorCaches {
    ReflectDoorCaches {
        method_plans: MethodPlans::new(),
        field_idents: FieldIdentMemo::new(),
        ctor_plans: CtorPlans::new(),
        phase_stats: crate::reflect_door_phases::DoorPhaseStats::new(),
        class_names: cratonvm_types::lock_order::OrderedPlRwLock::new(
            FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        ),
        class_names_idless: cratonvm_types::lock_order::OrderedPlRwLock::new(
            FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        ),
        ctor_class_id: std::sync::atomic::AtomicU32::new(0),
        ctor_mirror_memo: true,
        ctor_lazy_desc: false,
        ctor_mint_lean: false,
        class_component_id: std::sync::atomic::AtomicU32::new(0),
        ctor_component_id: std::sync::atomic::AtomicU32::new(0),
        wrapper_cid_per_vm: true,
        wrapper_cids: std::array::from_fn(|_| std::sync::atomic::AtomicU32::new(0)),
        interfaces_lean: false,
        // Default ON since interpreter round i5 w102: every JDK class's
        // getDeclaredMethods order is HotSpot's on w102 (L3W101DeclaredMethodOrder);
        // app classes approximate (HotSpot's own order for new symbols varies).
        // `=0` restores class-file order.
        hotspot_method_order: true,
        declared_classes_entries: true,
        native_accessor_only: std::sync::OnceLock::new(),
        // Interpreter round i7 wave 136, lane L3: the second kill switch
        // (`CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE`, i121-L4) folded in,
        // so `runtime_package_of` reads one per-VM bool.
        hidden_runtime_package: crate::nbflags().mh_hidden_class_runtime_package,
        lookup_in_package_member: true,
        inner_classes_spoof_check: true,
        spoof_verdicts: cratonvm_types::lock_order::OrderedPlRwLock::new(
            FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        ),
        hidden_name_text: true,
        declaring_class_any_outer_row: true,
        nesting_follows_enclosing_method: true,
        inner_tables_generation_check: true,
        inner_tables_generation: std::sync::atomic::AtomicU64::new(0),
        outer_resolution_failure_sticky: true,
        outer_resolution_failures: cratonvm_types::lock_order::OrderedPlRwLock::new(
            FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        ),
        name_bridge_verdict_memo: true,
        name_bridge_verdicts: cratonvm_types::lock_order::OrderedPlRwLock::new(
            FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        ),
        static_rmw_vm_lock: true,
        static_rmw_stripes: Arc::new(StaticRmwStripes::new()),
    }
}

/// zt t9/cl5 (`zgc-zt8cl4-dead-loader-classes-hold-143-bytes-each-FIXED-20261019.md`):
/// take this VM's `Class.getName()` memo rows ([`ClassGetNameMemo`]) of the
/// classes the unload transaction just removed, and answer their global-root
/// handles for the caller to release (`vm::memory::gc::unload_dead_class_metadata`
/// removes them from the JNI global-ref table).
///
/// Each row is a global root of an `Object[2]`: the mirror's `name` slot (the
/// internal-name String) and the interned binary name. Nothing removed a row
/// when its class unloaded, so every class whose `getName()` ever ran kept one
/// `Object[]` and two Strings for the life of the VM: the JDK's own
/// `ClassLoader.postDefineClass` calls `c.getPackageName()`, which calls
/// `getName()`, so that is every class `defineClass` defines. Measured on the
/// t8b census (`Zt7ClClassNames 40 1000 3 histo`, ZGC): 40002 classes
/// unloaded, yet about 2 Strings with their `byte[]` and 1 `Object[]` per class
/// still live (143 bytes per define; HotSpot 0). Class ids are never reused,
/// so a removed row could never be read again: this is a leak, not a stale
/// answer. Empty when the caches were never made. Takes only the memo's
/// write lock (`Scratch`, nothing acquired under it).
pub fn take_unloaded_class_name_memo_rows(
    slot: &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    class_ids: &[u32],
) -> Vec<usize> {
    if class_ids.is_empty() {
        return Vec::new();
    }
    let Some(boxed) = slot.get() else {
        return Vec::new();
    };
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    let Some(caches) = any.downcast_ref::<ReflectDoorCaches>() else {
        return Vec::new();
    };
    let mut memo = caches.class_names.write();
    if memo.is_empty() {
        return Vec::new();
    }
    class_ids.iter().filter_map(|id| memo.remove(id)).collect()
}


/// This VM's wrapper-class id slots (`CRATONVM_BOX_WRAPPER_CID_PER_VM`,
/// interpreter round i4 wave 92, lane L3), or `None` with the switch off or
/// for a context with no cache slot (mock contexts), in which case
/// `lang_math::alloc_wrapper` takes its process-table path as before.
#[inline]
pub fn per_vm_wrapper_cids(
    ctx: &dyn NativeContext,
) -> Option<&[std::sync::atomic::AtomicU32; 8]> {
    reflect_door_caches(ctx)
        .filter(|c| c.wrapper_cid_per_vm)
        .map(|c| &c.wrapper_cids)
}

/// A `java/` component class's id for a `Constructor` mint: by name, as
/// before; with `CRATONVM_REFLECT_CTOR_MINT_LEAN` on, the first non-zero
/// answer is kept in `slot` of this VM's reflect-door caches. Stable once
/// answered: a boot class's id never changes in a VM (a `--compatible` stub
/// upgrade keeps its id), and ids are never reused.
pub fn lean_component_id(
    ctx: &mut dyn NativeContext,
    name: &str,
    slot: fn(&ReflectDoorCaches) -> &std::sync::atomic::AtomicU32,
) -> ClassId {
    use std::sync::atomic::Ordering;
    let lean = reflect_door_caches(&*ctx).is_some_and(|c| c.ctor_mint_lean);
    if lean {
        let cached = reflect_door_caches(&*ctx)
            .map(|c| slot(c).load(Ordering::Acquire))
            .unwrap_or(0);
        if cached != 0 {
            return ClassId::new(cached);
        }
    }
    // zs s3/nd3: a loading lookup for `Constructor` (`java/lang/Class`, the
    // other user, is always loaded: no load, no collection).
    let id = listing_component_id(ctx, name);
    if lean && id.as_u32() != 0 {
        if let Some(c) = reflect_door_caches(&*ctx) {
            slot(c).store(id.as_u32(), Ordering::Release);
        }
    }
    id
}

/// `CRATONVM_REFLECT_CTOR_MINT_LEAN` for this VM (default off; `false` for a
/// context with no cache slot).
pub fn reflect_ctor_mint_lean_on(ctx: &dyn NativeContext) -> bool {
    reflect_door_caches(ctx).is_some_and(|c| c.ctor_mint_lean)
}

/// `CRATONVM_REFLECT_GET_INTERFACES_LEAN` for this VM (interpreter round i4
/// wave 95, lane L3; default off; `false` for a context with no cache slot).
pub fn reflect_get_interfaces_lean_on(ctx: &dyn NativeContext) -> bool {
    reflect_door_caches(ctx).is_some_and(|c| c.interfaces_lean)
}

/// `CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER` for this VM (interpreter round i5
/// wave 101, lane L3; default off; `false` for a context with no cache slot).
pub fn reflect_hotspot_method_order_on(ctx: &dyn NativeContext) -> bool {
    reflect_door_caches(ctx).is_some_and(|c| c.hotspot_method_order)
}

/// `java/lang/Class`'s `ClassId` as the `Class[]` component of
/// `getInterfaces0` under `CRATONVM_REFLECT_GET_INTERFACES_LEAN`: the per-VM
/// slot once a by-name lookup answered it (a boot class's id never changes in
/// a VM and ids are never reused), the by-name lookup otherwise.
pub fn interfaces_class_component_id(ctx: &mut dyn NativeContext) -> ClassId {
    use std::sync::atomic::Ordering;
    let cached = reflect_door_caches(&*ctx)
        .map(|c| c.class_component_id.load(Ordering::Acquire))
        .unwrap_or(0);
    if cached != 0 {
        return ClassId::new(cached);
    }
    let id = class_component_id(ctx);
    if id.as_u32() != 0 {
        if let Some(c) = reflect_door_caches(&*ctx) {
            c.class_component_id.store(id.as_u32(), Ordering::Release);
        }
    }
    id
}

/// [`lean_component_id`]'s slot for `java/lang/Class`.
pub fn class_component_slot(c: &ReflectDoorCaches) -> &std::sync::atomic::AtomicU32 {
    &c.class_component_id
}

/// [`lean_component_id`]'s slot for `java/lang/reflect/Constructor`.
pub fn ctor_component_slot(c: &ReflectDoorCaches) -> &std::sync::atomic::AtomicU32 {
    &c.ctor_component_id
}

/// Fold a finished door call's phase marks into this VM's census
/// (`CRATONVM_REFLECT_DOOR_PHASE_STATS`; interpreter round i3 wave 77, lane
/// L3). A timer that is off, or a context with no cache slot, records
/// nothing.
pub fn record_door_phases(
    ctx: &dyn NativeContext,
    kind: usize,
    timer: &crate::reflect_door_phases::PhaseTimer,
) {
    if !timer.is_on() {
        return;
    }
    if let Some(caches) = reflect_door_caches(ctx) {
        let _ = caches.phase_stats.record(kind, timer);
    }
}

/// This VM's [`MethodPlans`]; `None` for a context with no cache slot.
pub fn method_plans(ctx: &dyn NativeContext) -> Option<&MethodPlans> {
    reflect_door_caches(ctx).map(|caches| &caches.method_plans)
}

/// The `slot` value a `Method` mint of `meta` carries: its plan's id under
/// `CRATONVM_REFLECT_INVOKE_PLAN`, else `0` (as before).
pub fn method_plan_slot(ctx: &dyn NativeContext, meta: &MethodMetadata) -> i32 {
    if !ctx.reflect_invoke_plan() {
        return 0;
    }
    let Some(plans) = method_plans(ctx) else {
        return 0;
    };
    let Some(class_name) = ctx.class_name_arc_of_id(meta.declaring_class_id) else {
        return 0;
    };
    // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): the class's
    // flags, read before the table lock (as `CtorPlans::intern` does).
    let class_flags = ctx
        .reflect_invoke_plan_facts()
        .then(|| ctx.class_access_flags(meta.declaring_class_id));
    plans.intern_with_flags(
        meta.declaring_class_id,
        class_name,
        &meta.name,
        &meta.descriptor,
        i32::from(meta.access_flags),
        class_flags,
    )
}

/// The plan `method`'s `slot` names, when the switch is on and it names one.
pub fn method_plan_of(ctx: &dyn NativeContext, method: ObjectRef) -> Option<Arc<MethodPlan>> {
    let Value::Int(id) = ctx.get_field_by_name(method, "slot") else {
        return None;
    };
    if id <= 0 {
        return None;
    }
    method_plans(ctx)?.get(id)
}

