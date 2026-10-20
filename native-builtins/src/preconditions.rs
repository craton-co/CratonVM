//! `jdk/internal/util/Preconditions` — bounds checks that honour their
//! exception formatter.
//!
//! # What the JDK's contract actually is
//!
//! Every `Preconditions.check*` overload that takes a fourth argument takes a
//! `BiFunction<String, List<Number>, ? extends RuntimeException>` — the
//! *exception formatter*. It is the entire reason the overload exists. The JDK
//! ships three (`Preconditions.<clinit>` builds them from anonymous inner
//! classes, deliberately not lambdas, so the class is usable during bootstrap):
//!
//! | formatter          | exception class                   | who passes it |
//! |--------------------|-----------------------------------|---------------|
//! | `SIOOBE_FORMATTER` | `StringIndexOutOfBoundsException` | every `java.lang.String` bounds helper |
//! | `AIOOBE_FORMATTER` | `ArrayIndexOutOfBoundsException`  | array-domain callers |
//! | `IOOBE_FORMATTER`  | `IndexOutOfBoundsException`       | `java.nio` buffers |
//!
//! and a `null` formatter — which is what all six `java.util.Objects.check*`
//! methods pass — means `IndexOutOfBoundsException`, per
//! `Preconditions.outOfBounds`:
//!
//! ```text
//! RuntimeException e = oobef == null ? null : oobef.apply(checkKind, largs);
//! return e == null ? new IndexOutOfBoundsException(outOfBoundsMessage(checkKind, largs)) : e;
//! ```
//!
//! The override this module replaces discarded the formatter and threw
//! `ArrayIndexOutOfBoundsException` unconditionally. Both halves of that were
//! wrong, and both are control-flow bugs rather than message bugs:
//! `StringIndexOutOfBoundsException` and `ArrayIndexOutOfBoundsException` are
//! *siblings*, so `catch (StringIndexOutOfBoundsException)` — which real
//! parsing code writes — did not catch a `String.substring` failure; and
//! `ArrayIndexOutOfBoundsException` is a *subclass* of the
//! `IndexOutOfBoundsException` an `Objects.check*`/NIO caller is promised, so
//! `catch (IndexOutOfBoundsException)` still worked but nothing narrower did.
//! See
//! `preconditions-ignores-the-exception-formatter-FIXED-20260805.md`.
//!
//! # Why these are natives at all
//!
//! The *success* path. `String.charAt` → `StringLatin1.charAt` →
//! `String.checkIndex` → `Preconditions.checkIndex(index, length, SIOOBE_FORMATTER)`
//! is the JDK 25 call chain, so in principle this native carries every single
//! character read. The natives keep the in-bounds arithmetic in Rust and
//! allocate nothing, and only the (cold) out-of-bounds path builds an
//! exception.
//!
//! ## The per-character claim is FALSE of the shipping binary — MEASURED
//!
//! It used to say, without a denominator, that a Java `Preconditions` frame
//! was paid "on every single character read", and that sentence is why this
//! module was treated as hot. It is not. Measured 2026-08-17 on the release
//! binary built from `d2e127930`, HotSpot 25.0.3+9-LTS as the oracle
//! (`docs/internal/jdk-only/G20-1-the-first-performance-profile-of-this-branch-20260817.md`
//! §5), over a **1,000,000-iteration** `String.charAt` loop:
//!
//! | instrument | reading |
//! |---|---|
//! | `--dump-native-registry`, `--nojit` arm | `checkIndex(IIL..BiFunction;)I` **invocations = 1** — not 1,000,000. Whole-process native dispatches: **1,540** |
//! | wall time, JIT arm, median of 5 | **5 ns/call**, against HotSpot's 6 ns |
//! | the native-call boundary on the same host | **~141 ns/call** (`System.identityHashCode`, 167 ns, less a 26 ns interpreted-call control) |
//!
//! 5 ns is a factor of 28 *below* the cost of crossing into Rust once, so on
//! the arm that ships `charAt` provably does not enter this module per
//! character. Two independent instruments agree. Do **not** spend optimisation
//! effort here on the strength of the sentence above it; the three success
//! paths are already branch-and-return and allocate nothing, and the measured
//! hot natives on this branch are elsewhere (record §4, §6).
//!
//! This does not argue for deleting the module: its reason to exist is
//! *correctness* — honouring the exception formatter, which is a control-flow
//! contract three families of caller depend on — and that reason is unaffected
//! by how often the success path runs.
//!
//! Only the `int` overloads are registered. The `long` ones
//! (`MemorySegment`/`ByteBuffer` scale checks) are not on a per-character path,
//! their bytecode is correct once the formatters exist, and leaving them there
//! keeps this module's blast radius to exactly the methods that need it.
//!
//! # How the formatter is honoured
//!
//! [`throw_out_of_bounds`] resolves the exception class in three steps, cheapest
//! first:
//!
//! 1. `oobef == null` → `IndexOutOfBoundsException`, the JDK's own fallback.
//! 2. `oobef` is reference-identical to one of `Preconditions`' three static
//!    formatters → that formatter's class, built directly. This covers every
//!    caller inside `java.base` without running any Java code on the throw
//!    path.
//! 3. anything else (an application-supplied formatter) → actually invoke
//!    `oobef.apply(checkKind, args)` and throw whatever object it returns,
//!    exactly as `Preconditions.outOfBounds` does. A `null` return falls back
//!    to step 1, again per the JDK.
//!
//! Step 2 needs `Preconditions.<clinit>` to have run, which is why this module
//! does **not** register a no-op over it. The historical reason for suppressing
//! it — "the formatters are built with invokedynamic + LambdaMetafactory, and
//! this class initializes from `String.charAt` before `java.lang.invoke` is
//! usable" — has not been true since the JDK rewrote them as anonymous inner
//! classes for that very reason. `javap -c jdk.internal.util.Preconditions`
//! on JDK 25 shows three `new`/`invokespecial`/`invokestatic` triples and no
//! `invokedynamic` anywhere.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{
    out_of_bounds_message, MethodCallFailed, MethodCallResult, RuntimeError,
};
use cratonvm_types::{ObjectRef, Value};

/// Which `Preconditions` check failed. Selects both the `checkKind` string the
/// formatter is handed and the message template, mirroring
/// `Preconditions.outOfBoundsMessage`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CheckKind {
    /// `checkIndex(index, length)` — two arguments.
    Index,
    /// `checkFromToIndex(fromIndex, toIndex, length)` — three arguments.
    FromToIndex,
    /// `checkFromIndexSize(fromIndex, size, length)` — three arguments.
    FromIndexSize,
}

impl CheckKind {
    /// The `checkKind` string `Preconditions` passes to the formatter. These
    /// are matched by value inside `outOfBoundsMessage`, so they are part of
    /// the contract, not decoration.
    pub(crate) fn name(self) -> &'static str {
        match self {
            CheckKind::Index => "checkIndex",
            CheckKind::FromToIndex => "checkFromToIndex",
            CheckKind::FromIndexSize => "checkFromIndexSize",
        }
    }

    /// How many `Number` arguments the formatter receives.
    fn arity(self) -> usize {
        match self {
            CheckKind::Index => 2,
            CheckKind::FromToIndex | CheckKind::FromIndexSize => 3,
        }
    }

    /// `Preconditions.outOfBoundsMessage(checkKind, args)`.
    ///
    /// The three in-range shapes come from
    /// [`cratonvm_types::error::out_of_bounds_message`], which is also what
    /// `RuntimeError::sioobe_*` and the `java.nio` buffer natives format
    /// through — one copy of the wording for all three families of caller.
    pub(crate) fn message(self, args: &[i64]) -> String {
        // `outOfBoundsMessage` falls through to its default arm when the
        // argument count does not match the check kind. Reproduce that rather
        // than indexing past the end.
        if args.len() != self.arity() {
            let rendered: Vec<String> = args.iter().map(|value| value.to_string()).collect();
            return format!(
                "Range check failed: {} [{}]",
                self.name(),
                rendered.join(", ")
            );
        }
        match self {
            CheckKind::Index => out_of_bounds_message::check_index(args[0], args[1]),
            CheckKind::FromToIndex => {
                out_of_bounds_message::check_from_to_index(args[0], args[1], args[2])
            }
            CheckKind::FromIndexSize => {
                out_of_bounds_message::check_from_index_size(args[0], args[1], args[2])
            }
        }
    }
}

/// The three `Preconditions` static formatters, in field-declaration order,
/// paired with the exception class each one produces.
const KNOWN_FORMATTERS: [(&str, &str); 3] = [
    (
        "SIOOBE_FORMATTER",
        "java/lang/StringIndexOutOfBoundsException",
    ),
    (
        "AIOOBE_FORMATTER",
        "java/lang/ArrayIndexOutOfBoundsException",
    ),
    ("IOOBE_FORMATTER", "java/lang/IndexOutOfBoundsException"),
];

/// The class `Preconditions.outOfBounds` produces for a `null` formatter.
const FALLBACK_EXCEPTION: &str = "java/lang/IndexOutOfBoundsException";

/// Read an argument as an object reference, treating anything else (including
/// the `Value::Int(0)` an unwritten reference slot can read back as) as null.
fn object_arg(args: &[Value], index: usize) -> Option<ObjectRef> {
    match args.get(index) {
        Some(Value::Object(inner)) => *inner,
        _ => None,
    }
}

/// Read an argument as an `int`, defaulting to 0 — the shape the rest of this
/// crate's bounds natives already use.
fn int_arg(args: &[Value], index: usize) -> i32 {
    match args.get(index) {
        Some(Value::Int(value)) => *value,
        _ => 0,
    }
}

/// If `formatter` is one of `Preconditions`' three static formatters, return
/// the exception class it builds.
///
/// Identity, not class name: all three are instances of the *same* anonymous
/// class (the one `outOfBoundsExceptionFormatter` returns), so only the static
/// field they came from tells them apart.
fn known_formatter_class(
    ctx: &mut dyn NativeContext,
    formatter: ObjectRef,
) -> Option<&'static str> {
    let class_id = ctx.class_id_by_name("jdk/internal/util/Preconditions")?;
    for (field, exception_class) in KNOWN_FORMATTERS {
        let Some(index) = ctx.static_field_index_by_name(class_id, field) else {
            continue;
        };
        if let Value::Object(Some(candidate)) = ctx.get_static_field(class_id, index) {
            if candidate == formatter {
                return Some(exception_class);
            }
        }
    }
    None
}

/// Build `exception_class(message)` and hand it back as a thrown Java
/// exception.
///
/// Falls back to a `RuntimeError` of the same class if the object cannot be
/// constructed (a stripped or synthetic class library): a missing message is
/// survivable, a wrong class is the whole bug.
fn throw_constructed(
    ctx: &mut dyn NativeContext,
    exception_class: &str,
    message: String,
    fallback_index: i64,
) -> MethodCallFailed {
    let detail = ctx.create_string(&message);
    if let Ok(Some(Value::Object(Some(exception)))) = ctx.new_object_initialized(
        exception_class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exception);
    }
    let index = i32::try_from(fallback_index).unwrap_or(i32::MAX);
    match exception_class {
        "java/lang/StringIndexOutOfBoundsException" => {
            RuntimeError::StringIndexOutOfBoundsException {
                index,
                message: Some(message),
            }
            .into()
        }
        // The formatter's own text, as for the other two classes (wave 100
        // L7: this fell back to "Array index out of range: n").
        "java/lang/ArrayIndexOutOfBoundsException" => {
            RuntimeError::aioobe_with_message(index, message).into()
        }
        _ => RuntimeError::ioobe(message).into(),
    }
}

/// Ask an application-supplied formatter for the exception, exactly as
/// `Preconditions.outOfBounds` does: `oobef.apply(checkKind, List.of(args))`.
///
/// Returns `None` if the formatter cannot be called or answers `null`; the
/// caller then takes the JDK's `IndexOutOfBoundsException` fallback.
///
/// Every reference held across a re-entrant call is pinned. `base` covers the
/// whole batch, so the single `unpin_native_roots(base)` in the wrapper
/// releases the nested pins too.
fn apply_custom_formatter(
    ctx: &mut dyn NativeContext,
    formatter: ObjectRef,
    kind: CheckKind,
    args: &[i64],
) -> Option<ObjectRef> {
    let base = ctx.pin_native_root(formatter);
    let result = apply_custom_formatter_pinned(ctx, formatter, base, kind, args);
    ctx.unpin_native_roots(base);
    result
}

fn apply_custom_formatter_pinned(
    ctx: &mut dyn NativeContext,
    formatter: ObjectRef,
    base: usize,
    kind: CheckKind,
    args: &[i64],
) -> Option<ObjectRef> {
    let object_class = ctx.class_id_by_name("java/lang/Object")?;
    let boxed = ctx.try_new_ref_array(object_class, args.len())?;
    let boxed_pin = ctx.pin_native_root(boxed);
    for (slot, value) in args.iter().enumerate() {
        // `Integer.valueOf` rather than a hand-built wrapper: the box has to be
        // a real `Number` for `outOfBoundsMessage`'s `String.format("%s", ..)`.
        let boxed_value = ctx
            .invoke(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[Value::Int(i32::try_from(*value).unwrap_or(i32::MAX))],
            )
            .ok()??;
        let array = ctx.read_native_pin(boxed_pin, boxed);
        ctx.set_array_element(array, slot, boxed_value);
    }
    let array = ctx.read_native_pin(boxed_pin, boxed);
    // `Arrays.asList`, not `List.of`: the latter is a static *interface*
    // method, and the throw path must not depend on that resolving.
    let list = match ctx.invoke(
        "java/util/Arrays",
        "asList",
        "([Ljava/lang/Object;)Ljava/util/List;",
        &[Value::Object(Some(array))],
    ) {
        Ok(Some(Value::Object(Some(list)))) => list,
        _ => return None,
    };
    let list_pin = ctx.pin_native_root(list);
    let check_kind = ctx.create_string(kind.name());
    let list = ctx.read_native_pin(list_pin, list);
    let receiver = ctx.read_native_pin(base, formatter);
    let applied = ctx.invoke_virtual(
        receiver,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(check_kind)), Value::Object(Some(list))],
    );
    match applied {
        Ok(Some(Value::Object(Some(exception)))) => Some(exception),
        _ => None,
    }
}

/// `Preconditions.outOfBounds(oobef, checkKind, args...)` — build the
/// exception the formatter asks for and raise it.
///
/// `args` are the check's own arguments in declaration order, widened to `i64`
/// so message formatting is shared with the `long` overloads' templates.
pub(crate) fn throw_out_of_bounds(
    ctx: &mut dyn NativeContext,
    kind: CheckKind,
    args: &[i64],
    formatter: Option<ObjectRef>,
) -> MethodCallFailed {
    let message = kind.message(args);
    let fallback_index = args.first().copied().unwrap_or(0);
    let Some(formatter) = formatter else {
        return throw_constructed(ctx, FALLBACK_EXCEPTION, message, fallback_index);
    };
    if let Some(exception_class) = known_formatter_class(ctx, formatter) {
        return throw_constructed(ctx, exception_class, message, fallback_index);
    }
    if let Some(exception) = apply_custom_formatter(ctx, formatter, kind, args) {
        return MethodCallFailed::ExceptionThrown(exception);
    }
    // `outOfBounds` treats a formatter that returns null as no formatter.
    throw_constructed(ctx, FALLBACK_EXCEPTION, message, fallback_index)
}

// ---------------------------------------------------------------------------
// The raise, through the JDK's own bytecode
// ---------------------------------------------------------------------------

/// `CRATONVM_BOUNDS_RAISE_BYTECODE` (JIT round 16 wave 8, lane trace; a
/// correctness fix, default ON, `0` is the kill switch): a bounds check this
/// crate serves raises its exception by running the real JDK body of the
/// check ([`raise_through_bytecode`]) instead of building the exception in the
/// native's caller frame.
///
/// HotSpot's `StringIndexOutOfBoundsException` from `"abc".charAt(9)` lists
/// ten JDK frames, from `Preconditions$1.apply` down to `String.charAt`; a
/// native has no frame of its own, so an exception it builds lists none of
/// the frames the JDK body would have run
/// (`docs/internal/fixed-bugs/r16-throwpath-cached-charat-sioobe-has-no-jdk-frames-FIXED-20261001.md`).
/// The fast path is untouched: only a call that is about to throw runs Java.
/// Read once (a throw-path flag, cached like the other native-builtins gates).
pub(crate) fn bounds_raise_bytecode_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BOUNDS_RAISE_BYTECODE")
    })
}

/// Run `class_name.method_name descriptor`'s own bytecode (never a native
/// registered for it: `invoke_special_bytecode_only`) with `args`, for a call
/// the caller has ALREADY decided raises, and answer the exception it threw.
///
/// The exception is built inside the JDK frames that raise it, so its stack
/// trace is HotSpot's; the native's caller frame is below them as it would be
/// below the bytecode. `None` -- the caller then raises as it did before --
/// when the switch is off, when the body did not throw (it returned, or failed
/// inside the VM: an early-boot class that cannot run yet), or when what it
/// threw is not an instance of `expected` (`None` accepts any throwable: an
/// application formatter may answer any exception). Fail-closed: the worst
/// case is the old trace, never a different exception class.
///
/// Reentrancy: the bodies routed here (`String.charAt`, the three
/// `Preconditions` checks) reach no native that routes back to the same
/// body, so the nesting is bounded (`String.charAt` -> its `Preconditions`
/// check -> that check's own body: two levels).
pub(crate) fn raise_through_bytecode(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    expected: Option<&str>,
) -> Option<MethodCallFailed> {
    if !bounds_raise_bytecode_enabled() {
        return None;
    }
    let result = ctx.invoke_special_bytecode_only(class_name, method_name, descriptor, args);
    accept_bytecode_raise(ctx, result, expected)
}

/// [`raise_through_bytecode`]'s screen on the body's `result`.
pub(crate) fn accept_bytecode_raise(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    expected: Option<&str>,
) -> Option<MethodCallFailed> {
    let Err(MethodCallFailed::ExceptionThrown(exception)) = result else {
        return None;
    };
    if let Some(expected) = expected {
        let expected_id = ctx.class_id_by_name(expected)?;
        let actual = ctx.class_id_of_object(exception);
        if actual != expected_id && !ctx.is_subclass(actual, expected_id) {
            return None;
        }
    }
    Some(MethodCallFailed::ExceptionThrown(exception))
}

/// The `(int..., BiFunction)` descriptor of `kind`'s JDK method: the shape
/// that HAS a JDK body. The bare `(II)I` / `(III)I` spellings this module
/// also registers have none (see [`register`]).
fn formatter_descriptor(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Index => "(IILjava/util/function/BiFunction;)I",
        CheckKind::FromToIndex | CheckKind::FromIndexSize => {
            "(IIILjava/util/function/BiFunction;)I"
        }
    }
}

/// The exception class `Preconditions.outOfBounds` produces for `formatter`
/// when it is `null` or one of the three static formatters; `None` for an
/// application formatter, which may produce anything.
fn expected_exception_class(
    ctx: &mut dyn NativeContext,
    formatter: Option<ObjectRef>,
) -> Option<&'static str> {
    match formatter {
        None => Some(FALLBACK_EXCEPTION),
        Some(formatter) => known_formatter_class(ctx, formatter),
    }
}

/// Raise a failed check: through the JDK body of the check when this native
/// was called as the four-argument JDK method ([`raise_through_bytecode`]),
/// else (the bare spellings, the switch off, a body that could not raise) as
/// [`throw_out_of_bounds`] always did. `args` are the native's own arguments,
/// `checked` the check's `int`s widened.
///
/// Under `--jdk-only` the three JDK-shaped rows are retired
/// (`RETIRED_SHADOW_JOTP_TRIPLES`) and the bytecode runs anyway; this is the
/// `--compatible` half of the fix, whose first `charAt` raise used to start at
/// `String.checkIndex` with the seven `Preconditions` frames missing.
fn raise_out_of_bounds(
    ctx: &mut dyn NativeContext,
    kind: CheckKind,
    args: &[Value],
    checked: &[i64],
) -> MethodCallFailed {
    // The formatter follows the check's `int`s (`arity` of them).
    let formatter = object_arg(args, kind.arity());
    if args.len() != kind.arity() + 1 || !bounds_raise_bytecode_enabled() {
        return throw_out_of_bounds(ctx, kind, checked, formatter);
    }
    // The formatter is read again after the body ran: a collection inside it
    // may have moved the object, so it is pinned across.
    let pin = formatter.map(|f| ctx.pin_native_root(f));
    let expected = expected_exception_class(ctx, formatter);
    // The formatter slot as a reference whatever it read back as (an
    // unwritten reference slot can arrive as `Value::Int(0)`).
    let mut call_args = args.to_vec();
    call_args[kind.arity()] = Value::Object(formatter);
    let raised = raise_through_bytecode(
        ctx,
        "jdk/internal/util/Preconditions",
        kind.name(),
        formatter_descriptor(kind),
        &call_args,
        expected,
    );
    let formatter = match (pin, formatter) {
        (Some(base), Some(f)) => {
            let fresh = ctx.read_native_pin(base, f);
            ctx.unpin_native_roots(base);
            Some(fresh)
        }
        _ => formatter,
    };
    match raised {
        Some(raised) => raised,
        None => throw_out_of_bounds(ctx, kind, checked, formatter),
    }
}

// ---------------------------------------------------------------------------
// The natives
// ---------------------------------------------------------------------------

/// `checkIndex(int index, int length, BiFunction oobef)`.
fn check_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let index = int_arg(args, 0);
    let length = int_arg(args, 1);
    if index < 0 || index >= length {
        return Err(raise_out_of_bounds(
            ctx,
            CheckKind::Index,
            args,
            &[i64::from(index), i64::from(length)],
        ));
    }
    Ok(Some(Value::Int(index)))
}

/// `checkFromToIndex(int fromIndex, int toIndex, int length, BiFunction oobef)`.
fn check_from_to_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from = int_arg(args, 0);
    let to = int_arg(args, 1);
    let length = int_arg(args, 2);
    if from < 0 || from > to || to > length {
        return Err(raise_out_of_bounds(
            ctx,
            CheckKind::FromToIndex,
            args,
            &[i64::from(from), i64::from(to), i64::from(length)],
        ));
    }
    Ok(Some(Value::Int(from)))
}

/// `checkFromIndexSize(int fromIndex, int size, int length, BiFunction oobef)`.
fn check_from_index_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from = int_arg(args, 0);
    let size = int_arg(args, 1);
    let length = int_arg(args, 2);
    // The JDK writes the last clause as `from > length - size`, which cannot
    // overflow once the negatives are rejected. Widening does the same job
    // without the case analysis — `from + size` in `int` would wrap for
    // `(0, Integer.MAX_VALUE, len)` and report the range as in bounds.
    if from < 0 || size < 0 || length < 0 || i64::from(from) + i64::from(size) > i64::from(length) {
        return Err(raise_out_of_bounds(
            ctx,
            CheckKind::FromIndexSize,
            args,
            &[i64::from(from), i64::from(size), i64::from(length)],
        ));
    }
    Ok(Some(Value::Int(from)))
}

/// Register the `Preconditions` bounds checks.
///
/// Deliberately absent: a no-op over `<clinit>`. The three static formatters it
/// builds are what [`throw_out_of_bounds`] reads to tell
/// `StringIndexOutOfBoundsException` from `IndexOutOfBoundsException`, so
/// suppressing it silently reinstates the wrong-class bug for every
/// `java.lang.String` caller. See the module comment.
pub fn register(registry: &mut NativeMethodRegistry) {
    let class = "jdk/internal/util/Preconditions";
    registry.register(
        class,
        "checkIndex",
        "(IILjava/util/function/BiFunction;)I",
        check_index,
    );
    registry.register(
        class,
        "checkFromToIndex",
        "(IIILjava/util/function/BiFunction;)I",
        check_from_to_index,
    );
    registry.register(
        class,
        "checkFromIndexSize",
        "(IIILjava/util/function/BiFunction;)I",
        check_from_index_size,
    );
    // No-formatter shapes. These are not JDK 25 methods — `Objects.check*`
    // calls the four-argument forms with a `null` formatter — but earlier
    // CratonVM builds registered them and a synthetic class library may still
    // declare them. Routing them through the same code with an absent
    // formatter keeps them on the JDK's own `IndexOutOfBoundsException`
    // fallback instead of the `ArrayIndexOutOfBoundsException` they used to
    // throw.
    registry.register(class, "checkIndex", "(II)I", check_index);
    registry.register(class, "checkFromToIndex", "(III)I", check_from_to_index);
    registry.register(class, "checkFromIndexSize", "(III)I", check_from_index_size);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The message templates are the half of the contract a test can pin
    // without a VM: they are `Preconditions.outOfBoundsMessage` verbatim, and
    // the expected strings below were taken from HotSpot JDK 25 via
    // `probes/PreconditionsFormatterProbe`.

    #[test]
    fn check_index_message_matches_hotspot() {
        assert_eq!(
            CheckKind::Index.message(&[-1, 12]),
            "Index -1 out of bounds for length 12"
        );
        assert_eq!(
            CheckKind::Index.message(&[5, 5]),
            "Index 5 out of bounds for length 5"
        );
    }

    #[test]
    fn check_from_to_index_message_matches_hotspot() {
        assert_eq!(
            CheckKind::FromToIndex.message(&[-1, 12, 12]),
            "Range [-1, 12) out of bounds for length 12"
        );
        assert_eq!(
            CheckKind::FromToIndex.message(&[3, 2, 12]),
            "Range [3, 2) out of bounds for length 12"
        );
        assert_eq!(
            CheckKind::FromToIndex.message(&[i64::from(i32::MIN), i64::from(i32::MAX), 12]),
            "Range [-2147483648, 2147483647) out of bounds for length 12"
        );
    }

    #[test]
    fn check_from_index_size_message_repeats_the_from_index() {
        // `%<s` in the JDK's format string — the second placeholder is `from`
        // again, not `size`. Getting it wrong reads as an off-by-one in the
        // message rather than as a typo.
        assert_eq!(
            CheckKind::FromIndexSize.message(&[-1, 2, 12]),
            "Range [-1, -1 + 2) out of bounds for length 12"
        );
        assert_eq!(
            CheckKind::FromIndexSize.message(&[0, 999, 12]),
            "Range [0, 0 + 999) out of bounds for length 12"
        );
    }

    #[test]
    fn wrong_arity_takes_the_jdk_default_arm() {
        assert_eq!(
            CheckKind::Index.message(&[1, 2, 3]),
            "Range check failed: checkIndex [1, 2, 3]"
        );
    }

    #[test]
    fn check_kind_names_are_the_strings_out_of_bounds_message_switches_on() {
        assert_eq!(CheckKind::Index.name(), "checkIndex");
        assert_eq!(CheckKind::FromToIndex.name(), "checkFromToIndex");
        assert_eq!(CheckKind::FromIndexSize.name(), "checkFromIndexSize");
    }

    /// The regression that silently restores the whole bug.
    ///
    /// With `<clinit>` stubbed out, `SIOOBE_FORMATTER` and its two siblings are
    /// null, so `String.checkBoundsBeginEnd` hands this module a null formatter
    /// and every `String` bounds failure takes the no-formatter fallback —
    /// `IndexOutOfBoundsException` instead of
    /// `StringIndexOutOfBoundsException`. Nothing errors; the class is just
    /// quietly wrong again. The previous stub was justified by a
    /// `LambdaMetafactory` bootstrap hazard that the JDK removed several
    /// releases ago by rewriting the formatters as anonymous inner classes.
    #[test]
    fn clinit_is_not_stubbed_out() {
        let mut registry = NativeMethodRegistry::new();
        register(&mut registry);
        assert!(
            registry
                .find("jdk/internal/util/Preconditions", "<clinit>", "()V")
                .is_none(),
            "`Preconditions.<clinit>` must run: it builds the three static \
             formatters that `throw_out_of_bounds` reads to tell \
             StringIndexOutOfBoundsException from IndexOutOfBoundsException"
        );
    }

    #[test]
    fn every_bounds_check_overload_is_covered() {
        let mut registry = NativeMethodRegistry::new();
        register(&mut registry);
        // The formatter-taking overloads are the ones the defect was about;
        // the bare shapes route through the same code with an absent
        // formatter. A missing entry hands that method to bytecode, which is
        // correct but costs a Java frame on `String.charAt`'s per-character
        // path — the only reason any of this is native.
        for (name, descriptor) in [
            ("checkIndex", "(IILjava/util/function/BiFunction;)I"),
            ("checkFromToIndex", "(IIILjava/util/function/BiFunction;)I"),
            (
                "checkFromIndexSize",
                "(IIILjava/util/function/BiFunction;)I",
            ),
            ("checkIndex", "(II)I"),
            ("checkFromToIndex", "(III)I"),
            ("checkFromIndexSize", "(III)I"),
        ] {
            assert!(
                registry
                    .find("jdk/internal/util/Preconditions", name, descriptor)
                    .is_some(),
                "jdk/internal/util/Preconditions.{name}{descriptor} is not registered"
            );
        }
    }

    #[test]
    fn the_null_formatter_fallback_is_not_a_subclass_of_the_promised_class() {
        // Defect 2 of the known-issues record: the old code threw
        // `ArrayIndexOutOfBoundsException` here, which is a *subclass* of what
        // `Objects.check*` promises — the direction that breaks a `catch`.
        assert_eq!(FALLBACK_EXCEPTION, "java/lang/IndexOutOfBoundsException");
    }

    // --- JIT round 16 wave 8 (lane trace): the raise through the JDK body ---

    /// A body that RETURNED, or failed inside the VM, raised nothing: the
    /// caller keeps its own raise.
    #[test]
    fn a_body_that_did_not_throw_is_not_a_raise() {
        let mut ctx = crate::test_utils::mock_ctx();
        assert!(accept_bytecode_raise(&mut ctx, Ok(Some(Value::Int(3))), None).is_none());
        assert!(accept_bytecode_raise(&mut ctx, Ok(None), None).is_none());
        let internal: MethodCallResult = Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Internal {
                message: "early boot".to_string(),
            },
        ));
        assert!(accept_bytecode_raise(&mut ctx, internal, None).is_none());
    }

    /// With no expected class (an application formatter) any throwable is
    /// the raise; with one, only an instance of it (or a subclass).
    #[test]
    fn the_raise_is_screened_by_its_expected_class() {
        use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
        let mut ctx = crate::test_utils::mock_ctx();
        let ioobe = ctx
            .ensure_class_initialized("java/lang/IndexOutOfBoundsException")
            .unwrap();
        let sioobe = ctx
            .ensure_class_initialized("java/lang/StringIndexOutOfBoundsException")
            .unwrap();
        ctx.set_superclass(sioobe, ioobe);
        let thrown = match ctx.new_object("java/lang/StringIndexOutOfBoundsException") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("mock allocation failed: {other:?}"),
        };
        let raised = |ctx: &mut crate::test_utils::MockNativeContext, expected| {
            accept_bytecode_raise(
                ctx,
                Err(MethodCallFailed::ExceptionThrown(thrown)),
                expected,
            )
        };
        assert!(matches!(
            raised(&mut ctx, None),
            Some(MethodCallFailed::ExceptionThrown(o)) if o == thrown
        ));
        assert!(raised(&mut ctx, Some("java/lang/StringIndexOutOfBoundsException")).is_some());
        assert!(
            raised(&mut ctx, Some("java/lang/IndexOutOfBoundsException")).is_some(),
            "a subclass of the expected class is accepted"
        );
        let _aioobe = ctx
            .ensure_class_initialized("java/lang/ArrayIndexOutOfBoundsException")
            .unwrap();
        assert!(
            raised(&mut ctx, Some("java/lang/ArrayIndexOutOfBoundsException")).is_none(),
            "a sibling class is refused: the native's own raise is kept"
        );
        assert!(
            raised(&mut ctx, Some("no/such/Class")).is_none(),
            "an expected class that is not loaded refuses (fail-closed)"
        );
    }

    /// The JDK-shaped rows name a method with a JDK body; the check still
    /// raises when that body cannot (the mock runs no bytecode).
    #[test]
    fn a_failed_check_still_raises_when_the_body_cannot() {
        assert_eq!(
            formatter_descriptor(CheckKind::Index),
            "(IILjava/util/function/BiFunction;)I"
        );
        assert_eq!(
            formatter_descriptor(CheckKind::FromToIndex),
            "(IIILjava/util/function/BiFunction;)I"
        );
        assert_eq!(
            formatter_descriptor(CheckKind::FromIndexSize),
            "(IIILjava/util/function/BiFunction;)I"
        );
        let mut ctx = crate::test_utils::mock_ctx();
        let four = [Value::Int(5), Value::Int(3), Value::Object(None)];
        assert!(check_index(&mut ctx, &four).is_err());
        let bare = [Value::Int(5), Value::Int(3)];
        assert!(check_index(&mut ctx, &bare).is_err());
        let range = [
            Value::Int(2),
            Value::Int(1),
            Value::Int(3),
            Value::Object(None),
        ];
        assert!(check_from_to_index(&mut ctx, &range).is_err());
        let size = [Value::Int(2), Value::Int(9), Value::Int(3), Value::Int(0)];
        assert!(check_from_index_size(&mut ctx, &size).is_err());
        // The in-range answers are unchanged.
        assert_eq!(
            check_index(
                &mut ctx,
                &[Value::Int(1), Value::Int(3), Value::Object(None)]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
    }
}
