// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

use std::borrow::Cow;
use std::sync::Arc;

use cratonvm_native_api::{
    MhWmteRaise, NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

// `box_value` is deliberately NOT imported: after the split described below,
// every unqualified boxing call in this file is the canonical one, and the
// four sites that must stay fresh spell `crate::lang_class::box_value` in
// full so the exception is visible rather than inferred from the argument.
use crate::lang_class::{box_value_canonical, mirror_class_id, mirror_class_name, ClassTextName};
use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// WHICH BOXING HELPER THIS FILE USES, AND WHY IT IS TWO
// ---------------------------------------------------------------------------
//
// `box_value` allocates a FRESH wrapper on every call. `box_value_canonical`
// routes `I J Z B S C` through the registered `X.valueOf` natives — i.e.
// through the wrapper caches — and falls back to `box_value` for everything
// else. They are NOT interchangeable: the difference is observable with `==`,
// and the choice per call site is a MEASUREMENT.
//
// The measurement, taken on Microsoft OpenJDK 25.0.3+9 (105 rows, byte
// identical across three runs and under `-Xint`, so it is not a JIT artefact —
// scratchpad/f19/ReflBoxOracle.java, tabulated in
// docs/known-issues/jdk-only/F19-1-*.md §2):
//
//   MethodHandle return adaptation (`asType`/`invoke`/`invokeWithArguments`)
//       mh.asTypeInt / asTypeChar / asTypeBool / asTypeLong / asTypeByte /
//       asTypeShort = true,  mh.invokeAsObject = true,
//       mh.invokeWithArgsInt / Char / Long = true          -> CANONICAL
//   MethodHandle collector element boxing
//       mhcoll.int / char / bool / long = true, mhvar.int / char = true
//                                                          -> CANONICAL
//   VarHandle.get, every shape measured
//       vh.fieldInt / Char / Bool / Long / Byte / Short = true,
//       vh.staticInt = true, vh.arrInt / Char / Bool / Long = true,
//       vh.byteViewInt / byteViewLong = true,
//       vh.getAndSetInt = true, vh.compareAndExchangeInt = true
//       ffm.layoutInt / layoutChar / layoutLong / layoutByte = true
//                                                          -> CANONICAL
//   float / double, on EVERY one of those paths
//       mhnc.asTypeFloat = false, vhnc.fieldFloat = false   -> FRESH
//   any value outside its type's cache bound
//       mhoob.asTypeInt1000 = false, vhoob.fieldInt1000 = false,
//       vhoob.staticInt1000 = false, vhoob.arrInt1000 = false,
//       ffmoob.layoutInt1000 = false, mhcolloob.int1000 = false
//                                        -> FRESH, and `box_value_canonical`
//                                           produces that itself: the `valueOf`
//                                           natives have their own uncached arm
//
// Every "FRESH" row above is still `.equals`-equal to the canonical instance
// (blind.equalsOob / blind.equalsFloat = true), so no equality-shaped
// assertion can see this in either direction. `regression-suite/src/
// RJdkReflBox.java` asserts it with `==`, in both directions.
//
// Two sites in this file deliberately keep `box_value`:
//
//   * the `"F"` / `"D"` arms of the two collector loops. Routing them through
//     `box_value_canonical` would be behaviour-IDENTICAL (it delegates F and D
//     straight back), but leaving them spelled `box_value` keeps the measured
//     asymmetry visible at the site instead of hiding it inside a helper.
//     `Float`/`Double` have no cache on HotSpot at all: neg.floatValueOf and
//     neg.doubleValueOf are both `false`.
//   * `native_mhn_get_member_vm_info`'s vmindex. That `Object[]` slot is
//     JDK-internal plumbing whose HotSpot counterpart is `create`-boxed, not
//     `valueOf`-boxed, and no Java-visible identity depends on it. It is the
//     `Array.get` half of the split and is annotated at the site.
//
// ---------------------------------------------------------------------------
// Hoisted descriptor / class-name string constants
// ---------------------------------------------------------------------------
//
// These literals appeared in 30+ `.to_string()` sites scattered across the
// invokedynamic / MethodHandle / VarHandle hot paths. Centralising them as
// `&'static str` lets callers borrow without allocating on the per-call
// fast path, and gives the rest of the file a single source of truth for
// the descriptor character / wrapper-class name mapping.
pub(crate) const DESC_VOID: &str = "V";
pub(crate) const DESC_INT: &str = "I";
pub(crate) const DESC_LONG: &str = "J";
pub(crate) const DESC_FLOAT: &str = "F";
pub(crate) const DESC_DOUBLE: &str = "D";
pub(crate) const DESC_BOOLEAN: &str = "Z";
pub(crate) const DESC_BYTE: &str = "B";
pub(crate) const DESC_CHAR: &str = "C";
pub(crate) const DESC_SHORT: &str = "S";
pub(crate) const DESC_REF: &str = "L";
pub(crate) const DESC_OBJECT: &str = "Ljava/lang/Object;";
pub(crate) const DESC_MEMORY_SEGMENT: &str = "Ljava/lang/foreign/MemorySegment;";

pub(crate) const NAME_VOID: &str = "void";
pub(crate) const NAME_INT: &str = "int";
pub(crate) const NAME_LONG: &str = "long";
pub(crate) const NAME_FLOAT: &str = "float";
pub(crate) const NAME_DOUBLE: &str = "double";
pub(crate) const NAME_BOOLEAN: &str = "boolean";
pub(crate) const NAME_BYTE: &str = "byte";
pub(crate) const NAME_CHAR: &str = "char";
pub(crate) const NAME_SHORT: &str = "short";

pub(crate) const DESC_DEFAULT_METHOD: &str = "()V";
pub(crate) const DESC_DEFAULT_OBJECT_RETURN: &str = "()Ljava/lang/Object;";
pub(crate) const NAME_INVOKE: &str = "invoke";

/// C38: Detect a real-JDK array-element VarHandle call.
///
/// `MethodHandles.arrayElementVarHandle([J.class)` runs JDK bytecode that
/// returns a `java.lang.invoke.VarHandleLongs$Array` (or similar per element
/// type). Those real-JDK VarHandles don't match our synthetic 6-field layout
/// — slot 0 is `vform`, not our `VH_KIND` Int. The reliable signal is the
/// argument shape at the call site: for an array-element access the layout is
/// always `[vh, array_object, index, ...]`. When args[1] is an array object
/// and args[2] is an Int, treat the call as array-element access.
///
/// The shape alone is not enough: an INSTANCE-field handle given an array
/// receiver and an `int` value (`intFieldVh.set((Object) new H[1], 5)`) has the
/// same shape, and was routed here — it stored `5` at array index `5` (or threw
/// `ArrayIndexOutOfBoundsException`) where HotSpot's receiver cast throws
/// `ClassCastException` (r11w13-orch-varhandle-set-on-an-array-receiver-throws-
/// aioobe-not-cce). A STATIC handle's `compareAndSet((Object) arr, 5)` is the
/// same shape again. So a handle whose side-table kind says instance or static
/// is never an array-element call; only then is the meta consulted, so a
/// non-array first argument still costs one header read.
///
/// The shape says nothing about the array's TYPE: that is
/// [`vh_array_coordinate_cast`]'s, which every access-mode native runs first
/// (round 15 wave 4, lane vh5), so an array that reaches here is the handle's
/// whenever the handle's type is known and the cast's switch is on.
pub(crate) fn vh_array_call(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<(ObjectRef, i32)> {
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array) {
        return None;
    }
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => return None,
    };
    if let Some(Value::Object(Some(vh))) = args.first() {
        let vh = *vh;
        if vh_meta_get(ctx, vh)
            .is_some_and(|m| m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC)
        {
            return None;
        }
    }
    Some((arr, idx))
}

/// Range-check an array-element VarHandle coordinate, then narrow it to the
/// `usize` the heap accessors take.
///
/// EVERY array-element VarHandle access funnels through [`vh_array_call`] and
/// then through here, which is the point: the bounds check belongs on the
/// funnel, not on the individual accessors, because there are eight of them
/// (`get`/`set`/`compareAndSet`/`compareAndExchange`/`getAndSet`/`getAndAdd`/
/// `getAndBitwise*`/the ordering variants) and a check added to seven of them
/// is a silent hole in the eighth.
///
/// Why it has to be here at all: the heap DOES range-check
/// (`GenerationalHeap::get_array_element` returns `Err(index)` past the
/// length), but `vm_exec.rs`'s `NativeContext` impl swallows the result —
/// `get_array_element` ends in `.unwrap_or(Value::Int(0))` and
/// `set_array_element` in `let _ = ...`. So `va.get(arr, 7)` on a 3-element
/// array answered `0` and `va.set(arr, 7, x)` dropped the write, both without
/// a whisper. HotSpot throws for every access mode; measured on JDK 25:
///
/// ```text
/// va.get(arr, 7)          ArrayIndexOutOfBoundsException: Index 7 out of bounds for length 3
/// va.get(arr, -1)         ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
/// va.set/getAndSet/CAS/getAndAdd at 7 or -1 — same exception, same wording
/// ```
///
/// `RuntimeError::aioobe` is the constructor that produces exactly that text
/// (it is `Preconditions.checkIndex`'s, character for character), so the
/// message a caller catches and prints matches HotSpot's.
///
/// The negative case is the one that mattered most: the old code did
/// `*i as usize`, so `-1` became `usize::MAX` and only the heap's own
/// `index >= array_length` test stopped it from being a wild read.
/// regression-suite `RJdkHandles:263-269` asserts the throw.
pub(crate) fn vh_array_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: i32,
) -> Result<usize, MethodCallFailed> {
    let len = ctx.array_length(arr);
    if idx < 0 || (idx as usize) >= len {
        return Err(RuntimeError::aioobe(idx, len as i32).into());
    }
    Ok(idx as usize)
}

/// The descriptor of the JDK's static access-mode body `mode` on an
/// array-element or byte-view handle class (`VarHandleInts$Array.get`,
/// `VarHandleByteArrayAsLongs$ByteBufferHandle.compareAndSet`, ...): the
/// `(VarHandle, Object, int)` coordinates, then the mode's values of the
/// variable type `t`, with how many values that is. `None` for a mode the
/// bodies do not spell this way.
pub(crate) fn vh_index_body_descriptor(mode: &str, t: &str) -> Option<(String, usize)> {
    let (values, ret) = match mode {
        "get" | "getVolatile" | "getAcquire" | "getOpaque" => (0, t),
        "set" | "setVolatile" | "setRelease" | "setOpaque" => (1, "V"),
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => (2, "Z"),
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => (2, t),
        m if m.starts_with("getAnd") => (1, t),
        _ => return None,
    };
    Some((
        format!(
            "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;I{}){ret}",
            t.repeat(values)
        ),
        values,
    ))
}

/// JIT round 20 wave 15 (lane fuzz14b; a correctness fix, unconditional --
/// it rides `CRATONVM_BOUNDS_RAISE_BYTECODE`, the bounds raises' existing kill
/// switch): an index the door has ALREADY judged out of bounds on an
/// array-element (`VarHandle<T>s$Array`), `byte[]`-view or `ByteBuffer`-view
/// handle is raised by running the JDK's own static body of the access mode,
/// so the exception is built inside the frames that raise it on HotSpot 25:
///
/// ```text
/// IndexOutOfBoundsException: Index 25 out of bounds for length 25
///   at jdk.internal.util.Preconditions.outOfBounds
///   at jdk.internal.util.Preconditions.outOfBoundsCheckIndex
///   at jdk.internal.util.Preconditions.checkIndex
///   at java.lang.invoke.VarHandleByteArrayAsLongs$ByteBufferHandle.index
///   at java.lang.invoke.VarHandleByteArrayAsLongs$ByteBufferHandle.get
///   at <the Java caller>
/// ```
///
/// The native raise started at the Java caller: every JDK frame was missing
/// (`R20FuzzGVhViewMis1`, `R20FuzzGVhTraceFrames1`). A view handle this VM
/// minted is synthetic, so a real JDK handle of the same class and byte order
/// is built for the body (`new <class>(boolean be)`); a real handle (an
/// `arrayElementVarHandle` result, an adopted real view) is used as is. The
/// value parameters are passed as zero / null: the bodies check the index
/// before they read a value. Fail-closed: no JDK body, a handle that cannot
/// be built, or a body that throws anything but `expected` answers
/// `fallback`, the old raise.
pub(crate) fn vh_index_raise_through_jdk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    view: Option<(u8, bool, bool)>,
    mode: &str,
    coordinate: Value,
    index: i32,
    fallback: MethodCallFailed,
) -> MethodCallFailed {
    if !crate::preconditions::bounds_raise_bytecode_enabled() {
        return fallback;
    }
    let this_class = ctx.class_id_of_object(this);
    let this_name = ctx.class_name_of_id(this_class).unwrap_or_default();
    let (class_name, t, expected): (String, &'static str, &'static str) = match view {
        None => {
            let Some(family) = this_name
                .strip_prefix("java/lang/invoke/VarHandle")
                .and_then(|rest| rest.strip_suffix("$Array"))
            else {
                return fallback;
            };
            let t = match family {
                "Booleans" => "Z",
                "Bytes" => "B",
                "Shorts" => "S",
                "Chars" => "C",
                "Ints" => "I",
                "Longs" => "J",
                "Floats" => "F",
                "Doubles" => "D",
                "References" => "Ljava/lang/Object;",
                _ => return fallback,
            };
            (
                this_name.clone(),
                t,
                "java/lang/ArrayIndexOutOfBoundsException",
            )
        }
        Some((elem, _, buffer)) => {
            let (family, t) = match elem {
                b'S' => ("Shorts", "S"),
                b'C' => ("Chars", "C"),
                b'I' => ("Ints", "I"),
                b'J' => ("Longs", "J"),
                b'F' => ("Floats", "F"),
                b'D' => ("Doubles", "D"),
                _ => return fallback,
            };
            let (shape, expected) = if buffer {
                ("ByteBufferHandle", "java/lang/IndexOutOfBoundsException")
            } else {
                ("ArrayHandle", "java/lang/ArrayIndexOutOfBoundsException")
            };
            (
                format!("java/lang/invoke/VarHandleByteArrayAs{family}${shape}"),
                t,
                expected,
            )
        }
    };
    let Some((descriptor, values)) = vh_index_body_descriptor(mode, t) else {
        return fallback;
    };
    let mut coordinate = coordinate;
    let handle = match view {
        Some((_, little_endian, _)) if this_name != class_name => {
            // The coordinate is pinned across the handle's allocation.
            let pin = match coordinate {
                Value::Object(Some(c)) => Some((ctx.pin_native_root(c), c)),
                _ => None,
            };
            let made = ctx.new_object_initialized(
                &class_name,
                "(Z)V",
                &[Value::Int(i32::from(!little_endian))],
            );
            if let Some((base, c)) = pin {
                coordinate = Value::Object(Some(ctx.read_native_pin(base, c)));
                ctx.unpin_native_roots(base);
            }
            match made {
                Ok(Some(Value::Object(Some(h)))) => h,
                _ => return fallback,
            }
        }
        _ => this,
    };
    let zero = match t.as_bytes()[0] {
        b'J' => Value::Long(0),
        b'F' => Value::Float(0.0),
        b'D' => Value::Double(0.0),
        b'L' => Value::Object(None),
        _ => Value::Int(0),
    };
    let mut call = vec![Value::Object(Some(handle)), coordinate, Value::Int(index)];
    call.extend(std::iter::repeat(zero).take(values));
    crate::preconditions::raise_through_bytecode(
        ctx,
        &class_name,
        mode,
        &descriptor,
        &call,
        Some(expected),
    )
    .unwrap_or(fallback)
}

/// [`vh_array_index`], with an out-of-bounds index raised through the JDK body
/// of `mode` on the real handle `this` ([`vh_index_raise_through_jdk`]).
pub(crate) fn vh_array_index_raising(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: &str,
    arr: ObjectRef,
    idx: i32,
) -> Result<usize, MethodCallFailed> {
    match vh_array_index(ctx, arr, idx) {
        Ok(i) => Ok(i),
        Err(fallback) => Err(vh_index_raise_through_jdk(
            ctx,
            this,
            None,
            mode,
            Value::Object(Some(arr)),
            idx,
            fallback,
        )),
    }
}

/// `CRATONVM_VH_INDEX_COORDINATE_KINDS` (default on, both modes; round 15
/// wave 3, lane vh4; VH3-2,
/// `r15w3-vh3-minted-handle-index-coordinate-kinds-FIXED-20260930.md`): the `int`
/// INDEX coordinate of an array-element, byte-array-view or `ByteBuffer`-view
/// handle is converted from the call site's static type as `asType` converts
/// it ([`vh_index_coordinate_args`]). `0` restores the round-15-wave-2
/// decode: only a `Value::Int` index is an index (an array handle declines any
/// other, a view reads it as `0`).
pub(crate) fn vh_index_coordinate_kinds_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDEX_COORDINATE_KINDS")
    })
}

/// `CRATONVM_VH_VIEW_VALUE_ADAPT` (default on, both modes; round 15 wave 3,
/// lane vh4): a byte-array or `ByteBuffer` view's VALUE parameters are
/// converted to the view's primitive as `asType` converts them
/// (`vh_primitive_var_desc`, then `vh_adapt_value_args`), as a field's or an
/// array element's already are. `0` hands the raw value to the byte encoder
/// again (its bits: `5` into a `float` view stored a denormal, a boxed value
/// stored `0`).
pub(crate) fn vh_view_value_adapt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_VIEW_VALUE_ADAPT")
    })
}

/// Is `this` a handle whose SECOND coordinate is an `int` index: the VM's own
/// array-element, byte-array-view and `ByteBuffer`-view handles, and a real-JDK
/// `VarHandle*$Array` (no side-table row)? A view whose access mode `mode`
/// refuses (`UnsupportedOperationException`, which HotSpot raises from
/// `getMethodHandle` BEFORE `asType`) answers `false`, so the refusal keeps
/// its order. FFM layout and segment handles (`(MemorySegment, long)`) are
/// never index handles: a `long` there is their real coordinate.
pub(crate) fn vh_has_index_coordinate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
    mode: &str,
) -> bool {
    let meta = vh_meta_get(ctx, this);
    match meta.as_deref() {
        Some(m) => match m.kind {
            VH_KIND_ARRAY => true,
            VH_KIND_BYTE_VIEW_LE
            | VH_KIND_BYTE_VIEW_BE
            | VH_KIND_BYTE_BUFFER_VIEW_LE
            | VH_KIND_BYTE_BUFFER_VIEW_BE => !vh_view_mode_unsupported(m, mode),
            _ => false,
        },
        None => {
            // The array shape first (one header read), so an FFM handle's
            // `MemorySegment` coordinate never pays the class-name read.
            let shaped = match args.get(1) {
                Some(Value::Object(Some(a))) => matches!(ctx.heap_kind_of(*a), ObjectKind::Array),
                // A null array is still an array handle's coordinate: `asType`
                // judges the index before the access finds the null.
                Some(Value::Object(None)) => true,
                _ => false,
            };
            shaped && real_array_var_handle_descriptors(ctx, this).is_some()
        }
    }
}

/// The INDEX coordinate (`args[2]`) of an access on a handle
/// [`vh_has_index_coordinate`] accepts, converted from the call site's static
/// type as `asType` converts it (round 15 wave 3, lane vh4; VH3-2 of
/// `jit-r15w3-vh3-proposals-RETIRED-20261003.md`). `args` is `[handle, array, index,
/// values...]` with `n_values` values; `site` is the parameter-site word the
/// native took (`poly_call_site::take_vh_param_site`, `0` = none armed).
///
/// | index at the call site         | HotSpot 25 (`asType`)            |
/// |--------------------------------|----------------------------------|
/// | `int`/`short`/`char`/`byte`    | the index                        |
/// | `long`/`float`/`double`/`boolean` | `WrongMethodTypeException`    |
/// | `Object`/`Number`/`Integer`... | unbox-widen: `Integer`/`Short`/`Character`/`Byte` -> the index, null -> NPE, another box -> CCE |
/// | `String`, `Long`, ...          | `WrongMethodTypeException`       |
///
/// Before, only a `Value::Int` index was decoded: an array handle declined
/// the access (and answered `null`/`0` or dropped a store), and the byte views
/// read and WROTE at offset 0 whatever the index was -- a silent wrong value,
/// and on a `ByteBuffer` view an atomic update of the wrong word.
///
/// Borrows `args` unchanged -- no allocation, one match -- whenever the index
/// is a `Value::Int` that the site (when armed) does not declare
/// `long`/`float`/`double`/`boolean`: every ordinary access. Otherwise hands
/// back a copy whose `args[2]` is the converted `Value::Int`, so every decode
/// downstream (the bounds checks included) sees the REAL index. The unbox
/// reads the wrapper's payload and allocates nothing; only a refusal
/// allocates (the exception), and then nothing else runs.
///
/// Order: after the read-only and access-mode refusals (`getMethodHandle`'s
/// `UnsupportedOperationException` precedes `asType`), before the arity check
/// (a wrong count never reaches here: the length test below), the view's
/// run-time checks and the null-coordinate check (asType precedes the
/// access). A VALUE's static refusal is judged after this one, so a site that
/// is wrong in both its index and a value reports the index (both are
/// `WrongMethodTypeException`); a run-time index refusal (null, a wrong box)
/// then precedes a value's static one, which HotSpot orders the other way.
pub(crate) fn vh_index_coordinate_args<'a>(
    ctx: &mut dyn NativeContext,
    args: &'a [Value],
    n_values: usize,
    site: u64,
    mode: &str,
) -> Result<Cow<'a, [Value]>, MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::{vh_param_site_class_at, vh_site};
    if args.len() != 3 + n_values {
        return Ok(Cow::Borrowed(args));
    }
    let index = args[2];
    // The index parameter, counted back from the LAST call-site parameter:
    // the `n_values` values trail it.
    let class = vh_param_site_class_at(site, n_values);
    if matches!(index, Value::Int(_)) && !matches!(class, b'Z' | b'J' | b'F' | b'D') {
        return Ok(Cow::Borrowed(args));
    }
    if !vh_index_coordinate_kinds_enabled() {
        return Ok(Cow::Borrowed(args));
    }
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Cow::Borrowed(args));
    };
    if !vh_has_index_coordinate(ctx, this, args, mode) {
        return Ok(Cow::Borrowed(args));
    }
    // `MethodType.canConvert(site, int)` for a primitive site: widening only.
    if vh_site::is_primitive(class) && !primitive_widens(class, b'I') {
        return Err(vh_astype_refused(ctx, this, |ctx| {
            crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/invoke/WrongMethodTypeException",
                &format!("cannot convert {} to int", vh_prim_java_name(class)),
            )
        }));
    }
    // The static half of a reference index (`vh_adapt_value` repeats it), so
    // its refusal carries the handle's shape for the door's message.
    if matches!(index, Value::Object(_)) && vh_site_admits_primitive(class, b'I') == Some(false) {
        return Err(vh_astype_refused(ctx, this, |ctx| {
            vh_value_wmte(ctx, index, b'I')
        }));
    }
    let converted = match index {
        Value::Int(i) => i,
        // A `long`/`float`/`double` never narrows to `int`, armed or not.
        Value::Long(_) | Value::Float(_) | Value::Double(_) => {
            return Err(vh_astype_refused(ctx, this, |ctx| {
                vh_value_wmte(ctx, index, b'I')
            }));
        }
        // A reference: the static check against the site's class, then
        // `unboxWiden`, exactly as a value parameter of an `int` variable.
        // The index converts BEFORE the array coordinate's cast: measured on
        // HotSpot 25 (`R15DrainBVhIndexBoxOrder`, round 15 wave 7: a wrong
        // array with a null / `Long` index is the index's NPE / CCE).
        Value::Object(_) => match vh_adapt_value(ctx, b'I', index, class)? {
            Value::Int(i) => i,
            _ => return Err(vh_value_wmte(ctx, index, b'I')),
        },
        _ => return Ok(Cow::Borrowed(args)),
    };
    let mut out = args.to_vec();
    out[2] = Value::Int(converted);
    Ok(Cow::Owned(out))
}

/// The `VH_KIND_ARRAY` side-table arms of `get` / `getAndAdd` /
/// `getAndBitwise*` (round 15 wave 3, lane vh4): the element index, or `None`
/// when the index is not an `int` (only under
/// `CRATONVM_VH_INDEX_COORDINATE_KINDS=0`; the caller then answers its
/// declined value without touching the array). Those arms run only when
/// [`vh_array_call`] declined, i.e. when `arr` is NOT an array (a CCE on
/// HotSpot: the coordinate is cast to the array type) or the index was not an
/// `int`. They used to take `*i as usize` unchecked (a negative index wrapped)
/// and read, or ADD INTO, the element of whatever object `arr` was.
pub(crate) fn vh_kind_array_fallback_index(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    array_class: Option<&str>,
    args: &[Value],
) -> Result<Option<usize>, MethodCallFailed> {
    if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array) {
        let cid = ctx.class_id_of_object(arr);
        let from = ctx
            .class_name_of_id(cid)
            .unwrap_or_default()
            .class_text(&*ctx);
        let to = array_class
            .unwrap_or("[Ljava/lang/Object;")
            .class_text(&*ctx);
        return Err(RuntimeError::ClassCastException {
            message: format!("Cannot cast {from} to {to}"),
        }
        .into());
    }
    match args.get(2) {
        Some(Value::Int(i)) => vh_array_index(ctx, arr, *i).map(Some),
        _ => Ok(None),
    }
}

/// `CRATONVM_VH_ARRAY_COORDINATE_CAST` (default on, both modes; round 15
/// wave 4, lane vh5; VH4-1,
/// `r15w3-vh4-array-coordinate-runtime-type-unchecked-FIXED-20260930.md`): the
/// array coordinate of an array-element handle, and the `byte[]` /
/// `ByteBuffer` coordinate of a view, is cast to the handle's coordinate type
/// ([`vh_array_coordinate_cast`]). `0` restores the shape-only recognition,
/// which read a `long[]` through an `int[]` handle and stored into an
/// `Object[]` through it.
pub(crate) fn vh_array_coordinate_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_ARRAY_COORDINATE_CAST")
    })
}

thread_local! {
    /// Per-thread memo of [`vh_real_array_coordinate_desc`], on the terms of
    /// `VH_META_MEMO` ([`vh_memo_lookup`]): a real-JDK `VarHandle*$Array` has
    /// no side-table row, and its array type is a class-name read (plus two
    /// field reads for `References$Array`) that no access should pay.
    pub(crate) static VH_ARRAY_COORD_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<str>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );

    /// `(vm, class id)` of the last `ByteBuffer`-view coordinate class
    /// [`vh_byte_buffer_coordinate_admits`] admitted. Positive answers only:
    /// every `ByteBuffer` subclass is a boot class (package-private
    /// constructors), so an admitted id never changes meaning.
    pub(crate) static VH_BYTE_BUFFER_COORD_OK: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// The array coordinate type (`[I`, `[Ljava/lang/String;`) of a REAL-JDK
/// array-element handle (`real_array_var_handle_descriptors`'s first
/// coordinate), memoised per thread; `None` for every other handle.
pub(crate) fn vh_real_array_coordinate_desc(ctx: &dyn NativeContext, vh: ObjectRef) -> Option<Arc<str>> {
    vh_memo_lookup(&VH_ARRAY_COORD_MEMO, ctx, vh, |_| {
        let (_, coords) = real_array_var_handle_descriptors(ctx, vh)?;
        coords
            .into_iter()
            .next()
            .filter(|d| d.starts_with('['))
            .map(Arc::<str>::from)
    })
}

/// Is `coord` an instance of the array type `want` (`checkcast`'s answer)?
/// `Some(false)` is a positive mismatch, `None` "cannot tell" (the callers
/// refuse only on `Some(false)`). A primitive array type admits exactly its
/// own element kind; a reference array type never admits a primitive array;
/// `Object[]` admits every reference array (JLS 4.10.3); an array whose header
/// names exactly `want`'s component is admitted without asking the VM; the
/// rest is the VM's `checkcast` predicate (`array_value_casts_to_desc`).
pub(crate) fn vh_array_coordinate_admits(
    ctx: &dyn NativeContext,
    coord: ObjectRef,
    want: &str,
) -> Option<bool> {
    if !want.starts_with('[') {
        return None;
    }
    if !matches!(ctx.heap_kind_of(coord), ObjectKind::Array) {
        return Some(false);
    }
    let have = ctx.heap_element_type_of(coord);
    if let Some(elem) = one_dim_primitive_array_element(want) {
        return Some(have == elem);
    }
    if have != ArrayElementType::Reference {
        return Some(false);
    }
    if want == DESC_OBJECT_ARRAY {
        return Some(true);
    }
    // Only an internal (`/`) name is a question the VM's predicate answers;
    // a Java-form name from a mirror would be refused for its spelling.
    if want.contains('.') {
        return None;
    }
    // A reference array's header carries its COMPONENT's class id, so an exact
    // one-dimensional match is one read-locked name compare.
    if let Some(component) = want.strip_prefix("[L").and_then(|c| c.strip_suffix(';')) {
        let header = ctx.class_name_arc_of_id(ctx.class_id_of_object(coord));
        if header.as_deref() == Some(component) {
            return Some(true);
        }
    }
    ctx.array_value_casts_to_desc(coord, want)
}

pub(crate) const DESC_OBJECT_ARRAY: &str = "[Ljava/lang/Object;";

/// Is `coord` a `java.nio.ByteBuffer` (the coordinate type of a `ByteBuffer`
/// view)? [`reference_arg_admitted`]'s answer, with the last admitted class
/// remembered per thread so a view's steady state pays one compare.
pub(crate) fn vh_byte_buffer_coordinate_admits(ctx: &mut dyn NativeContext, coord: ObjectRef) -> Option<bool> {
    if matches!(ctx.heap_kind_of(coord), ObjectKind::Array) {
        return Some(false);
    }
    let key = (ctx.vm_identity(), ctx.class_id_of_object(coord).as_u32());
    if VH_BYTE_BUFFER_COORD_OK.with(|c| c.get() == Some(key)) {
        return Some(true);
    }
    let buffer = Value::Object(Some(coord));
    let verdict = reference_arg_admitted(ctx, buffer, "Ljava/nio/ByteBuffer;");
    if verdict == Some(true) {
        VH_BYTE_BUFFER_COORD_OK.with(|c| c.set(Some(key)));
    }
    verdict
}

/// `asType`'s cast of the LEADING coordinate of an array-element or view
/// access to the handle's coordinate type (round 15 wave 4, lane vh5; VH4-1,
/// `r15w3-vh4-array-coordinate-runtime-type-unchecked-FIXED-20260930.md`).
///
/// `arrayElementVarHandle(int[].class)`'s coordinate is `int[]`, a byte-array
/// view's `byte[]`, a `ByteBuffer` view's `ByteBuffer`. A call site that
/// passes it as `Object` makes the JDK body cast it, and HotSpot 25 throws
/// `ClassCastException` for any other type (worded by
/// [`vh_coordinate_cast_failure`], round 15 wave 4 lane vh6). The natives
/// recognised an array access by SHAPE ([`vh_array_call`]: any array, an `int`
/// index), so `(int) AI.get((Object) longs, i)` read the `long` element,
/// `AI.set((Object) objs, i, v)` stored a VM autobox into an `Object[]` (heap
/// corruption in Java terms), and a byte view read any array's elements as
/// bytes (`R15Vh4ArrayCoordCast`).
///
/// `args` is `[handle, coordinate, index, values...]` with `n_values`
/// values; any other length is the arity check's (a WMTE, which precedes).
/// A null coordinate casts and is the null-coordinate check's NPE. Runs after
/// the read-only / access-mode UOEs and the index's static refusals, and
/// before the arity check (whose view arm raises the `ByteBuffer` view's
/// run-time `IllegalStateException`s, which follow the cast on HotSpot); a
/// view mode the view refuses is left to that UOE. Known order residuals
/// (`asType` judges every parameter statically before converting any): a
/// VALUE's static WMTE and a boxed index's run-time refusal are reported
/// after / before this cast respectively, the reverse of HotSpot's (VH4-2).
///
/// Cost on the hot path: the side-table memo hit the native pays anyway, a
/// header kind and element-kind read; a real-JDK handle adds one per-thread
/// memo probe ([`vh_real_array_coordinate_desc`]). The coordinates of FFM
/// handles (a `long` offset) and field handles (other arities) never reach
/// the memo.
pub(crate) fn vh_array_coordinate_cast(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    n_values: usize,
    mode: &str,
) -> Result<(), MethodCallFailed> {
    if args.len() != 3 + n_values {
        return Ok(());
    }
    let (Value::Object(Some(this)), Value::Object(Some(coord))) = (args[0], args[1]) else {
        return Ok(());
    };
    if !vh_array_coordinate_cast_enabled() {
        return Ok(());
    }
    let meta = vh_meta_get(ctx, this);
    let target: Cow<'static, str> = match meta.as_deref() {
        Some(m) => match m.kind {
            VH_KIND_ARRAY => {
                if vh_array_coordinate_admits(&*ctx, coord, &m.class_name) != Some(false) {
                    return Ok(());
                }
                Cow::Owned(m.class_name.clone())
            }
            VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => {
                if vh_view_mode_unsupported(m, mode)
                    || vh_array_coordinate_admits(&*ctx, coord, "[B") != Some(false)
                {
                    return Ok(());
                }
                Cow::Borrowed("[B")
            }
            VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => {
                if vh_view_mode_unsupported(m, mode)
                    || vh_byte_buffer_coordinate_admits(ctx, coord) != Some(false)
                {
                    return Ok(());
                }
                Cow::Borrowed("Ljava/nio/ByteBuffer;")
            }
            _ => return Ok(()),
        },
        None => {
            // An FFM handle's second coordinate is a `long` offset: not an
            // array handle, and not worth the memo probe.
            if matches!(args[2], Value::Long(_) | Value::Float(_) | Value::Double(_)) {
                return Ok(());
            }
            let Some(want) = vh_real_array_coordinate_desc(&*ctx, this) else {
                return Ok(());
            };
            if vh_array_coordinate_admits(&*ctx, coord, &want) != Some(false) {
                return Ok(());
            }
            Cow::Owned(want.to_string())
        }
    };
    // Round 15 wave 4 (lane vh6): HotSpot's wording for this cast.
    Err(vh_coordinate_cast_failure(
        ctx,
        Value::Object(Some(coord)),
        &target,
    ))
}

/// `CRATONVM_VH_CAST_CHECKCAST_MESSAGE` (default on, both modes; round 15
/// wave 4, lane vh6): the array / view coordinate cast of a handle whose JDK
/// body casts with a `checkcast` instruction is refused with the VM's
/// `checkcast` wording ([`vh_coordinate_cast_failure`]). `0` restores
/// `Cannot cast X to Y` for it.
pub(crate) fn vh_cast_checkcast_message_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_CAST_CHECKCAST_MESSAGE")
    })
}

/// Does the JDK 25 `VarHandle` implementation cast a coordinate of type
/// `target_desc` with a `checkcast` INSTRUCTION (the VM's `class X cannot be
/// cast to class Y (...)` message) rather than `Class.cast` (`Cannot cast X
/// to Y`)? The access-mode methods take the coordinate as `Object`; the cast is
/// in their bodies:
///
/// * `VarHandleInts.Array` (and every primitive `VarHandle*s.Array`):
///   `int[] array = (int[]) oarray;` -- `checkcast`;
/// * `VarHandleByteArrayAsInts.ArrayHandle`: `byte[] ba = (byte[]) oba;`,
///   and `ByteBufferHandle`: `ByteBuffer bb = (ByteBuffer)
///   Objects.requireNonNull(obb);` -- `checkcast`;
/// * `VarHandleReferences.Array` (any reference array, `int[][]` included):
///   `(Object[]) handle.arrayType.cast(oarray)` -- `Class.cast`, as are its
///   value and CAS-expectation casts (`handle.componentType.cast(..)`).
///
/// Measured on HotSpot 25 (`R15Vh6CastMessages`): `coordInt` / `coordBuffer`
/// print the `checkcast` wording; `coordStr`, `coordUser`, `arr2Value`,
/// `casExpected` print `Cannot cast`. So: a one-dimensional PRIMITIVE array
/// type (a byte view's `[B` is one) or `java.nio.ByteBuffer`.
pub(crate) fn vh_coordinate_cast_is_checkcast(target_desc: &str) -> bool {
    match target_desc.as_bytes() {
        [b'[', elem] => b"ZBCSIJFD".contains(elem),
        _ => matches!(
            target_desc,
            "Ljava/nio/ByteBuffer;" | "Ljava.nio.ByteBuffer;"
        ),
    }
}

/// The `checkcast` refusal text, `class [J cannot be cast to class [I`: the
/// bare two-operand shape the VM's exception funnel
/// (`runtime::exceptions::hotspot_class_cast_message`) completes with
/// HotSpot's `(... are in module java.base of loader 'bootstrap')` clause.
/// Both operands in `Klass::external_name` form (dotted, arrays as
/// descriptors). `from` is already external.
pub(crate) fn vh_checkcast_message(from: &str, target_desc: &str) -> String {
    let to = target_desc
        .strip_prefix('L')
        .and_then(|t| t.strip_suffix(';'))
        .unwrap_or(target_desc)
        .hidden_dotted();
    format!("class {from} cannot be cast to class {to}")
}

/// The `ClassCastException` of the array / view COORDINATE cast of `value` to
/// `target_desc` (round 15 wave 4, lane vh6): the `checkcast` wording when the
/// JDK body casts with a `checkcast` instruction
/// ([`vh_coordinate_cast_is_checkcast`]), else `Class.cast`'s
/// ([`reference_cast_failure`]). `R15Vh5CoordCastModes`'s `msg` line: HotSpot
/// 25 `class [J cannot be cast to class [I ([J and [I are in module
/// java.base of loader 'bootstrap')`, where CratonVM printed `Cannot cast [J
/// to [I`. The component value / expectation casts stay `Cannot cast`.
pub(crate) fn vh_coordinate_cast_failure(
    ctx: &mut dyn NativeContext,
    value: Value,
    target_desc: &str,
) -> MethodCallFailed {
    if !vh_cast_checkcast_message_enabled() || !vh_coordinate_cast_is_checkcast(target_desc) {
        return reference_cast_failure(ctx, value, target_desc);
    }
    let from = match value {
        Value::Object(Some(o)) if ctx.object_is_array(o) => entry_value_type_display(&*ctx, o),
        Value::Object(Some(o)) => {
            let cid = ctx.class_id_of_object(o);
            match ctx.class_name_of_id(cid) {
                Some(name) => name.class_text(&*ctx),
                None => return reference_cast_failure(ctx, value, target_desc),
            }
        }
        _ => return reference_cast_failure(ctx, value, target_desc),
    };
    RuntimeError::ClassCastException {
        message: vh_checkcast_message(&from, target_desc),
    }
    .into()
}

#[cfg(test)]
mod r15w4_vh6_cast_message_tests {
    use super::*;

    /// The probe's `msg` lines: the bare shape the VM funnel completes.
    #[test]
    fn a_primitive_array_cast_is_worded_as_checkcast() {
        assert_eq!(
            vh_checkcast_message("[J", "[I"),
            "class [J cannot be cast to class [I"
        );
        assert_eq!(
            vh_checkcast_message("[B", "Ljava/nio/ByteBuffer;"),
            "class [B cannot be cast to class java.nio.ByteBuffer"
        );
    }

    /// HotSpot 25 (`R15Vh6CastMessages`): `checkcast` for a primitive array
    /// handle's and a view's coordinate; `Class.cast` for a reference array
    /// handle (`String[]`, a user class, `int[][]`).
    #[test]
    fn only_primitive_array_and_view_coordinates_take_the_checkcast_wording() {
        for yes in ["[I", "[J", "[B", "[Z", "[D", "Ljava/nio/ByteBuffer;"] {
            assert!(vh_coordinate_cast_is_checkcast(yes), "{yes}");
        }
        for no in [
            "[[I",
            "[Ljava/lang/String;",
            "[Ljava/lang/Object;",
            "[LFoo;",
            "Ljava/lang/String;",
            "I",
            "[",
            "[X",
        ] {
            assert!(!vh_coordinate_cast_is_checkcast(no), "{no}");
        }
    }
}

/// Round 15 wave 4 (lane vh5; VH4-1): the array coordinate is cast to the
/// handle's array type. Probe twin: `R15Vh4ArrayCoordCast` (`longArr`,
/// `refArrSet`, `bvIntArr`), `R15Vh5CoordCastModes`.
#[cfg(test)]
mod r15w4_vh5_array_coordinate_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        }
    }

    fn handle(ctx: &mut MockNativeContext, kind: i32, class_name: &str, desc: &str) -> Value {
        let vh = obj(ctx, "java/lang/invoke/VarHandle");
        vh_meta_put(
            ctx,
            vh,
            VarHandleMeta {
                kind,
                class_name: class_name.to_string(),
                field_name: String::new(),
                field_desc: desc.to_string(),
                field_index: -1,
                class_id: 0,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        Value::Object(Some(vh))
    }

    fn is_cce(err: &MethodCallFailed) -> bool {
        matches!(
            err,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            ))
        )
    }

    /// The probe's `longArr` / `refArrSet` roads on the VM's own `int[]`
    /// handle: a `long[]` read and an `Object[]` store are CCE and the
    /// `Object[]` keeps its element; the `int[]` control reads.
    #[test]
    fn an_int_array_handle_casts_its_array() {
        if !vh_array_coordinate_cast_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        let mut ctx = MockNativeContext::new();
        let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
        let ints = ctx.new_array(ArrayElementType::Int, 4);
        ctx.set_array_element(ints, 1, Value::Int(41));
        let longs = ctx.new_array(ArrayElementType::Long, 4);
        let objs = ctx.new_array(ArrayElementType::Reference, 4);
        let marker = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        ctx.set_array_element(objs, 2, marker);
        let got = varhandle_get(&mut ctx, &[av, Value::Object(Some(ints)), Value::Int(1)]);
        assert!(got.is_ok(), "int[] control: {got:?}");
        let err =
            varhandle_get(&mut ctx, &[av, Value::Object(Some(longs)), Value::Int(1)]).unwrap_err();
        assert!(is_cce(&err), "long[] read: {err:?}");
        let err = varhandle_set(
            &mut ctx,
            &[av, Value::Object(Some(objs)), Value::Int(2), Value::Int(7)],
        )
        .unwrap_err();
        assert!(is_cce(&err), "Object[] store: {err:?}");
        assert_eq!(
            ctx.get_array_element(objs, 2),
            marker,
            "no refused store landed"
        );
        let err = varhandle_compare_and_set(
            &mut ctx,
            &[
                av,
                Value::Object(Some(longs)),
                Value::Int(0),
                Value::Long(0),
                Value::Long(9),
            ],
        )
        .unwrap_err();
        assert!(is_cce(&err), "long[] CAS: {err:?}");
        assert_eq!(
            ctx.get_array_element(longs, 0),
            Value::Int(0),
            "no refused CAS landed"
        );
        // A non-array coordinate is CCE too; a null one is still the NPE.
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let err = varhandle_get(&mut ctx, &[av, s, Value::Int(0)]).unwrap_err();
        assert!(is_cce(&err), "String coordinate: {err:?}");
        if let Err(err) = varhandle_get(&mut ctx, &[av, Value::Object(None), Value::Int(0)]) {
            assert!(!is_cce(&err), "null coordinate: {err:?}");
        }
    }

    /// A reference-array handle admits every reference array the VM cannot
    /// refuse (the mock's `checkcast` answers "cannot tell") and refuses a
    /// primitive array; `Object[]` admits any reference array.
    #[test]
    fn a_reference_array_handle_refuses_only_primitive_arrays() {
        if !vh_array_coordinate_cast_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let ov = handle(
            &mut ctx,
            VH_KIND_ARRAY,
            "[Ljava/lang/Object;",
            "Ljava/lang/Object;",
        );
        let sv = handle(
            &mut ctx,
            VH_KIND_ARRAY,
            "[Ljava/lang/String;",
            "Ljava/lang/String;",
        );
        let objs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 2)));
        let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 2)));
        assert!(varhandle_get(&mut ctx, &[ov, objs, Value::Int(0)]).is_ok());
        assert!(varhandle_get(&mut ctx, &[sv, objs, Value::Int(0)]).is_ok());
        let err = varhandle_get(&mut ctx, &[ov, ints, Value::Int(0)]).unwrap_err();
        assert!(is_cce(&err), "int[] into Object[] handle: {err:?}");
    }

    /// The probe's `bvIntArr` road: a byte view casts its coordinate to
    /// `byte[]`, and a mode the view refuses is still the UOE, not the CCE.
    #[test]
    fn a_byte_view_casts_to_byte_array_after_its_mode_refusal() {
        if !vh_array_coordinate_cast_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let bv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
        let bytes = Value::Object(Some(ctx.new_array(ArrayElementType::Byte, 8)));
        let ints = ctx.new_array(ArrayElementType::Int, 8);
        ctx.set_array_element(ints, 0, Value::Int(0x7f));
        let ints = Value::Object(Some(ints));
        assert!(varhandle_get(&mut ctx, &[bv, bytes, Value::Int(0)]).is_ok());
        let err = varhandle_get(&mut ctx, &[bv, ints, Value::Int(0)]).unwrap_err();
        assert!(is_cce(&err), "int[] through a byte view: {err:?}");
        let err = varhandle_set(&mut ctx, &[bv, ints, Value::Int(0), Value::Int(1)]).unwrap_err();
        assert!(is_cce(&err), "int[] store through a byte view: {err:?}");
        let err =
            varhandle_get_and_set(&mut ctx, &[bv, ints, Value::Int(0), Value::Int(1)]).unwrap_err();
        assert!(
            matches!(
                err,
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::UnsupportedOperationException { .. }
                ))
            ),
            "getAndSet on a byte view: {err:?}"
        );
    }

    /// The admission predicate alone: exact element kinds, `Object[]` over
    /// every reference array, never a primitive array into a reference type.
    #[test]
    fn the_admission_predicate_is_checkcasts_shape() {
        let mut ctx = MockNativeContext::new();
        let ints = ctx.new_array(ArrayElementType::Int, 1);
        let bools = ctx.new_array(ArrayElementType::Boolean, 1);
        let refs = ctx.new_array(ArrayElementType::Reference, 1);
        let s = obj(&mut ctx, "java/lang/String");
        let c: &dyn NativeContext = &ctx;
        assert_eq!(vh_array_coordinate_admits(c, ints, "[I"), Some(true));
        assert_eq!(vh_array_coordinate_admits(c, ints, "[J"), Some(false));
        assert_eq!(vh_array_coordinate_admits(c, bools, "[Z"), Some(true));
        assert_eq!(vh_array_coordinate_admits(c, bools, "[B"), Some(false));
        assert_eq!(
            vh_array_coordinate_admits(c, refs, "[Ljava/lang/Object;"),
            Some(true)
        );
        assert_eq!(
            vh_array_coordinate_admits(c, ints, "[Ljava/lang/Object;"),
            Some(false)
        );
        assert_eq!(vh_array_coordinate_admits(c, ints, "[[I"), Some(false));
        assert_eq!(vh_array_coordinate_admits(c, s, "[I"), Some(false));
        assert_eq!(vh_array_coordinate_admits(c, ints, "I"), None);
    }
}

/// Round 15 wave 3 (lane vh4; VH3-2,
/// `r15w3-vh3-minted-handle-index-coordinate-kinds-FIXED-20260930.md`): the index
/// coordinate of the VM's array and byte-view handles is converted by the
/// call site's static type, and the byte views' values are adapted. Probe
/// twins: `R15Vh3MintedCoordKinds`, `R15Vh4ViewValueKinds`.
#[cfg(test)]
mod r15w3_vh4_index_coordinate_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::poly_call_site::{arm_vh_param_site, vh_param_site_classes};
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        }
    }

    fn boxed(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = obj(ctx, class);
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn handle(ctx: &mut MockNativeContext, kind: i32, class_name: &str, desc: &str) -> Value {
        let vh = obj(ctx, "java/lang/invoke/VarHandle");
        vh_meta_put(
            ctx,
            vh,
            VarHandleMeta {
                kind,
                class_name: class_name.to_string(),
                field_name: String::new(),
                field_desc: desc.to_string(),
                field_index: -1,
                class_id: 0,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        Value::Object(Some(vh))
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    fn runtime(err: &MethodCallFailed) -> Option<&RuntimeError> {
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(r)) => Some(r),
            _ => None,
        }
    }

    fn int_of(v: MethodCallResult, ctx: &MockNativeContext) -> i32 {
        match v.unwrap() {
            Some(Value::Int(i)) => i,
            // `get` boxes: the payload of the canonical `Integer`.
            Some(Value::Object(Some(b))) => match ctx.get_field(b, 0) {
                Value::Int(i) => i,
                other => panic!("box payload {other:?}"),
            },
            other => panic!("answered {other:?}"),
        }
    }

    /// The probe's `arr*` roads on the VM's own `int[]` handle: a `long` index
    /// is WMTE (and stores nothing), an `Integer` through an `Object` site
    /// reads its element, and an armed `long`/`boolean` site class refuses even
    /// an `Int` representation while a `short` one reads.
    #[test]
    fn an_array_handles_index_converts_by_the_site_type() {
        if !vh_index_coordinate_kinds_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        let mut ctx = MockNativeContext::new();
        // Its own canonical-box row (see `a_reference_index_follows_unbox_widen`;
        // interpreter round i2 wave 59, lane L4).
        ctx.set_vm_identity(0x15F5_0004);
        let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
        let arr = ctx.new_array(ArrayElementType::Int, 8);
        for i in 0..8 {
            ctx.set_array_element(arr, i, Value::Int(100 + i as i32));
        }
        let a = Value::Object(Some(arr));
        let err = varhandle_get(&mut ctx, &[av, a, Value::Long(3)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long index: {err:?}");
        let three = boxed(&mut ctx, "java/lang/Integer", Value::Int(3));
        assert_eq!(int_of(varhandle_get(&mut ctx, &[av, a, three]), &ctx), 103);
        let err = varhandle_set(&mut ctx, &[av, a, Value::Long(5), Value::Int(-1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long index store: {err:?}");
        assert_eq!(
            ctx.get_array_element(arr, 5),
            Value::Int(105),
            "no refused store landed"
        );
        // A boxed index with a negative value is bounds-checked as itself.
        let minus = boxed(&mut ctx, "java/lang/Integer", Value::Int(-1));
        let err = varhandle_get(&mut ctx, &[av, a, minus]).unwrap_err();
        assert!(
            matches!(
                runtime(&err),
                Some(RuntimeError::ArrayIndexOutOfBoundsException { .. })
            ),
            "boxed -1: {err:?}"
        );
        // Armed word: `([IJ)I` refuses an `Int` too; `([IS)I` reads.
        arm_vh_param_site(vh_param_site_classes("([IJ)I"));
        let err = varhandle_get(&mut ctx, &[av, a, Value::Int(2)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "armed long site: {err:?}");
        arm_vh_param_site(vh_param_site_classes("([IZ)I"));
        let err = varhandle_get(&mut ctx, &[av, a, Value::Int(1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "armed boolean site: {err:?}");
        arm_vh_param_site(vh_param_site_classes("([IS)I"));
        assert_eq!(
            int_of(varhandle_get(&mut ctx, &[av, a, Value::Int(2)]), &ctx),
            102
        );
        // `compareAndSet` counts two values behind the index: `([IJII)Z`.
        arm_vh_param_site(vh_param_site_classes("([IJII)Z"));
        let err = varhandle_compare_and_set(
            &mut ctx,
            &[av, a, Value::Int(4), Value::Int(104), Value::Int(9)],
        )
        .unwrap_err();
        assert!(is_wmte(&ctx, &err), "armed long site, CAS: {err:?}");
        assert_eq!(ctx.get_array_element(arr, 4), Value::Int(104));
    }

    /// Interpreter round i2 wave 57, lane L4: the word's return byte
    /// (`vh_param_site_word`) is `asType`'s return direction. An `int`
    /// variable reads at a `long` site and refuses a `short` one; a `long`
    /// variable refuses an `int` site, and a refused `getAndAdd` adds nothing.
    #[test]
    fn a_primitive_return_site_the_variable_does_not_widen_to_is_refused() {
        use cratonvm_native_api::poly_call_site::vh_param_site_word;
        if !crate::nbflags().vh_primitive_return_astype {
            return; // the switch is off in this environment
        }
        let mut ctx = MockNativeContext::new();
        // `get` answers the CANONICAL box from the process-wide cache keyed by
        // `vm_identity()`: under the mock default `0` a full parallel lib run
        // served a box another test's (dropped) mock heap had filed, `invalid
        // ObjectRef in mock heap` in `int_of` (host `--release` run, waves 57
        // and 58). Its own row, as `a_reference_index_follows_unbox_widen`
        // does (interpreter round i2 wave 59, lane L4).
        ctx.set_vm_identity(0x15F5_0057);
        let iv = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
        let ints = ctx.new_array(ArrayElementType::Int, 4);
        ctx.set_array_element(ints, 1, Value::Int(7));
        let a = Value::Object(Some(ints));
        arm_vh_param_site(vh_param_site_word("([II)J"));
        assert_eq!(
            int_of(varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]), &ctx),
            7
        );
        arm_vh_param_site(vh_param_site_word("([II)I"));
        assert_eq!(
            int_of(varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]), &ctx),
            7
        );
        arm_vh_param_site(vh_param_site_word("([II)S"));
        let err = varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "int read at a short site: {err:?}");
        let lv = handle(&mut ctx, VH_KIND_ARRAY, "[J", "J");
        let longs = ctx.new_array(ArrayElementType::Long, 4);
        ctx.set_array_element(longs, 2, Value::Long(40));
        let l = Value::Object(Some(longs));
        arm_vh_param_site(vh_param_site_word("([JI)I"));
        let err = varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long read at an int site: {err:?}");
        arm_vh_param_site(vh_param_site_word("([JII)I"));
        let err =
            varhandle_get_and_add(&mut ctx, &[lv, l, Value::Int(2), Value::Int(1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "getAndAdd at an int site: {err:?}");
        assert_eq!(
            ctx.get_array_element(longs, 2),
            Value::Long(40),
            "nothing added"
        );
        // An `Object` site carries no return byte: not judged here.
        arm_vh_param_site(vh_param_site_word("([JI)Ljava/lang/Object;"));
        assert!(varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]).is_ok());
        // A reference site takes a primitive variable whose wrapper it is
        // assignable from: `Number`, `Long`; not `Integer`, `String`, `int[]`.
        for (site, ok) in [
            ("([JI)Ljava/lang/Number;", true),
            ("([JI)Ljava/lang/Long;", true),
            ("([JI)Ljava/lang/constant/ConstantDesc;", true),
            ("([JI)Ljava/lang/Integer;", false),
            ("([JI)Ljava/lang/String;", false),
            ("([JI)[J", false),
        ] {
            arm_vh_param_site(vh_param_site_word(site));
            let r = varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]);
            match r {
                Ok(_) => assert!(ok, "{site}: answered"),
                Err(e) => assert!(!ok && is_wmte(&ctx, &e), "{site}: {e:?}"),
            }
        }
    }

    /// A null index box is NPE, a `Long` box is CCE (`unboxWiden` refuses a
    /// narrowing wrapper), and a `String`-typed site is WMTE statically.
    #[test]
    fn a_reference_index_follows_unbox_widen() {
        if !vh_index_coordinate_kinds_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        // `get` answers the CANONICAL `Integer` box of the element, from the
        // process-wide cache keyed by `vm_identity()`; the mock default `0` is
        // shared with every other test, whose dropped mock heaps leave boxes
        // this heap does not hold (round 15 wave 6, lane mh5: `invalid
        // ObjectRef in mock heap` once wave 5's boxing test filled the row).
        ctx.set_vm_identity(0x15F5_0003);
        let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
        let a = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 4)));
        let err = varhandle_get(&mut ctx, &[av, a, Value::Object(None)]).unwrap_err();
        assert!(
            matches!(
                runtime(&err),
                Some(RuntimeError::NullPointerException { .. })
            ),
            "null index: {err:?}"
        );
        let long = boxed(&mut ctx, "java/lang/Long", Value::Long(1));
        let err = varhandle_get(&mut ctx, &[av, a, long]).unwrap_err();
        assert!(
            matches!(runtime(&err), Some(RuntimeError::ClassCastException { .. })),
            "Long index: {err:?}"
        );
        let short = boxed(&mut ctx, "java/lang/Short", Value::Int(2));
        assert_eq!(int_of(varhandle_get(&mut ctx, &[av, a, short]), &ctx), 0);
        arm_vh_param_site(vh_param_site_classes("([ILjava/lang/String;)I"));
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let err = varhandle_get(&mut ctx, &[av, a, s]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "String-typed index: {err:?}");
    }

    /// The probe's `bv*` roads: a byte view read with an `Integer` index reads
    /// at THAT offset (it read offset 0), a `long` index is WMTE, and a store
    /// with a boxed index writes at its offset.
    #[test]
    fn a_byte_views_index_is_the_real_offset() {
        if !vh_index_coordinate_kinds_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x15F5_0005); // its own canonical-box row (wave 59)
        let bv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
        let bytes = ctx.new_array(ArrayElementType::Byte, 16);
        ctx.set_array_element(bytes, 8, Value::Int(0x2a));
        let b = Value::Object(Some(bytes));
        let eight = boxed(&mut ctx, "java/lang/Integer", Value::Int(8));
        assert_eq!(int_of(varhandle_get(&mut ctx, &[bv, b, eight]), &ctx), 0x2a);
        let err = varhandle_get(&mut ctx, &[bv, b, Value::Long(8)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long offset: {err:?}");
        let four = boxed(&mut ctx, "java/lang/Integer", Value::Int(4));
        varhandle_set(&mut ctx, &[bv, b, four, Value::Int(0x0102_0304)]).unwrap();
        assert_eq!(ctx.get_array_element(bytes, 4), Value::Int(0x04));
        assert_eq!(ctx.get_array_element(bytes, 7), Value::Int(0x01));
        assert_eq!(
            ctx.get_array_element(bytes, 0),
            Value::Int(0),
            "offset 0 untouched"
        );
        let err = varhandle_set(&mut ctx, &[bv, b, Value::Long(0), Value::Int(9)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long offset store: {err:?}");
        assert_eq!(
            ctx.get_array_element(bytes, 0),
            Value::Int(0),
            "no refused store landed"
        );
    }

    /// `CRATONVM_VH_VIEW_VALUE_ADAPT`: an `int` into a `float` view stores
    /// `5.0f`, a boxed `Integer` into an `int` view stores its value, and a
    /// `long` from an armed `long` site into an `int` view is WMTE.
    #[test]
    fn a_byte_views_value_is_adapted() {
        if !vh_view_value_adapt_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let fv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "F");
        let bytes = ctx.new_array(ArrayElementType::Byte, 8);
        let b = Value::Object(Some(bytes));
        varhandle_set(&mut ctx, &[fv, b, Value::Int(0), Value::Int(5)]).unwrap();
        let stored = byte_view_get(&ctx, bytes, 0, b'F', true);
        assert_eq!(stored, Value::Float(5.0), "int widens to float");
        let iv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
        let seven = boxed(&mut ctx, "java/lang/Integer", Value::Int(7));
        varhandle_set(&mut ctx, &[iv, b, Value::Int(4), seven]).unwrap();
        assert_eq!(byte_view_get(&ctx, bytes, 4, b'I', true), Value::Int(7));
        arm_vh_param_site(vh_param_site_classes("([BIJ)V"));
        let err = varhandle_set(&mut ctx, &[iv, b, Value::Int(4), Value::Long(9)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "long into an int view: {err:?}");
        assert_eq!(byte_view_get(&ctx, bytes, 4, b'I', true), Value::Int(7));
    }
}

/// Read the VH_FIELD_DESC string from a VarHandle object. Prefers the
/// WP4.2 side table (see vh_meta_table comment) and falls back to the
/// raw object slot for VarHandles allocated outside our path.
pub(crate) fn vh_field_desc(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Cow<'static, str> {
    if let Some(m) = vh_meta_get(ctx, vh) {
        return Cow::Owned(m.field_desc.clone());
    }
    match vh_read_string(ctx, vh, VH_FIELD_DESC) {
        Some(s) => Cow::Owned(s),
        None => Cow::Borrowed(DESC_OBJECT),
    }
}

/// Convert a VarHandle field descriptor to the single-char type descriptor
/// that `box_value` expects (e.g. "I", "J", "Ljava/lang/Object;").
pub(crate) fn vh_type_desc(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Cow<'static, str> {
    let desc = vh_field_desc(ctx, vh);
    if desc.len() == 1 {
        desc
    } else {
        Cow::Borrowed(DESC_REF)
    }
}

/// Round-7 HIGH-2 fix: caller-side variant of `vh_type_desc` that consumes
/// an already-fetched `&VarHandleMeta` instead of re-locking the side
/// table. Hot natives (`varhandle_get`/`_set`/`_compare_and_set`) call
/// `vh_meta_get` exactly once and pass the bound `Arc` down to here, so a
/// single VH op no longer pays 2–3 mutex traversals.
///
/// The single-character arm answers with a `&'static str` rather than an owned
/// copy: it used to `clone()` the field descriptor, i.e. heap-allocate and
/// free a one-byte `String` on **every** `VarHandle` get/set/CAS. The eight
/// primitive descriptors are a closed set, so there is nothing to own.
pub(crate) fn vh_type_desc_from_meta(meta: &VarHandleMeta) -> Cow<'static, str> {
    Cow::Borrowed(match meta.field_desc.as_bytes() {
        [b'I'] => "I",
        [b'J'] => "J",
        [b'F'] => "F",
        [b'D'] => "D",
        [b'Z'] => "Z",
        [b'B'] => "B",
        [b'S'] => "S",
        [b'C'] => "C",
        [b'V'] => "V",
        _ => DESC_REF,
    })
}

// ---------------------------------------------------------------------------
// VarHandle synthetic field layout (6 fields)
// ---------------------------------------------------------------------------
/// VarHandle kind
pub(crate) const VH_KIND: usize = 0; // i32: 0=instanceField, 1=staticField, 2=array
/// Target class name (JVM internal, e.g. "java/lang/Foo")
pub(crate) const VH_CLASS: usize = 1; // Object(String)
/// Field name
pub(crate) const VH_FIELD: usize = 2; // Object(String)
/// Field descriptor (e.g. "I", "J", "Ljava/lang/String;")
pub(crate) const VH_FIELD_DESC: usize = 3; // Object(String)
/// Resolved field index (slot in the object's field array)
pub(crate) const VH_FIELD_INDEX: usize = 4; // Int
/// Cached ClassId of the declaring class
pub(crate) const VH_CLASS_ID: usize = 5; // Int (ClassId raw)
pub(crate) const VH_FIELD_COUNT: usize = 6;

// --- SHARED SLOT MAP: read this before adding a slot ------------------------
//
// `java/lang/invoke/VarHandle` is written by TWO files, and the slot numbers
// above are only half the map. `phases_late/reflect_invoke.rs`
// (`register_p59_varhandle`, synthetic-JDK only) imposes its own 3-slot
// meaning on 0-2 and used to put its two describe-yourself `Class` mirrors at
// 4 and 5 — the SAME slots as `VH_FIELD_INDEX` / `VH_CLASS_ID` here, with
// incompatible types (`Object(Class)` vs `Int`).
//
// That collision never corrupted anything, because every read on both sides
// is a tag match (`Value::Object(Some(_))` there, `Value::Int(_)` here) and
// the heap stores tagged 16-byte cells that the collector walks by tag — a
// cross-write degrades an answer to "unknown", it does not produce a
// mis-typed value or a scanned-as-oop integer. It was still a landmine: the
// two writers were one changed guard away from meeting on one object.
//
// So the two describe-yourself slots now live HERE, past everything this file
// uses, and `reflect_invoke.rs` imports them instead of declaring its own.
// One block, one map, and a `usize` that cannot silently mean two things.
/// Slot 6 (`reflect_invoke.rs` only): the `Class` mirror of the variable the
/// handle accesses — exactly what `varType()` must return.
pub(crate) const VH_META_VAR_TYPE: usize = 6; // Object(Class)
/// Slot 7 (`reflect_invoke.rs` only): the `Class` mirror of the handle's
/// LEADING coordinate; `null` for a static-field handle, which has none.
pub(crate) const VH_META_COORD0: usize = 7; // Object(Class)
/// Allocation size for a VarHandle that carries the two slots above.
/// (`reflect_invoke.rs` re-publishes this as its `VH_META_NUM_FIELDS`; the
/// name differs here only so the two do not collide in `lib.rs`, which
/// glob-imports `lang_invoke::*`.)
pub(crate) const VH_META_SLOT_COUNT: usize = 8;

pub(crate) const VH_KIND_INSTANCE: i32 = 0;
pub(crate) const VH_KIND_STATIC: i32 = 1;
pub(crate) const VH_KIND_ARRAY: i32 = 2;
// Byte-array-view VarHandle (`MethodHandles.byteArrayViewVarHandle(<T>[].class,
// order)`): views a `byte[]` as a wider primitive at a BYTE index, with the
// element descriptor carried in `field_desc` ("J"/"I"/"S"/"C"/"D"/"F"). The
// little/big-endian variants are distinct kinds so the byte order is encoded
// without growing `VarHandleMeta`. See `byte_view_{get,set}`.
pub(crate) const VH_KIND_BYTE_VIEW_LE: i32 = 3;
pub(crate) const VH_KIND_BYTE_VIEW_BE: i32 = 4;
// ByteBuffer-view VarHandle (`MethodHandles.byteBufferViewVarHandle`). Same
// element/endianness metadata as byte-array views, but coordinates are
// `(ByteBuffer, byteIndex)` and direct buffers must hit native memory.
pub(crate) const VH_KIND_BYTE_BUFFER_VIEW_LE: i32 = 5;
pub(crate) const VH_KIND_BYTE_BUFFER_VIEW_BE: i32 = 6;
// An FFM LAYOUT VarHandle (`ValueLayout.JAVA_INT.varHandle()`,
// `layout.varHandle(PathElement...)`), whose coordinates are
// `(MemorySegment, long[, long])`. Reported by the describe-yourself pair;
// this kind is never written into a slot, because the handle's meaning lives
// in `P67_MEMORY_SEGMENT_VH_TABLE` (see `SegmentVhShape`). Deliberately NOT
// `3`: `phases_late/reflect_invoke.rs` numbers ITS memory-segment kind 3 under
// a different slot map, and 3 already means `VH_KIND_BYTE_VIEW_LE` here.
pub(crate) const VH_KIND_MEMORY_SEGMENT_LAYOUT: i32 = 7;

// ---------------------------------------------------------------------------
// WP4.2 — VarHandle metadata side table
// ---------------------------------------------------------------------------
//
// The synthetic VarHandle fields above are written into the storage slots of
// whatever class `alloc_concurrent_synthetic` resolves for "java/lang/invoke
// /VarHandle". In real-JDK mode, that's the real `VarHandle` class — whose
// slot 0 (`vform`) is declared `Ljava/lang/invoke/VarForm;`. The descriptor-
// aware setter `Heap::set_field_as` coerces our `Value::Int(VH_KIND_INSTANCE
// = 0)` into `Value::Object(None)` (line 898 of `gc/src/heap.rs`), and the
// next `get_field` returns `Object(None)` instead of `Int(0)`. The kind tag,
// the field index, the class name, and the field name are then all unread-
// able and the VarHandle silently no-ops.
//
// To keep the synthetic VarHandle working without a heavy rewrite, we keep a
// side table keyed by the VarHandle's weak lock key (see `VhMetaTable`
// below; it was the heap pointer until Round 9, the bare hash until
// gc-common w18-b, `(VM, hash)` until w29-b). The slot
// writes in `alloc_*_var_handle` still happen (for compatibility with any
// caller that does happen to read them — all of `varhandle_get`, `_set`,
// `_compare_and_set` consult the side table first and fall back to slot
// reads), but the canonical truth lives here.

#[derive(Clone, Debug)]
pub(crate) struct VarHandleMeta {
    pub kind: i32,
    pub class_name: String,
    pub field_name: String,
    pub field_desc: String,
    pub field_index: i32,
    pub class_id: u32,
    /// The variable is `final`, so every WRITE access mode on this handle is
    /// unsupported.
    ///
    /// A property of the HANDLE, not of the variable's type, which is why it
    /// lives here rather than being derived at access time: deriving it would
    /// mean a `declared_fields` walk (a `Vec<FieldMetadata>` of owned
    /// `String`s) on every `set`, and `set` is 698 000 calls on one probe.
    /// Decided once, where the field is already being resolved.
    pub read_only: bool,
    /// `Some` when the handle has invoke-exact behaviour
    /// (`withInvokeExactBehavior()`, `--jdk-only`, interpreter round i1 wave
    /// 37, lane L4): an access whose call-site type is not exactly its access
    /// mode type is a `WrongMethodTypeException`. Holds the five access mode
    /// descriptors, by `VarHandle.AccessType` ordinal
    /// ([`vh_access_mode_descriptor`]), computed once when the exact copy is
    /// minted so the per-access check allocates nothing; an EMPTY entry (a
    /// handle that cannot describe its coordinates exactly) accepts.
    ///
    /// A property of the HANDLE: the exact copy is a distinct object whose row
    /// differs from its plain twin's only here, and whose real `exact` field
    /// is set as well (`hasInvokeExactBehavior()` is JDK bytecode reading it,
    /// and the doors gate on it). Judged by
    /// [`varhandle_exact_call_site_refusal`].
    pub exact: Option<Arc<[String; 5]>>,
    /// For an INSTANCE handle over a reference field declared as a class or
    /// interface other than `java/lang/Object`: that type's class id, as the
    /// same flat `class_id_by_name` lookup `reference_arg_admitted` uses
    /// resolved it when the handle was made. For a field declared as a
    /// one-dimensional reference array (`[Lname;`), the COMPONENT `name`'s
    /// class id instead ([`VarHandleInstanceFieldPlan::ref_array_component_class_id`]
    /// is where the plan exposes it). `0` for every other handle, and for a
    /// type the lookup could not resolve.
    ///
    /// Exists for the JIT's thin write/CAS helpers, which must not store a
    /// value the native's `vh_instance_refusal` would refuse, and cannot take
    /// a by-name lookup per access. See
    /// [`VarHandleInstanceFieldPlan::ref_type_class_id`].
    pub ref_type_class_id: u32,
    /// For an INSTANCE handle: the class its field was resolved in (the class
    /// `findVarHandle` was asked for, or the `Field`'s declaring class for
    /// `unreflectVarHandle`), kept apart from the coordinate (`class_name` /
    /// `class_id`), which is the LOOKUP class for a protected field of another
    /// package reached through a subclass's lookup. The by-name readers (the
    /// natives' late re-resolution, [`vh_instance_field_by_name`], and
    /// `toMethodHandle`'s accessor) look the field up here, so a lookup class
    /// that declares a HIDING field of the same name no longer changes which
    /// field the handle names. `0` for every other handle and when unknown
    /// (the readers then use `class_name`, as before). Interpreter round i1
    /// wave 46, lane L4
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub holder_class_id: u32,
}

// `VarHandleMeta` is wrapped in `Arc<>` so the hot `get`/`set`/`CAS`
// natives (which can pull the meta 1–3 times per op) only pay a refcount
// bump under the Mutex instead of a full clone of three owned `String`s.
// `vh_meta_get` returns `Option<Arc<VarHandleMeta>>` — callers destructure
// the inner fields via `Arc::as_ref()` and copy the few primitive members
// they actually need (`kind`, `field_index`, `class_id`).
// Round-7 HIGH-4 fix: migrated from `std::sync::Mutex` to
// `parking_lot::Mutex` to drop pthread / poisoning overhead and align
// with the rest of the project.  Every VarHandle `get`/`set`/`CAS` op
// hits this table, so even a small per-op saving compounds.
// Round-9 CRIT GC-correctness fix: keys must survive moving GC.
// Previously this table was keyed by `vh.as_ptr() as usize` — when the GC
// relocated the VarHandle during compaction the entry became orphaned and
// every subsequent `varhandle_get/_set/_compare_and_set` silently fell
// back to slot reads (which return Object(None) for our synthetic layout
// on real-JDK VarHandle, see WP4.2 comment above).
//
// The fix: use `NativeContext::identity_hash_code(vh)` as the key.
// `identity_hash_code` is GC-stable because it lives in the object's MARK
// WORD (`ObjectHeader::mark_word_identity_hash`), and every mover copies the
// header verbatim. NOT, as this said until 2026-08-30, because
// `HashCodeTable::update_after_gc` remaps it: that table has no production
// consumer at all (see its own doc comment). All meta accessors therefore
// take `&mut dyn NativeContext` so they can compute the key.
//
// `vh_meta_put` below registers every VarHandle as a PERMANENT GC root, so
// no VarHandle is normally reclaimed; the entries accumulate, but the root is
// what retains them and the root is load-bearing (B-J) — a separate question
// about VarHandle lifetime. (The ZGC-only identity-hash eviction channel this
// used to explain its absence from is gone, gc-common w29-b.)
//
// gc-common w18-b (`common-w15e-varhandle-meta-tables-keyed-by-bare-identity-
// hash`): the key leads with the VM. Every heap mints identity hashes from 1,
// so two VMs in one process hand the same hash to different handles, and the
// bare-hash key let VM B's `vh_meta_put` overwrite VM A's row -- A's `get` /
// `set` / CAS then read and wrote B's field slot on A's receiver. A row now
// answers only the VM that filed it, and `forget_vm_var_handle_meta` drops a
// VM's rows at teardown.
//
// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
// rank 4, the same-VM half): a row is filed under the VarHandle's WEAK LOCK
// KEY (`crate::gc_stable_weak_lock_key`), not `(VM, identity hash)`. Within
// one VM two live objects share an identity hash once the heap's counter
// wraps, and VarHandle B's `vh_meta_put` overwrote A's row: A's `get` / `set`
// / CAS then addressed B's field slot on A's receiver (a corrupting store),
// and a real-JDK VarHandle that was never registered but shared a registered
// one's hash was served that one's meta. The weak key is unique per live
// object and move-stable (the registry is re-addressed in the stop-the-world
// epilogue), and it is freed if the handle ever dies
// ([`forget_var_handle_meta_keys`]) -- a VarHandle is a permanent root today,
// but that root is itself keyed by identity hash and a collider can unroot
// it (`var_handle_roots`, census rank 2).
//
// gc-common w36-a (`common-w15e-...` JIT half, `common-w33a-...` site 5): the
// JIT names the HANDLE too. Its three VarHandle fast paths
// (`vm/src/jit/helpers.rs`) pass the VM, the handle's identity hash and its
// CURRENT address to [`varhandle_instance_field_plan_for_handle`], which
// resolves the handle's weak lock key exactly as [`crate::existing_weak_lock_key`]
// does (the VM's registry slot at that address) and reads the row under it.
// The hash index (`by_hash`), the per-VM row count and the VM-less / hash-only
// lookups (`sole_row`, `sole_vm`, `varhandle_instance_field_plan[_for_vm]`)
// are gone: an unregistered handle that shares a registered one's hash (a
// compact instance's hash repeats every 2^20 mints per class, `docs/GC.md`)
// has no registry slot at its own address, so it gets no plan.

/// One VarHandle's meta row: the VM that filed it (for per-VM teardown), and
/// the meta.
pub(crate) struct VhMetaRow {
    pub(crate) vm: usize,
    pub(crate) meta: Arc<VarHandleMeta>,
}

/// The VarHandle meta rows, keyed by the handle's weak lock key.
#[derive(Default)]
pub(crate) struct VhMetaTable {
    pub(crate) rows: rustc_hash::FxHashMap<usize, VhMetaRow>,
}

impl VhMetaTable {
    /// The row of the handle whose weak lock key is `key`.
    pub(crate) fn get(&self, key: usize) -> Option<&Arc<VarHandleMeta>> {
        self.rows.get(&key).map(|row| &row.meta)
    }

    /// File (or refile) the row of the handle keyed `key`. A key names one
    /// object of one VM, so a refile keeps its VM.
    pub(crate) fn insert(&mut self, vm: usize, key: usize, meta: Arc<VarHandleMeta>) {
        match self.rows.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().meta = meta,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(VhMetaRow { vm, meta });
            }
        }
    }

    /// Drop the row keyed `key` (its handle died, or a test cleans up).
    pub(crate) fn remove(&mut self, key: usize) -> Option<Arc<VarHandleMeta>> {
        self.rows.remove(&key).map(|row| row.meta)
    }

    /// Drop every row `vm` filed; answers how many went.
    pub(crate) fn forget_vm(&mut self, vm: usize) -> usize {
        let before = self.rows.len();
        self.rows.retain(|_, row| row.vm != vm);
        before - self.rows.len()
    }
}

/// The weak lock key `vm`'s registry holds for the object with identity hash
/// `identity_hash` at `addr`, WITHOUT minting one: the match
/// [`crate::existing_weak_lock_key`] makes, for a caller that has no
/// `NativeContext` but already has the three facts (the JIT's VarHandle fast
/// paths, gc-common w36-a). Reads the registry shard holding `hash` directly
/// (`lock_key_shards::lock_key_shard`, gcg g3/k) rather than adding a
/// `lib.rs` twin of `existing_weak_lock_key`.
///
/// `addr` must be the object's CURRENT address: the stop-the-world epilogue
/// re-addresses every live slot, so a moved handle is found at its new
/// address and never at its old one (a stale address misses, which declines).
pub(crate) fn existing_weak_lock_key_at(vm: usize, identity_hash: i32, addr: usize) -> Option<usize> {
    let hash = identity_hash as u32;
    let reg = crate::lock_key_shards::lock_key_shard(hash).lock();
    reg.buckets
        .get(&hash)?
        .iter()
        .find(|s| s.vm == vm && s.last_ptr == addr)
        .map(|slot| crate::pack_lock_key(hash, slot.generation))
}

pub(crate) static VH_META_TABLE: std::sync::OnceLock<parking_lot::Mutex<VhMetaTable>> =
    std::sync::OnceLock::new();

pub(crate) fn vh_meta_table() -> &'static parking_lot::Mutex<VhMetaTable> {
    VH_META_TABLE.get_or_init(|| parking_lot::Mutex::new(VhMetaTable::default()))
}

pub(crate) fn vh_meta_put(ctx: &mut dyn NativeContext, vh: ObjectRef, meta: VarHandleMeta) {
    // B-J: register every VarHandle as a permanent GC root. Every VarHandle
    // creation path (alloc_static_var_handle / alloc_instance_var_handle /
    // findVarHandle) funnels through here, so this one call covers them all.
    // Without it a moving GC reclaimed VarHandles held only by `static final`
    // fields, leaving those slots with all-zero headers → `VarHandle.set`
    // misdispatch and apparent heap corruption (kafka consumer crashes).
    ctx.register_var_handle_root(vh);
    let vm = ctx.vm_identity();
    let key = vh_lock_key(ctx, vh);
    let resolved_instance =
        meta.kind == VH_KIND_INSTANCE && meta.field_index >= 0 && meta.exact.is_none();
    let mut t = vh_meta_table().lock();
    t.insert(vm, key, Arc::new(meta));
    drop(t);
    // After the insert, so no reader can memoise the pre-insert answer against
    // the post-insert generation.
    vh_meta_bump_generation();
    // A genuine JDK handle is adopted on first access with its slot already
    // resolved; it never visits vh_meta_update_field_index. Publish the same
    // readiness event, after dropping the metadata lock, for that transition.
    if resolved_instance {
        ctx.note_varhandle_field_resolution();
    }
}

/// The weak lock key a VarHandle's meta row (and FFM layout shape) is filed
/// under, minting it on first use (gc-common w29-b). For the registration
/// paths; lookups use [`crate::existing_weak_lock_key`], which never mints.
pub(crate) fn vh_lock_key(ctx: &dyn NativeContext, vh: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, vh)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// Serve `vh`'s entry of a per-thread memo, or compute it with `lookup` (which
/// is handed `vh`'s weak lock key, or `None` when `vh` was never registered)
/// and memoise the answer.
///
/// A line is keyed by `(vm, identity hash, ADDRESS, generation)`. The address
/// is what makes a hit mean "this very object" (gc-common w29-b): the hash
/// alone named every live collider. An address can be reused by another
/// object only after the collection that moved or freed the first one, and
/// [`VH_META_GENERATION`] moves on every moving collection
/// ([`note_objects_moved`], from the lock-key sweep of the stop-the-world
/// epilogue) and on every change to the tables -- a registered handle freed in
/// place (it lost its permanent root) has its key freed and its rows dropped
/// by [`forget_var_handle_meta_keys`], which moves it too; an unregistered
/// one's negative line can at worst be answered for another unregistered
/// object.
pub(crate) fn vh_memo_lookup<T: Clone + 'static>(
    memo: &'static std::thread::LocalKey<std::cell::RefCell<Vec<VhMemoLine<T>>>>,
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    lookup: impl FnOnce(Option<usize>) -> Option<T>,
) -> Option<T> {
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(vh);
    let addr = vh.as_ptr() as usize;
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = memo.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        (line.vm == vm && line.key == hash && line.addr == addr && line.generation == generation)
            .then(|| line.value.clone())
    });
    if let Some(value) = hit {
        return value;
    }
    let value = lookup(crate::existing_weak_lock_key(ctx, vh));
    memo.with(|memo| {
        memo.borrow_mut()[slot] = VhMemoLine {
            vm,
            key: hash,
            addr,
            generation,
            value: value.clone(),
        };
    });
    value
}

pub(crate) fn vh_meta_get(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> Option<Arc<VarHandleMeta>> {
    let found = vh_memo_lookup(&VH_META_MEMO, &*ctx, vh, |key| {
        let key = key?;
        // Refcount bump only — no per-field String clone.
        vh_meta_table().lock().get(key).cloned()
    });
    if found.is_none() && vh_real_adoption_enabled() {
        let adopted = vh_adopt_real_byte_view(ctx, vh)?;
        return vh_adopt_exact_flag(ctx, vh, adopted);
    }
    found
}

/// Whether a row-less `VarHandle` may be a REAL field / byte-view handle to
/// adopt: only when the JDK's own `Lookup.findVarHandle` body can have built
/// one (the family retired, or the pure lever). Otherwise every miss on a real
/// array handle paid a class-name allocation (+25..30% on `vh.array.get/set`,
/// measured SBR3 mh). A pure function of the static tables, read once.
pub(crate) fn vh_real_adoption_enabled() -> bool {
    // The unit tests drive `vh_meta_get` with mock handles that are not heap objects: with the
    // family listed in the retirement tables the adoption below would read fields of one.
    #[cfg(test)]
    {
        return false;
    }
    #[cfg(not(test))]
    vh_real_adoption_gate()
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn vh_real_adoption_gate() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_native_api::unretire::pure_bytecode()
            || cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
                "java/lang/invoke/MethodHandles$Lookup",
                "findVarHandle",
                "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
            )
    })
}

/// A REAL `VarHandle` the JDK's own bytecode built with invoke-exact behaviour
/// (`withInvokeExactBehavior()` on a real field / byte-view handle) carries its
/// `exact` field set, but the row [`vh_adopt_real_byte_view`] filed has no exact
/// access-mode types, so the doors accepted a mistyped call
/// (`handle's method type (X)int but found (Object)Object` never raised). File
/// the five types the carrier's own `withInvokeExactBehavior` computes (SBR3 lane mh).
pub(crate) fn vh_adopt_exact_flag(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    row: Arc<VarHandleMeta>,
) -> Option<Arc<VarHandleMeta>> {
    if !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0) {
        return Some(row);
    }
    let (_kind, value_desc, coords, coords_exact) = vh_value_and_coordinate_descriptors(ctx, vh);
    let types: Arc<[String; 5]> = Arc::new(std::array::from_fn(|access_type| {
        if coords_exact {
            vh_access_mode_descriptor(access_type as i32, &coords, &value_desc)
        } else {
            String::new()
        }
    }));
    let mut exact_row = (*row).clone();
    exact_row.exact = Some(types);
    vh_meta_put(ctx, vh, exact_row);
    let key = crate::existing_weak_lock_key(&*ctx, vh)?;
    vh_meta_table().lock().get(key).cloned()
}

/// A REAL `VarHandle<T>s$FieldInstance{ReadOnly,ReadWrite}` /
/// `$FieldStatic{ReadOnly,ReadWrite}` (what `Lookup.findVarHandle` /
/// `findStaticVarHandle` return when their own bytecode runs under
/// `CRATONVM_PURE_BYTECODE=1`) reaches a door with no side-table row. Its
/// `fieldOffset` is the VM's slot index (`Unsafe.objectFieldOffset` /
/// `staticFieldOffset` answer one), its `receiverType` / `base` the holder, so
/// the declared fields name the variable and the row a registered
/// `findVarHandle` would have filed can be filed here (SBR2 wave 10, F5).
pub(crate) fn vh_adopt_real_field_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    class_name: &str,
) -> Option<Arc<VarHandleMeta>> {
    // `VarHandles.makeFieldHandle` wraps the static-field handle of a class
    // that is not initialised yet in a `LazyInitializingVarHandle` (its
    // `target`); the access ensures the holder's `<clinit>` first, which the
    // static row's own access already does (`vh_static_slot`). Same row.
    if class_name == "java/lang/invoke/LazyInitializingVarHandle" {
        let target = match ctx.get_field_by_name(vh, "target") {
            Value::Object(Some(t)) => t,
            _ => return None,
        };
        let row = vh_meta_get(ctx, target)?;
        vh_meta_put(ctx, vh, (*row).clone());
        let key = crate::existing_weak_lock_key(&*ctx, vh)?;
        return vh_meta_table().lock().get(key).cloned();
    }
    let rest = class_name.strip_prefix("java/lang/invoke/VarHandle")?;
    let (family, shape) = rest.split_once('$')?;
    if !matches!(
        family,
        "Ints"
            | "Longs"
            | "Booleans"
            | "Bytes"
            | "Shorts"
            | "Chars"
            | "Floats"
            | "Doubles"
            | "References"
    ) {
        return None;
    }
    let (is_static, read_only) = match shape {
        "FieldInstanceReadOnly" => (false, true),
        "FieldInstanceReadWrite" => (false, false),
        "FieldStaticReadOnly" => (true, true),
        "FieldStaticReadWrite" => (true, false),
        _ => return None,
    };
    let offset = match ctx.get_field_by_name(vh, "fieldOffset") {
        Value::Long(v) if v >= 0 => v as usize,
        Value::Int(v) if v >= 0 => v as usize,
        _ => return None,
    };
    // A static's offset is the VM's synthetic one (`staticFieldOffset` files
    // `offset -> (class, static index)`); an instance's is its slot index.
    let (holder, slot) = if is_static {
        // gci cl48: against the handle's own `base` (the holder's mirror): a
        // same-named class of another loader shares the minted offset.
        let base = match ctx.get_field_by_name(vh, "base") {
            Value::Object(base) => base,
            _ => None,
        };
        crate::unsafe_natives_ext::unsafe_static_field_target_via_base(&*ctx, base, offset)?
    } else {
        let mirror = match ctx.get_field_by_name(vh, "receiverType") {
            Value::Object(Some(m)) => m,
            _ => return None,
        };
        // JIT round 20 wave 8c: the `Unsafe` offset back to its slot (a byte
        // offset under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`); a handle never
        // names the inside of a field.
        let (slot, k) = crate::unsafe_natives_ext::unsafe_decode_field_offset(
            ctx.unsafe_byte_field_offsets(),
            offset,
        );
        if k != 0 {
            return None;
        }
        (ctx.class_id_from_mirror(mirror)?, slot)
    };
    // A lambda proxy's capture (interpreter round i3 wave 80, lane L4; page
    // `i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack`,
    // `--jdk-only`): the proxy is no class-store class, so it has no name
    // there and `declared_fields` knows none of its `arg$N`; without this arm
    // no row was filed and `vh.get(s)` answered `null`. The row is the one the
    // `--compatible` finder files (`lk_lambda_proxy_var_handle`): the proxy's
    // internal name and id, which `vh_instance_refusal` casts receivers to.
    let (holder_name, proxy_declared) = match ctx.class_name_of_id(holder) {
        Some(n) => (n, None),
        None if !is_static => (
            crate::lang_class::lambda_internal_name(&*ctx, holder)?,
            Some(crate::lang_class::declared_fields_with_aliases(
                &*ctx, holder,
            )),
        ),
        None => return None,
    };
    let field = if let Some(declared) = proxy_declared {
        declared
            .into_iter()
            .find(|f| !f.is_static && f.slot_index == slot)?
    } else {
        // An instance field may be INHERITED (interpreter round i3 wave 79, lane
        // L4): `receiverType` is the class the lookup named (`findVarHandle(Derived,
        // "b", ..)`), and `b` is a field of its superclass, so the slot is searched
        // up the superclass chain (an instance slot index is absolute). Searching
        // `Derived`'s own fields only found nothing, no row was filed, and the
        // access read `null` (`L4W74InheritedFieldSlot` `varhandle-inherited-second`,
        // `--jdk-only`). A static's holder is its declaring class already.
        let mut search = Some(holder);
        let mut field = None;
        for _ in 0..64 {
            let Some(cid) = search else {
                break;
            };
            field = ctx.declared_fields(cid).into_iter().find(|f| {
                f.is_static == is_static
                    && if is_static {
                        ctx.static_field_index_by_name(cid, &f.name) == Some(slot)
                    } else {
                        f.slot_index == slot
                    }
            });
            if field.is_some() || is_static {
                break;
            }
            search = ctx.superclass_of(cid).filter(|&parent| parent != cid);
        }
        field?
    };
    let kind = if is_static {
        VH_KIND_STATIC
    } else {
        VH_KIND_INSTANCE
    };
    let ref_type_class_id = vh_ref_type_lookup_name(&field.descriptor)
        .or_else(|| vh_ref_array_component_lookup_name(&field.descriptor))
        .and_then(|n| ctx.class_id_by_name(n))
        .map_or(0, |cid| cid.as_u32());
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind,
            class_name: holder_name,
            field_name: field.name.clone(),
            field_desc: field.descriptor.clone(),
            // JIT round 20 wave 11 (lane huntDisp): the decoded SLOT, not the
            // raw `fieldOffset` (a byte offset under
            // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`: the row read `16 + 8 * slot`).
            field_index: if is_static { -1 } else { slot as i32 },
            class_id: holder.as_u32(),
            read_only,
            exact: None,
            ref_type_class_id,
            holder_class_id: field.declaring_class_id.as_u32(),
        },
    );
    let key = crate::existing_weak_lock_key(&*ctx, vh)?;
    vh_meta_table().lock().get(key).cloned()
}

/// A REAL `VarHandleByteArrayAs<T>s$ArrayHandle` / `$ByteBufferHandle` -- what
/// `MethodHandles.byteArrayViewVarHandle` / `byteBufferViewVarHandle` return
/// when their own bytecode runs (`CRATONVM_PURE_BYTECODE=1`, where the
/// registered factories that mint a tagged synthetic handle are not in front)
/// -- reaches an access door with no side-table row. File the row the
/// registered factory would have filed (kind from the real object's own `be`
/// field, element from its class name), so the doors' byte-view arms serve it.
/// Without it the door sees an untagged handle of array shape and reads the
/// `byte[]`'s own element type: `cannot convert int to byte` for a `short`/`int`
/// view's value (SBR2 wave 10, family F2).
pub(crate) fn vh_adopt_real_byte_view(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> Option<Arc<VarHandleMeta>> {
    let cid = ctx.class_id_of_object(vh);
    let name = ctx.class_name_of_id(cid)?;
    let Some(rest) = name.strip_prefix("java/lang/invoke/VarHandleByteArrayAs") else {
        return vh_adopt_real_field_handle(ctx, vh, &name);
    };
    let (family, shape) = rest.split_once('$')?;
    let elem = match family {
        "Shorts" => b'S',
        "Chars" => b'C',
        "Ints" => b'I',
        "Longs" => b'J',
        "Floats" => b'F',
        "Doubles" => b'D',
        _ => return None,
    };
    let buffer = match shape {
        "ArrayHandle" => false,
        "ByteBufferHandle" => true,
        _ => return None,
    };
    let be = match ctx.get_field_by_name(vh, "be") {
        Value::Int(v) => v != 0,
        _ => return None,
    };
    let kind = match (buffer, be) {
        (false, false) => VH_KIND_BYTE_VIEW_LE,
        (false, true) => VH_KIND_BYTE_VIEW_BE,
        (true, false) => VH_KIND_BYTE_BUFFER_VIEW_LE,
        (true, true) => VH_KIND_BYTE_BUFFER_VIEW_BE,
    };
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind,
            class_name: String::new(),
            field_name: String::new(),
            field_desc: (elem as char).to_string(),
            field_index: -1,
            class_id: 0,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    let key = crate::existing_weak_lock_key(&*ctx, vh)?;
    vh_meta_table().lock().get(key).cloned()
}

/// Drop the VarHandle meta rows and FFM layout-handle shapes filed under
/// `keys`, weak lock keys the registry has just freed (the handle died, or
/// its VM is going). Called by `lib.rs::forget_core_object_state_keys` with
/// the registry lock released; each table lock is a leaf.
pub(crate) fn forget_var_handle_meta_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut dropped = false;
    {
        let mut t = vh_meta_table().lock();
        if !t.rows.is_empty() {
            for k in keys {
                dropped |= t.remove(*k).is_some();
            }
        }
    }
    {
        let mut t = p67_memory_segment_vh_table().lock();
        if !t.is_empty() {
            for k in keys {
                dropped |= t.remove(k).is_some();
            }
        }
    }
    if dropped {
        // After the removal, as in `vh_meta_put`.
        vh_meta_bump_generation();
    }
}

/// A moving collection has just re-addressed the heap (its stop-the-world
/// epilogue, `lib.rs::sweep_lock_keys`): discard every thread's VarHandle
/// memo lines, which are keyed by address ([`vh_memo_lookup`]).
pub(crate) fn note_objects_moved() {
    vh_meta_bump_generation();
}

/// Per-VM teardown (gc-common w18-b): drop every VarHandle meta row and FFM
/// layout-handle shape `vm` filed. Idempotent.
///
/// VM identities are never recycled (`NEXT_VM_IDENTITY` counts up), so a dead
/// VM's rows could not be served to a later VM through the VM-aware readers;
/// what this prevents is the leak (a long-lived embedder that creates and
/// disposes of VMs).
pub fn forget_vm_var_handle_meta(vm: usize) {
    let dropped = vh_meta_table().lock().forget_vm(vm);
    p67_memory_segment_vh_table()
        .lock()
        .retain(|_, row| row.0 != vm);
    if dropped != 0 {
        // After the removal, for the same reason `vh_meta_put` bumps after
        // its insert: a memoised answer must not outlive the change.
        vh_meta_bump_generation();
    }
}

/// What a `VarHandle` access mode reduces to when the handle names an
/// ordinary INSTANCE field whose slot is already resolved: one field read at
/// `field_index`, decoded against `value_desc`.
///
/// Exposed so a caller that already holds the heap — the JIT's per-call-site
/// native fast path — can serve `VarHandle.get` as a field load instead of
/// entering the native funnel, allocating a wrapper for the erased `Object`
/// return, and unboxing it straight back out. See
/// `vm::jit::helpers::try_varhandle_instance_field_read` for the semantics it
/// is obliged to keep, and the refusals it makes instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarHandleInstanceFieldPlan {
    /// Slot in the receiver's field array.
    pub field_index: u32,
    /// Single-character value descriptor — `b'I'`, `b'J'`, `b'F'`, `b'D'`,
    /// `b'Z'`, `b'B'`, `b'S'`, `b'C'`, or `b'L'` standing for any reference
    /// (which is exactly the collapse `vh_type_desc_from_meta` performs
    /// before handing the descriptor to `box_value`).
    pub value_desc: u8,
    /// The handle's coordinate class (`VarHandleMeta::class_id`). The native
    /// refuses a receiver that is not an instance of it
    /// (`vh_instance_refusal` -> `ClassCastException`), so a fast path must
    /// decline any receiver it cannot prove is one. `0` when unknown.
    pub class_id: u32,
    /// The handle is over a `final` field: every WRITE mode is refused by the
    /// native (`vh_check_read_only`), so a write or CAS fast path must decline
    /// and let the native decide (it also owns the kill switch).
    pub read_only: bool,
    /// For `value_desc == b'L'`: `true` when the native judges no reference
    /// value stored into this field — a field declared `java/lang/Object`.
    /// An ARRAY-typed field is judged (`vh_array_field_value_admitted`), so it
    /// is never unchecked; until round 11 wave 4 it was, and the JIT's thin
    /// write helpers stored a `String` into an `int[]` field.
    pub ref_value_unchecked: bool,
    /// For a ONE-dimensional primitive array field (`[I`, `[B`, ...): the
    /// element type a stored value must have. `vh_array_field_value_admitted`
    /// admits exactly that array and nothing else, and the header's element
    /// tag answers it in one read, so a fast path can decide it locally.
    /// `None` for every other field; for another array-typed field a fast path
    /// declines every non-null value and the native decides.
    pub ref_array_element: Option<ArrayElementType>,
    /// For `value_desc == b'L'` and `!ref_value_unchecked`: the declared
    /// type's class id (`VarHandleMeta::ref_type_class_id`), `0` when it could
    /// not be resolved — a fast path then declines every non-null value.
    /// Always `0` for an array-typed field.
    pub ref_type_class_id: u32,
    /// For a ONE-dimensional reference array field (`[Ljava/lang/String;`):
    /// the COMPONENT class id, resolved by name when the handle was made, `0`
    /// when unresolved and for every other field (rt proposal W8-2).
    ///
    /// A CratonVM reference array's header carries its component class id, so
    /// a value whose header says `Reference` elements and exactly this
    /// component IS an array of the field's own type. `checkcast`'s
    /// predicate, which the native judges these stores with
    /// (`vh_array_field_value_admitted`), always admits that, so a fast path
    /// may admit it too. It admits a subtype component as well (`Sub[]` into
    /// a `Base[]` field, rt proposal W9-2), by the exact ClassId subtype
    /// relation; anything it cannot prove declines to the native.
    pub ref_array_component_class_id: u32,
}

/// The internal class name `reference_arg_admitted` would look up for a field
/// of descriptor `field_desc`: `Some` only for an `L...;` type other than
/// `java/lang/Object`, the one shape that predicate actually judges.
pub(crate) fn vh_ref_type_lookup_name(field_desc: &str) -> Option<&str> {
    let name = field_desc.strip_prefix('L')?.strip_suffix(';')?;
    (name != "java/lang/Object").then_some(name)
}

/// The component class name of a ONE-dimensional reference array field
/// descriptor (`[Ljava/lang/String;` -> `java/lang/String`), `None` for any
/// other descriptor. `java/lang/Object` is kept: an exact `Object[]` value is
/// still exactly the field's type.
pub(crate) fn vh_ref_array_component_lookup_name(field_desc: &str) -> Option<&str> {
    let name = field_desc.strip_prefix("[L")?.strip_suffix(';')?;
    (!name.is_empty()).then_some(name)
}

/// Look a [`VarHandleInstanceFieldPlan`] up by the VarHandle's weak lock key --
/// the same row [`vh_meta_get`] reads, so a handle this answers for is exactly
/// a handle `varhandle_get` would have served from the side table.
///
/// `None` for every shape the plan cannot describe, and each of those is a
/// case the funnel still has to run: a static-field handle, an array-element
/// or byte-array/ByteBuffer-view handle (all distinct `kind`s), a handle
/// whose field slot has not been resolved yet, and a `SegmentVarHandle`
/// (a real JDK class, never in this table at all).
pub(crate) fn varhandle_instance_field_plan_uncached(lock_key: usize) -> Option<VarHandleInstanceFieldPlan> {
    let table = vh_meta_table().lock();
    let meta = table.get(lock_key)?;
    // An invoke-exact handle has no plan (interpreter round i1 wave 49, lane
    // L4): the fast paths that read one (the JIT's thin `VarHandle` helpers,
    // which hold no call-site descriptor) cannot judge the site's exact type,
    // so every access of such a handle goes where it is judged.
    if meta.kind != VH_KIND_INSTANCE || meta.field_index < 0 || meta.exact.is_some() {
        return None;
    }
    let bytes = meta.field_desc.as_bytes();
    let value_desc = match bytes.first().copied()? {
        c @ (b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'S' | b'C') if bytes.len() == 1 => c,
        b'L' | b'[' => b'L',
        _ => return None,
    };
    // `VarHandleMeta::ref_type_class_id` holds the component's id for a
    // reference-array field; the plan keeps the two meanings apart.
    let ref_array = vh_ref_array_component_lookup_name(&meta.field_desc).is_some();
    Some(VarHandleInstanceFieldPlan {
        field_index: meta.field_index as u32,
        value_desc,
        class_id: meta.class_id,
        read_only: meta.read_only,
        ref_value_unchecked: value_desc == b'L' && meta.field_desc == "Ljava/lang/Object;",
        ref_array_element: one_dim_primitive_array_element(&meta.field_desc),
        ref_type_class_id: if meta.field_desc.starts_with('[') {
            0
        } else {
            meta.ref_type_class_id
        },
        ref_array_component_class_id: if ref_array { meta.ref_type_class_id } else { 0 },
    })
}

/// Immutable layout of a plain byte-array view read. Exact handles and all
/// other coordinates stay on the descriptor-judging native road.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarHandleByteArrayViewPlan {
    pub value_desc: u8,
    pub little_endian: bool,
}

thread_local! {
    pub(crate) static VH_BYTE_VIEW_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<VarHandleByteArrayViewPlan>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// Resolve THIS live handle's metadata, using the same VM/address/generation
/// contract as the field plan. No values or heap pointers are cached.
pub fn varhandle_byte_array_view_plan_for_handle(
    vm: usize,
    identity_hash: i32,
    vh_addr: usize,
) -> Option<VarHandleByteArrayViewPlan> {
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (identity_hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = VH_BYTE_VIEW_PLAN_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        (line.vm == vm
            && line.key == identity_hash
            && line.addr == vh_addr
            && line.generation == generation)
            .then_some(line.value)
    });
    if let Some(plan) = hit {
        return plan;
    }
    let plan = existing_weak_lock_key_at(vm, identity_hash, vh_addr).and_then(|key| {
        let table = vh_meta_table().lock();
        let meta = table.get(key)?;
        if meta.exact.is_some() {
            return None;
        }
        let (value_desc, little_endian) = byte_view_kind(Some(meta))?;
        if !b"SCIJFD".contains(&value_desc) {
            return None;
        }
        Some(VarHandleByteArrayViewPlan {
            value_desc,
            little_endian,
        })
    });
    VH_BYTE_VIEW_PLAN_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = VhMemoLine {
            vm,
            key: identity_hash,
            addr: vh_addr,
            generation,
            value: plan,
        };
    });
    plan
}

/// Immutable layout of a plain byte-buffer view read. Exact handles and all
/// other coordinates stay on the descriptor-judging native road.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarHandleByteBufferViewPlan {
    pub value_desc: u8,
    pub little_endian: bool,
}

thread_local! {
    pub(crate) static VH_BYTE_BUFFER_VIEW_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<VarHandleByteBufferViewPlan>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// Resolve THIS live handle's metadata, using the same VM/address/generation
/// contract as the field plan. No values or heap pointers are cached.
pub fn varhandle_byte_buffer_view_plan_for_handle(
    vm: usize,
    identity_hash: i32,
    vh_addr: usize,
) -> Option<VarHandleByteBufferViewPlan> {
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (identity_hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = VH_BYTE_BUFFER_VIEW_PLAN_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        (line.vm == vm
            && line.key == identity_hash
            && line.addr == vh_addr
            && line.generation == generation)
            .then_some(line.value)
    });
    if let Some(plan) = hit {
        return plan;
    }
    let plan = existing_weak_lock_key_at(vm, identity_hash, vh_addr).and_then(|key| {
        let table = vh_meta_table().lock();
        let meta = table.get(key)?;
        if meta.exact.is_some() {
            return None;
        }
        let (value_desc, little_endian) = byte_buffer_view_kind(Some(meta))?;
        if !b"SCIJFD".contains(&value_desc) {
            return None;
        }
        Some(VarHandleByteBufferViewPlan {
            value_desc,
            little_endian,
        })
    });
    VH_BYTE_BUFFER_VIEW_PLAN_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = VhMemoLine {
            vm,
            key: identity_hash,
            addr: vh_addr,
            generation,
            value: plan,
        };
    });
    plan
}

/// Bumped whenever [`vh_meta_table`]'s contents change, so a reader that
/// cached a lookup can tell in one relaxed load whether its answer still
/// stands.
///
/// Every mutation site bumps it: `vh_meta_put` (a new handle),
/// `vh_meta_update_field_index` (a handle whose field slot has just been
/// resolved, which turns a `None` plan into a `Some`), the FFM layout-handle
/// registration, the per-VM teardown and the freed-key drop -- and, since
/// gc-common w29-b, every moving collection ([`note_objects_moved`]), because
/// the native-path memos are keyed by address. A stale NEGATIVE answer is
/// exactly as dangerous as a stale positive one — the resolve-on-first-use
/// path depends on the second call seeing the newly resolved slot.
pub(crate) static VH_META_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Version of the existing weak VarHandle metadata proofs. JIT primitive-read
/// caches pair this with VM identity, live handle address and the heap epoch;
/// a changed table, freed key or moved handle invalidates the complete proof.
#[inline]
pub fn varhandle_metadata_generation() -> u64 {
    VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Publish that [`vh_meta_table`] has changed.
pub(crate) fn vh_meta_bump_generation() {
    VH_META_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// One direct-mapped thread-local memo line for [`vh_meta_get`] /
/// [`p67_segment_vh_shape`] (see [`vh_memo_lookup`]).
///
/// `vm` is part of the key: one thread can run natives for two VMs (the unit
/// test binaries do), and the hash alone does not name a row any more. `addr`
/// too, since gc-common w29-b: two live handles of one VM can share a hash.
#[derive(Clone)]
pub(crate) struct VhMemoLine<T> {
    pub(crate) vm: usize,
    pub(crate) key: i32,
    pub(crate) addr: usize,
    pub(crate) generation: u64,
    pub(crate) value: Option<T>,
}

impl<T> VhMemoLine<T> {
    /// A line no lookup matches (`generation` is never `u64::MAX`).
    const fn empty() -> Self {
        VhMemoLine {
            vm: 0,
            key: 0,
            addr: 0,
            generation: u64::MAX,
            value: None,
        }
    }
}

thread_local! {
    /// Per-thread memo of [`vh_meta_get`], guarded by [`VH_META_GENERATION`].
    ///
    /// The plan memo beside this one took the global lock off the JIT's fast
    /// paths, and the scaling probe went from 0.07x to 0.68x at 24 threads. It
    /// did NOTHING for `CompletableFuture` composition, because composition is
    /// CAS-dominated, CAS has no direct bind, and the GENERIC native reaches
    /// the table through `vh_meta_get` — which was still taking the mutex on
    /// every operation. Memoising only the fast paths leaves the funnel
    /// convoying exactly as before.
    ///
    /// Holding the `Arc` here keeps the meta alive per thread, which costs
    /// nothing real: every `VarHandle` is already a permanent GC root
    /// (`vh_meta_put` registers one) and the table holds the same `Arc`.
    pub(crate) static VH_META_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<VarHandleMeta>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );

    /// Per-thread memo of [`p67_segment_vh_shape`], on the same terms as
    /// [`VH_META_MEMO`] (gc-common w29-b: the lookup now also takes the
    /// lock-key registry's mutex, which this keeps off the steady state of an
    /// FFM layout handle's `get` / `set`).
    pub(crate) static P67_SHAPE_MEMO: std::cell::RefCell<Vec<VhMemoLine<SegmentVhShape>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// One direct-mapped thread-local line of the JIT plan memo
/// ([`varhandle_instance_field_plan_for_handle`]): the handle it answers for
/// (VM, identity hash and ADDRESS), the generation it was computed in, and the
/// answer.
#[derive(Clone, Copy)]
pub(crate) struct VhPlanMemoLine {
    pub(crate) vm: usize,
    pub(crate) key: i32,
    pub(crate) addr: usize,
    pub(crate) generation: u64,
    pub(crate) plan: Option<VarHandleInstanceFieldPlan>,
}

/// Slots in the per-thread memo. A power of two so the index is a mask, and
/// small enough to stay in L1 — a thread touches a handful of distinct handles
/// in practice (`CompletableFuture` uses three).
pub(crate) const VH_PLAN_MEMO_SLOTS: usize = 64;

thread_local! {
    /// Per-thread memo of [`varhandle_instance_field_plan_for_handle`].
    ///
    /// **Why per-thread and not a better shared map.** `vh_meta_table` is ONE
    /// process-global `parking_lot::Mutex`, and every `VarHandle` operation
    /// took it — the JIT read fast path, the write fast path and the generic
    /// funnel alike. Measured with `HibfixVarHandleScale`, whose threads each
    /// own their own object and their own field so there is no contention on
    /// the DATA:
    ///
    /// ```text
    ///  1 thread   8 780 723 ops/s   1.00x
    ///  8 threads  1 343 026 ops/s   0.15x
    /// 16 threads    790 919 ops/s   0.09x
    /// ```
    ///
    /// Throughput went DOWN with threads — 11x slower at 16 than at 1. That is
    /// lock convoying, and no amount of making the critical section cheaper
    /// fixes it; the shared cache line has to leave the steady-state path.
    /// A `RwLock` would not do it either: a reader still does an atomic RMW on
    /// one word, which is the thing that convoys. Since gc-common w36-a a miss
    /// also takes the lock-key registry's mutex, one more reason the steady
    /// state must hit here.
    ///
    /// The generation check IS a shared read, but a relaxed LOAD of a word
    /// nothing is writing stays in every core's cache and scales.
    pub(crate) static VH_PLAN_MEMO: std::cell::RefCell<[VhPlanMemoLine; VH_PLAN_MEMO_SLOTS]> =
        std::cell::RefCell::new(
            [VhPlanMemoLine { vm: 0, key: 0, addr: 0, generation: u64::MAX, plan: None };
                VH_PLAN_MEMO_SLOTS],
        );
}

/// Look a [`VarHandleInstanceFieldPlan`] up for ONE handle: the executing VM's
/// `vm_identity` (`SharedVm::vm_identity`), the handle's identity hash (the
/// `NativeContext`-compatible one, a monitor-displaced hash included) and its
/// CURRENT address. This is the form the JIT's VarHandle read, write and CAS
/// fast paths call (`vm/src/jit/helpers.rs`).
///
/// gc-common w36-a (`common-w15e-varhandle-meta-tables-keyed-by-bare-identity-hash`,
/// JIT half; `common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
/// site 5). The lookup this replaces had only the hash and answered with the
/// VM's lone row under it: an UNREGISTERED handle (a real-JDK `VarHandle` the
/// natives never filed) whose hash equalled one registered handle's was served
/// that handle's plan -- a field access at the wrong slot -- and a compact
/// instance's hash repeats every 2^20 mints per class. This resolves the
/// handle's weak lock key from the VM's registry slot AT `vh_addr`
/// ([`existing_weak_lock_key_at`], the match [`vh_meta_get`] makes through
/// [`crate::existing_weak_lock_key`]), so the plan is this very object's row
/// or nothing; and it needs no "sole VM" guess, so a process with two VMs
/// keeps the fast path.
///
/// Memoised per thread on `(vm, hash, address, VH_META_GENERATION)`, the key
/// [`vh_memo_lookup`] uses for the natives' own memos, and valid for the same
/// reasons: an address names another object only after the collection that
/// moved or freed the first; the generation moves on every moving collection
/// ([`note_objects_moved`]) and on every change to the table (a registered
/// handle that dies has its key freed and its row dropped by
/// [`forget_var_handle_meta_keys`], which moves it too); and an unregistered
/// handle's negative line can at worst answer `None` for another unregistered
/// object. A `None` is memoised as well: the read path asks about handles
/// that are not resolved instance fields on every call.
///
/// Cost: the steady state is a thread-local line compare and one acquire load
/// of the generation, as before. A miss (first use per thread and handle, and
/// again after each moving collection or table change) takes the lock-key
/// registry's mutex, then the table's -- never both at once.
pub fn varhandle_instance_field_plan_for_handle(
    vm: usize,
    identity_hash: i32,
    vh_addr: usize,
) -> Option<VarHandleInstanceFieldPlan> {
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (identity_hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = VH_PLAN_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = memo[slot];
        (line.vm == vm
            && line.key == identity_hash
            && line.addr == vh_addr
            && line.generation == generation)
            .then_some(line.plan)
    });
    if let Some(plan) = hit {
        return plan;
    }
    // The registry's lock is released before the table's is taken.
    let plan = existing_weak_lock_key_at(vm, identity_hash, vh_addr)
        .and_then(varhandle_instance_field_plan_uncached);
    VH_PLAN_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = VhPlanMemoLine {
            vm,
            key: identity_hash,
            addr: vh_addr,
            generation,
            plan,
        };
    });
    plan
}

pub(crate) fn vh_meta_update_field_index(ctx: &mut dyn NativeContext, vh: ObjectRef, idx: i32) {
    // A handle never registered has no key, and so no row to update.
    let Some(key) = crate::existing_weak_lock_key(&*ctx, vh) else {
        return;
    };
    let vm = ctx.vm_identity();
    let mut t = vh_meta_table().lock();
    let Some(existing) = t.get(key) else {
        return;
    };
    // Build a fresh Arc with the bumped field_index (Arc-immutability —
    // the old Arc may still be held by an in-flight call site).
    let mut updated = (**existing).clone();
    updated.field_index = idx;
    t.insert(vm, key, Arc::new(updated));
    drop(t);
    // This is the resolve-on-first-use transition: the plan for `key` was
    // `None` a moment ago and is `Some` now, so every memoised negative for it
    // has to be discarded.
    vh_meta_bump_generation();
    ctx.note_varhandle_field_resolution();
}

// ---------------------------------------------------------------------------
// FFM layout VarHandles (`ValueLayout.JAVA_INT.varHandle()`,
// `layout.varHandle(PathElement...)`)
// ---------------------------------------------------------------------------
//
// The receiver is a synthetic `java/lang/invoke/VarHandle` whose slots the real
// class declares as `vform`/`…`, so nothing about what it addresses can be read
// off the object — this table IS the handle's meaning. It used to hold only the
// access WIDTH, which was enough for `accessModeType` and for nothing else:
//
//   * `varType()`/`coordinateTypes()` refused (`kind 0`) because they fell
//     through to the generic path, which read slot 0 — `foreign_ffm`'s
//     endianness flag — as a kind tag;
//   * `get`/`set` fell through to the INSTANCE-FIELD arm for the same reason
//     and silently did nothing. Measured on the shipping binary:
//     `vhInt.set(seg, 0L, 11); (int) vhInt.get(seg, 0L)` answered `0`, and the
//     cross-check `seg.get(JAVA_INT, 4)` agreed — the write never happened —
//     where HotSpot answers `11`/`22` (probes/PFfm.java, `RJdkForeign
//     .layoutVarHandles`);
//   * an index coordinate (`sequenceElement()`) could not be expressed at all,
//     so a sequence-element handle addressed offset 0 and `set` took the INDEX
//     as its value.
//
// Width alone also cannot name a carrier: `JAVA_INT` and `JAVA_FLOAT` are both
// four bytes and must answer `int`/`float`. So the row records the carrier, the
// byte order, the fixed offset the layout path walked to, and the stride of the
// one open index the path left behind.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentVhShape {
    /// Access width in bytes (1, 2, 4 or 8).
    pub width: i32,
    /// JVM descriptor byte of the layout's carrier: `b'Z'`, `b'B'`, `b'C'`,
    /// `b'S'`, `b'I'`, `b'J'`, `b'F'`, `b'D'`, or [`FFM_ADDRESS_CARRIER`] for
    /// `ADDRESS` (whose real carrier is `MemorySegment`, not an integer).
    pub carrier: u8,
    pub little_endian: bool,
    /// Byte offset the layout path resolved to, added to the caller's own
    /// offset coordinate.
    pub base_offset: i64,
    /// Stride of the trailing `long` index coordinate; `0` means the handle has
    /// no index coordinate.
    pub stride: i64,
    /// The leaf layout's `byteAlignment()`. HotSpot's `VarHandle`s for a
    /// `MemoryLayout` refuse an access whose resolved offset does not satisfy
    /// this against the segment's own `maxByteAlignment()` — see
    /// `docs/feature-designs/native-dispatch-architecture.md
    /// ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`.
    /// `1` means "never refuses", which is what every byte-carrier layout and
    /// every pre-existing construction site that predates this field means.
    pub alignment: i64,
    /// For an [`FFM_ADDRESS_CARRIER`] leaf: the byte size of the address
    /// layout's target layout (`Utils.pointeeByteSize`), which is the size of
    /// the segment a read answers. `0` without a target, and for every other
    /// carrier (round 14 wave 1, lane ffm;
    /// `r13w13-ffm7-address-read-ignores-target-layout`).
    pub target_size: i64,
    /// The target layout's alignment (`Utils.pointeeByteAlign`): a read
    /// pointer it does not divide is refused. `1` means never refuses.
    pub target_align: i64,
    /// Round 14 wave 6 (lane ffm6): the element count of the sequence whose
    /// `sequenceElement()` opened the index coordinate. JDK 25's
    /// `LayoutPath.addScaledOffset` is `Objects.checkIndex(index, bound)`, so
    /// an index outside `[0, bound)` is `IndexOutOfBoundsException` even when
    /// the address still lies inside the segment. `None`: not checked (no
    /// index coordinate, a scaled index, or `CRATONVM_FFM_LAYOUT_VH_INDEX_BOUND=0`).
    pub index_bound: Option<i64>,
    /// Round 14 wave 6 (lane ffm6): the index coordinate is
    /// `MemoryLayout.arrayElementVarHandle`'s leading one, which JDK 25 feeds
    /// through `AbstractLayout.scale(offset, index)`: a negative offset or
    /// index is `IllegalArgumentException`, and `offset + byteSize * index`
    /// overflowing is `ArithmeticException` (`Math.addExact`/`multiplyExact`).
    pub scaled_index: bool,
    /// Round 14 wave 6 (lane ffm6): the byte size of the layout the handle was
    /// made FROM (`varHandle(path)`'s receiver). JDK 25's `checkSegment` ->
    /// `checkEnclosingLayout(base, enclosing, ..)` requires that whole layout
    /// to fit at the handle's base offset (the offset coordinate, scaled by
    /// the index for an `arrayElementVarHandle`), not just the accessed
    /// member: `sequenceLayout(10, JAVA_INT).varHandle(sequenceElement())`
    /// on an 8-byte segment refuses even index 0. `0`: only the member is
    /// checked (`CRATONVM_FFM_LAYOUT_VH_ENCLOSING_BOUNDS=0`, and every shape
    /// built before this field).
    pub enclosing_size: i64,
}

/// Keyed like [`VhMetaTable`]: by the handle's weak lock key (gc-common w29-b;
/// it was `(vm_identity, identity hash)` since gc-common w18-b, and the bare
/// hash before that). Every heap mints identity hashes from the same seed, so
/// the bare hash named a different handle in each VM of the process; within
/// one VM two live layout handles share a hash once the counter wraps, and
/// the second registration overwrote the first's shape -- a `JAVA_INT` handle
/// then read eight bytes at another layout's offset. The row carries its VM
/// for [`forget_vm_var_handle_meta`]; a dead handle's row goes with its key
/// ([`forget_var_handle_meta_keys`]).
pub(crate) static P67_MEMORY_SEGMENT_VH_TABLE: std::sync::OnceLock<
    parking_lot::Mutex<rustc_hash::FxHashMap<usize, (usize, SegmentVhShape)>>,
> = std::sync::OnceLock::new();

pub(crate) fn p67_memory_segment_vh_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<usize, (usize, SegmentVhShape)>> {
    P67_MEMORY_SEGMENT_VH_TABLE
        .get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

pub(crate) fn register_p67_memory_segment_var_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    shape: SegmentVhShape,
) {
    ctx.register_var_handle_root(vh);
    let vm = ctx.vm_identity();
    let key = vh_lock_key(ctx, vh);
    let shape = SegmentVhShape {
        width: shape.width.clamp(1, 8),
        ..shape
    };
    p67_memory_segment_vh_table()
        .lock()
        .insert(key, (vm, shape));
    // A handle's negative memo line may predate its registration.
    vh_meta_bump_generation();
}

pub(crate) fn p67_segment_vh_shape(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
) -> Option<SegmentVhShape> {
    vh_memo_lookup(&P67_SHAPE_MEMO, ctx, vh, |key| {
        p67_memory_segment_vh_table()
            .lock()
            .get(&key?)
            .map(|&(_, shape)| shape)
    })
}

pub(crate) fn p67_memory_segment_var_handle_width(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
) -> Option<i32> {
    p67_segment_vh_shape(ctx, vh).map(|s| s.width)
}

/// The layout carrier byte for `ADDRESS` — `ValueLayout.ADDRESS` and
/// `AddressLayout`, whose real carrier is `MemorySegment`.
///
/// Before this existed, `p67_carrier_descriptor_byte` folded `ADDRESS` into
/// the same fallback as any other non-primitive carrier of its width (`b'J'`,
/// since `ADDRESS` is 8 bytes), so `varType()` answered `long` and `get`/`set`
/// moved a raw pointer value with no `MemorySegment` on either end. That is
/// the `varType()` half of
/// `docs/feature-designs/native-dispatch-architecture.md
/// ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`'s
/// §1: fixing the DESCRIPTION alone (answering `MemorySegment` from
/// `varType()` while `get`/`set` still moved a `long`) would make the handle
/// describe itself as something its own accessors do not produce — worse than
/// today's consistent-but-wrong answer. This carrier code is therefore never
/// read by `layout_vh_decode`/`layout_vh_encode` directly (those only ever
/// see it filtered through `layout_vh_get`/`layout_vh_set`, which special-case
/// it into a real `MemorySegment` mint/unwrap around the same raw-pointer
/// bytes every other 8-byte carrier already moves) — see those two functions.
///
/// Not a JVM descriptor byte (`'Z'`/`'B'`/… are all uppercase ASCII letters
/// the classfile format assigns meaning to); deliberately lowercase so it can
/// never collide with one if a ninth primitive-shaped carrier is ever added.
pub(crate) const FFM_ADDRESS_CARRIER: u8 = b'a';

/// The JVM descriptor of an FFM layout handle's carrier.
pub(crate) fn layout_vh_carrier_desc(carrier: u8) -> &'static str {
    match carrier {
        b'Z' => DESC_BOOLEAN,
        b'B' => DESC_BYTE,
        b'C' => DESC_CHAR,
        b'S' => DESC_SHORT,
        b'J' => DESC_LONG,
        b'F' => DESC_FLOAT,
        b'D' => DESC_DOUBLE,
        FFM_ADDRESS_CARRIER => DESC_MEMORY_SEGMENT,
        _ => DESC_INT,
    }
}

/// The coordinate descriptors of an FFM layout handle: `(MemorySegment, long)`
/// plus one more `long` when the layout path left an open index behind.
pub(crate) fn layout_vh_coordinates(shape: SegmentVhShape) -> Vec<String> {
    let mut coords = vec![
        "Ljava/lang/foreign/MemorySegment;".to_string(),
        DESC_LONG.to_string(),
    ];
    if shape.stride > 0 {
        coords.push(DESC_LONG.to_string());
    }
    coords
}

/// Where an FFM layout `VarHandle` access lands: a real address for a
/// native/off-heap segment, or an array object plus a byte offset into it for
/// a heap segment (`MemorySegment.ofArray(...)`), which has no native
/// pointer at all. Read/write have to branch on this because a heap array is
/// GC-managed — there is no raw pointer to dereference, ever, not even
/// transiently.
pub(crate) enum LayoutVhAddr {
    Native(usize),
    Heap {
        base: ObjectRef,
        elem_width: usize,
        elem_type: cratonvm_types::ArrayElementType,
        /// Absolute byte offset into `base`'s data, i.e. `view.start + offset`.
        byte_offset: i64,
    },
}

/// Round 14 wave 6 (lane ffm6): the refusal JDK 25 raises for a layout
/// handle's index coordinate, or `None`. It runs in the coordinate adapter
/// (`collectCoordinates`) BEFORE the target handle, so it comes before the
/// value's refusals, the segment's `checkAccess` and the session check.
///
/// * [`SegmentVhShape::scaled_index`] (`arrayElementVarHandle`):
///   `AbstractLayout.scale(offset, index)` -- `checkNonNegativeArgument` on
///   both (offset first), then `Math.addExact(offset,
///   Math.multiplyExact(byteSize, index))`.
/// * [`SegmentVhShape::index_bound`] (`sequenceElement()`):
///   `Objects.checkIndex(index, bound)`.
///
/// Before it, a negative index surfaced as the segment bounds refusal (or,
/// with a positive offset, addressed memory BEFORE it), and a sequence index
/// past the element count addressed whatever followed the sequence.
pub(crate) fn layout_vh_index_refusal(shape: SegmentVhShape, args: &[Value]) -> Option<MethodCallFailed> {
    if shape.stride <= 0 {
        return None;
    }
    let long_at = |i: usize| -> i64 {
        match args.get(i) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        }
    };
    let index = long_at(3);
    if shape.scaled_index {
        let offset = long_at(2);
        let negative = if offset < 0 {
            Some(("offset", offset))
        } else if index < 0 {
            Some(("index", index))
        } else {
            None
        };
        if let Some((name, value)) = negative {
            return Some(
                RuntimeError::IllegalArgumentException {
                    message: format!("The provided {name} is negative: {value}"),
                }
                .into(),
            );
        }
        let overflow = shape
            .stride
            .checked_mul(index)
            .and_then(|scaled| offset.checked_add(scaled))
            .is_none();
        if overflow {
            return Some(
                RuntimeError::ArithmeticException {
                    message: "long overflow".to_string(),
                }
                .into(),
            );
        }
        return None;
    }
    match shape.index_bound {
        Some(bound) if index < 0 || index >= bound => Some(
            RuntimeError::IndexOutOfBoundsException {
                message: Some(format!("Index {index} out of bounds for length {bound}")),
            }
            .into(),
        ),
        _ => None,
    }
}

/// Resolve an FFM layout handle's access into a validated address, and say
/// which argument slot carries the value on a write.
///
/// `args` is `[vh, segment, offset, (index,)? (value)?]`. The offset is the
/// layout path's own `base_offset` plus the caller's offset coordinate plus
/// `index * stride` when the handle has an index coordinate.
///
/// Out of bounds raises `IndexOutOfBoundsException`, which is what the JDK
/// raises for a segment access past the end — never a zero read or a dropped
/// write, which is what this path did before it existed.
pub(crate) fn layout_vh_access(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    args: &[Value],
) -> Result<Option<(LayoutVhAddr, usize)>, MethodCallFailed> {
    let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
        return Ok(None);
    };
    // GC-safety: the scope check runs `Scope.checkValidState()` bytecode, which
    // can collect and relocate the segment. The pin and the re-read live inside
    // `p67_segment_check_scope` now — it takes its receiver by `&mut` and hands
    // back the forwarded reference — so this call site cannot get it wrong, and
    // neither can the three that never had the hand-written pin this replaces.
    let mut seg = seg;
    // `CRATONVM_FFM_BOUNDS_BEFORE_SCOPE` (`panama::ffm_bounds_before_scope`).
    if !crate::panama::ffm_bounds_before_scope() {
        crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut seg)?;
        return layout_vh_locate(ctx, shape, args, seg).map(Some);
    }
    // Round 13 wave 1 (lane mhffm): HotSpot's order -- bounds and alignment
    // (`checkEnclosingLayout`), then the session inside `ScopedMemoryAccess`
    // -- so a closed (or another thread's) segment accessed out of bounds is
    // `IndexOutOfBoundsException`, as there. A heap segment's array can move
    // while the session check runs Java, so its address is located again.
    let located = layout_vh_locate(ctx, shape, args, seg)?;
    crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut seg)?;
    if matches!(located.0, LayoutVhAddr::Native(_)) {
        return Ok(Some(located));
    }
    layout_vh_locate(ctx, shape, args, seg).map(Some)
}

/// [`layout_vh_access`]'s bounds, alignment and address, for a segment whose
/// session is checked by the caller.
pub(crate) fn layout_vh_locate(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    args: &[Value],
    seg: ObjectRef,
) -> Result<(LayoutVhAddr, usize), MethodCallFailed> {
    let long_at = |i: usize| -> i64 {
        match args.get(i) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        }
    };
    let mut offset = shape.base_offset.saturating_add(long_at(2));
    let value_index = if shape.stride > 0 {
        offset = offset.saturating_add(shape.stride.saturating_mul(long_at(3)));
        4
    } else {
        3
    };
    // The HOST base: a segment over a direct buffer reports the buffer's
    // `Unsafe` arena handle, which `layout_vh_raw_native` cannot dereference.
    let base = crate::panama_libffi::segment_host_address(ctx, seg);
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let width = shape.width.clamp(1, 8) as i64;
    // Round 14 wave 6 (lane ffm6): the whole enclosing layout must fit at the
    // handle's base (`SegmentVhShape::enclosing_size`): the offset coordinate,
    // plus the scaled index for an `arrayElementVarHandle` (whose index is
    // folded into the base before the target handle runs). A
    // `sequenceElement()` index is not: it moves the member, not the base.
    let root_base = if shape.scaled_index && shape.stride > 0 {
        long_at(2).saturating_add(shape.stride.saturating_mul(long_at(3)))
    } else {
        long_at(2)
    };
    if shape.enclosing_size > 0
        && (root_base < 0 || root_base.saturating_add(shape.enclosing_size) > size)
    {
        // SBR3 lane ffm: the segment names itself, as
        // `AbstractMemorySegmentImpl.outOfBoundException` does (j27 wave 4,
        // lane FFMMSG: a carrier too; it said `segment of N bytes`).
        let text = segment_refusal_text(ctx, seg);
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(crate::panama::out_of_bounds_message(
                &text,
                root_base,
                shape.enclosing_size,
            )),
        }
        .into());
    }
    if offset < 0 || offset.saturating_add(width) > size {
        // j27 wave 4 (lane FFMMSG): `outOfBoundException`'s text naming the
        // segment; it said `VarHandle access of N bytes at offset .. is out
        // of bounds for a segment of N bytes`. HotSpot's numbers are the
        // enclosing layout's (`root_base`, its size), equal to these for a
        // value-layout handle; a member handle is refused above when
        // `enclosing_size` is modelled.
        return Err(crate::panama::segment_out_of_bounds(ctx, seg, offset, width));
    }
    // `segment_address` returns 0 for a HEAP segment (`MemorySegment.ofArray`
    // etc.) — a heap segment has no native pointer, only a Java array
    // reference, which is not a defect in `segment_address` and must not be
    // read as "this segment is invalid". It used to be, here: every
    // layout-derived VarHandle access on a heap segment was refused with a
    // spurious IndexOutOfBoundsException, where HotSpot supports it fine. See
    // `docs/feature-designs/native-dispatch-architecture.md
    // ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`.
    let addr = if base != 0 {
        LayoutVhAddr::Native((base as usize).wrapping_add(offset as usize))
    } else {
        let Some(view) = crate::panama::heap_segment_view(ctx, seg) else {
            // A native segment whose block is gone (a closed arena's freed
            // handle resolves to no address): the JDK's answer is the
            // session's (`IllegalStateException: Already closed`), not a
            // bounds failure of a segment that still reports its length.
            let mut closed_probe = seg;
            crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut closed_probe)?;
            return Err(crate::panama::segment_out_of_bounds(
                ctx,
                closed_probe,
                offset,
                width,
            ));
        };
        LayoutVhAddr::Heap {
            base: view.base,
            elem_width: view.elem_width,
            elem_type: view.elem_type,
            byte_offset: view.start.saturating_add(offset),
        }
    };
    // The bounds check above says the access fits; this says it is aligned.
    // Same predicate as every other FFM alignment gate in `panama.rs`
    // (`asSlice`, `elements`/`spliterator`, the heap `get`/`set` accessor),
    // and the same one `heap_segment_view` above resolves from, so the two
    // cannot disagree about which segment this is:
    // the layout's required alignment must not exceed what the segment
    // promises AT THIS OFFSET (`seg.asSlice(offset).maxByteAlignment()`), not
    // merely what it promises at offset 0 — a `MemorySegment.ofArray(new
    // int[2])` is 4-byte alignable throughout, so a `JAVA_LONG` (alignment 8)
    // var handle access anywhere in it must be refused, exactly the "no
    // access checks alignment" defect this field was added to close.
    // Round 14 wave 7 (lane review7): `shape.alignment` is the ROOT layout's,
    // which JDK 25's `checkEnclosingLayout` checks at the handle's BASE (the
    // `root_base` above), not at the member: `structLayout(JAVA_INT,
    // JAVA_INT.withName("b"), JAVA_LONG).varHandle(groupElement("b"))` reads
    // offset 4 of an 8-aligned segment at base 0. A shape that models the
    // enclosing layout (`enclosing_size > 0`; `CRATONVM_FFM_LAYOUT_VH_ENCLOSING_BOUNDS=0`
    // restores the member offset) is probed there; its bounds check above
    // already refused a negative `root_base`.
    let align_at = if shape.enclosing_size > 0 {
        root_base
    } else {
        offset
    };
    if shape.alignment > 1
        && shape.alignment > crate::panama::pe_segment_max_byte_alignment_at(ctx, seg, align_at)
    {
        // j27 wave 4 (lane FFMMSG): HotSpot's text (`.. (of <layout>) for
        // segment <segment>`); it ended at `for segment`.
        let vh = match args.first() {
            Some(Value::Object(Some(vh))) => Some(*vh),
            _ => None,
        };
        return Err(enclosing_misaligned(ctx, seg, vh, align_at, shape.alignment));
    }
    Ok((addr, value_index))
}

/// Decode a raw bit pattern into the `Value` `shape.carrier` calls for.
///
/// `raw`'s low `width` bytes hold the value already assembled in the
/// "natural" orientation both addressing modes produce it in — see
/// [`layout_vh_raw_native`] and [`layout_vh_read_heap`], which handle
/// `little_endian` themselves before calling this. Shared by both so the
/// carrier switch — the part a heap/native split has no reason to duplicate —
/// exists exactly once.
pub(crate) fn layout_vh_decode(shape: SegmentVhShape, raw: u64) -> Value {
    match shape.carrier {
        b'Z' => Value::Int(i32::from((raw as u8) != 0)),
        b'B' => Value::Int(raw as u8 as i8 as i32),
        b'C' => Value::Int(raw as u16 as i32),
        b'S' => Value::Int(raw as u16 as i16 as i32),
        // `FFM_ADDRESS_CARRIER` decodes to the same raw 8-byte `Long` `b'J'`
        // does -- `layout_vh_get` is what turns this into a `MemorySegment`,
        // not this function. See `FFM_ADDRESS_CARRIER`'s own doc.
        b'J' | FFM_ADDRESS_CARRIER => Value::Long(raw as i64),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'D' => Value::Double(f64::from_bits(raw)),
        _ => Value::Int(raw as u32 as i32),
    }
}

/// Encode a `Value` into the raw bit pattern [`layout_vh_decode`] reads back —
/// the inverse, and shared for the same reason.
///
/// No `FFM_ADDRESS_CARRIER` arm: `layout_vh_set` unwraps that carrier's
/// `MemorySegment` value coordinate to its raw pointer `Long` before this
/// runs, so it reaches here exactly like `b'J'` and needs no arm of its own.
pub(crate) fn layout_vh_encode(shape: SegmentVhShape, value: Value) -> u64 {
    match (shape.carrier, value) {
        (b'F', Value::Float(f)) => f.to_bits() as u64,
        (b'F', Value::Int(v)) => (v as f32).to_bits() as u64,
        (b'D', Value::Double(d)) => d.to_bits(),
        (b'D', Value::Float(f)) => (f as f64).to_bits(),
        (_, Value::Long(v)) => v as u64,
        (_, Value::Int(v)) => v as i64 as u64,
        (_, Value::Float(f)) => f.to_bits() as u64,
        (_, Value::Double(d)) => d.to_bits(),
        _ => 0,
    }
}

/// Assemble the raw bit pattern [`layout_vh_decode`] expects by reading
/// `shape.width` bytes at a native address, honouring `shape.little_endian`.
pub(crate) fn layout_vh_raw_native(shape: SegmentVhShape, addr: usize) -> u64 {
    let le = shape.little_endian;
    let w = shape.width.clamp(1, 8) as usize;
    // SAFETY: `addr` was bounds-checked against the segment's own recorded
    // size by `layout_vh_access`, and the width is the layout's.
    unsafe {
        let p = addr as *const u8;
        let mut raw = [0u8; 8];
        std::ptr::copy_nonoverlapping(p, raw.as_mut_ptr(), w);
        let mut v: u64 = 0;
        for i in 0..w {
            let b = raw[i] as u64;
            if le {
                v |= b << (8 * i);
            } else {
                v = (v << 8) | b;
            }
        }
        v
    }
}

/// Read an FFM layout handle's variable out of a native (off-heap) segment.
pub(crate) fn layout_vh_read_native(shape: SegmentVhShape, addr: usize) -> Value {
    layout_vh_decode(shape, layout_vh_raw_native(shape, addr))
}

/// Write an FFM layout handle's variable into a native (off-heap) segment.
pub(crate) fn layout_vh_write_native(shape: SegmentVhShape, addr: usize, value: Value) {
    let w = shape.width.clamp(1, 8) as usize;
    let raw = layout_vh_encode(shape, value);
    let bytes = if shape.little_endian {
        raw.to_le_bytes()
    } else {
        // Big-endian: the significant bytes are the LAST `w` of the eight.
        let be = raw.to_be_bytes();
        let mut out = [0u8; 8];
        out[..w].copy_from_slice(&be[8 - w..]);
        out
    };
    // SAFETY: bounds-checked by `layout_vh_access`; `w` is the layout width.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, w);
    }
}

/// Read an FFM layout handle's variable out of a HEAP segment
/// (`MemorySegment.ofArray(...)`), which has no native pointer to
/// dereference. `crate::panama::heap_bytes_read` always assembles its `u64`
/// as little-endian (memory's lowest-address byte is the value's LSB); a
/// `!little_endian` layout needs that reversed WITHIN `width` bytes, which is
/// what `seg_swap_bytes` does and is already proven equivalent to the native
/// path's own big-endian assembly (both walk the same memory bytes, just in
/// opposite shift directions).
pub(crate) fn layout_vh_read_heap(
    ctx: &dyn NativeContext,
    shape: SegmentVhShape,
    base: ObjectRef,
    elem_width: usize,
    byte_offset: i64,
) -> Value {
    let w = shape.width.clamp(1, 8) as usize;
    let raw_le = crate::panama::heap_bytes_read(ctx, base, elem_width, byte_offset, w);
    let raw = if shape.little_endian {
        raw_le
    } else {
        seg_swap_bytes(raw_le, w as i64)
    };
    layout_vh_decode(shape, raw)
}

/// Write an FFM layout handle's variable into a HEAP segment — see
/// [`layout_vh_read_heap`], its read-side twin, for the endianness argument.
pub(crate) fn layout_vh_write_heap(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    base: ObjectRef,
    elem_type: cratonvm_types::ArrayElementType,
    elem_width: usize,
    byte_offset: i64,
    value: Value,
) {
    let w = shape.width.clamp(1, 8) as usize;
    let raw = layout_vh_encode(shape, value);
    let raw_le = if shape.little_endian {
        raw
    } else {
        seg_swap_bytes(raw, w as i64)
    };
    crate::panama::heap_bytes_write(ctx, base, elem_type, elem_width, byte_offset, w, raw_le);
}

/// `VarHandle.get` for an FFM layout handle, boxed for the polymorphic call
/// site.
pub(crate) fn layout_vh_get(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    args: &[Value],
) -> MethodCallResult {
    if let Some(refusal) = layout_vh_index_refusal(shape, args) {
        return Err(refusal);
    }
    // Round 12 wave 6 (lane ffm3): an access window from before the scope
    // check to the read, so a shared close waits for it before freeing the
    // block (see `ffm_fast::AccessWindow`); dropped before the boxing
    // allocation below.
    let window = crate::ffm_fast::AccessWindow::open();
    let Some((addr, _)) = layout_vh_access(ctx, shape, args)? else {
        return Ok(Some(Value::Object(None)));
    };
    let value = match addr {
        LayoutVhAddr::Native(a) => layout_vh_read_native(shape, a),
        LayoutVhAddr::Heap {
            base,
            elem_width,
            byte_offset,
            ..
        } => layout_vh_read_heap(ctx, shape, base, elem_width, byte_offset),
    };
    drop(window);
    if shape.carrier == FFM_ADDRESS_CARRIER {
        // `value` is the raw pointer bits as a `Long` (`layout_vh_decode`'s
        // `FFM_ADDRESS_CARRIER` arm) -- box_value_canonical cannot turn that
        // into a `MemorySegment`, it only boxes primitives, so mint the
        // segment here instead of falling through to it. Two-slot, the same
        // shape `MemorySegment.NULL` mints (`foreign_ffm.rs`).
        let ptr = match value {
            Value::Long(v) => v,
            _ => 0,
        };
        // The real address a store of an arena segment wrote (so that C can
        // read it) is that segment's handle again, as `address()` named it
        // (`ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`).
        let ptr = crate::panama_libffi::arena_call_pointer_to_handle(ptr);
        // Round 14 wave 1 (lane ffm): sized by the layout's target (0 without
        // one) and refused when the target's alignment does not divide the
        // pointer, as JDK 25's `LONG_TO_ADDRESS_TARGET` filter
        // (`Utils.longToAddress`) does; the shape carries 0 / 1 when
        // `CRATONVM_FFM_ADDRESS_READ_TARGET` is off
        // (`r13w13-ffm7-address-read-ignores-target-layout`).
        if shape.target_align > 1 && ptr & (shape.target_align - 1) != 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid alignment constraint for address: 0x{ptr:x}"),
            }
            .into());
        }
        // Round 14 wave 3 (lane ffm patch): the six-slot raw-address carrier
        // `get(ADDRESS)` answers, in the global scope as JDK 25's
        // `Utils.longToAddress` makes it (`CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE`).
        let seg = crate::panama::pe_zero_length_segment(ctx, ptr)?;
        ctx.set_field(seg, 1, Value::Long(shape.target_size.max(0)));
        return Ok(Some(Value::Object(Some(seg))));
    }
    // CANONICAL — measured `ffm.layoutInt` / `layoutChar` / `layoutLong` /
    // `layoutByte` = true. `layout_vh_decode` and `layout_vh_carrier_desc`
    // both switch on `shape.carrier`, so the `Value` variant and the
    // descriptor always agree here and the helper's variant guard is never
    // the arm taken.
    Ok(Some(box_value_canonical(
        ctx,
        value,
        layout_vh_carrier_desc(shape.carrier),
    )))
}

/// `CRATONVM_FFM_LAYOUT_VH_READ_ONLY` (default on): a layout VarHandle write
/// to a read-only segment is `IllegalArgumentException: Attempt to write a
/// read-only segment`. Off restores the unchecked write (round 12 wave 7,
/// lane ffm4).
pub(crate) fn layout_vh_read_only_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_LAYOUT_VH_READ_ONLY")
    })
}

/// `CRATONVM_FFM_VH_ADDRESS_HEAP_REFUSAL` (default on; round 14 wave 1, lane
/// ffm): an `ADDRESS` layout VarHandle write of a heap segment is
/// `IllegalArgumentException: Heap segment not allowed`, as JDK 25's
/// `SharedUtils.unboxSegment` makes it. `0` restores the silent C null. j27
/// wave 4 (lane FFMMSG): the message is HotSpot's, the segment's own
/// `toString()` (`panama::heap_segment_not_allowed`); it named the carrier
/// class.
pub(crate) fn layout_vh_address_heap_refusal() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_VH_ADDRESS_HEAP_REFUSAL")
    })
}

/// `VarHandle.set` for an FFM layout handle.
pub(crate) fn layout_vh_set(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    args: &[Value],
) -> MethodCallResult {
    // Round 14 wave 6 (lane ffm6): the index coordinate's adapter runs
    // before even the value's (`layout_vh_index_refusal`).
    if let Some(refusal) = layout_vh_index_refusal(shape, args) {
        return Err(refusal);
    }
    // Round 14 wave 3 (lane ffm patch): JDK 25 unboxes an ADDRESS value
    // (`SharedUtils.unboxSegment`, a `filterValue` adapter) BEFORE the access
    // checks the target, so the value's refusals come first
    // (`CRATONVM_FFM_ADDRESS_STORE_VALUE_FIRST`). Plain reads.
    if shape.carrier == FFM_ADDRESS_CARRIER && crate::panama::ffm_address_store_value_first() {
        let index = if shape.stride > 0 { 4 } else { 3 };
        match args.get(index).copied() {
            Some(Value::Object(None)) => {
                // Round 14 wave 5 (lane fixup2): the JDK's helpful message
                // (`CRATONVM_FFM_NULL_ADDRESS_NPE_MESSAGE`).
                return Err(RuntimeError::NullPointerException {
                    message: crate::panama::ffm_null_address_npe_message(Some(
                        "VarHandle access with a null value coordinate",
                    )),
                }
                .into());
            }
            Some(Value::Object(Some(seg)))
                if layout_vh_address_heap_refusal()
                    && crate::panama::is_heap_shaped_segment(ctx, seg) =>
            {
                return Err(crate::panama::heap_segment_not_allowed(ctx, seg));
            }
            _ => {}
        }
    }
    // Round 12 wave 7 (lane ffm4): a read-only segment refuses the write, as
    // `checkAccess` does before anything else; this path never asked, so
    // `seg.asReadOnly()` written through a layout VarHandle was written.
    if layout_vh_read_only_check() {
        if let Some(Value::Object(Some(seg))) = args.get(1).copied() {
            let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
                ctx,
                &[Value::Object(Some(seg))],
            )?;
            if matches!(read_only, Some(Value::Int(n)) if n != 0) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Attempt to write a read-only segment".into(),
                }
                .into());
            }
        }
    }
    // Round 12 wave 6 (lane ffm3): see `layout_vh_get`. Held to the write;
    // nothing below allocates.
    let _window = crate::ffm_fast::AccessWindow::open();
    // Round 13 wave 1 (lane mhffm): an ADDRESS value is a segment reference,
    // and the session check inside `layout_vh_access` can run Java (and
    // collect); it was read back from the caller's `args` afterwards.
    let value_pin = if shape.carrier == FFM_ADDRESS_CARRIER {
        let index = if shape.stride > 0 { 4 } else { 3 };
        match args.get(index) {
            Some(Value::Object(Some(v))) => Some((ctx.pin_native_root(*v), *v)),
            _ => None,
        }
    } else {
        None
    };
    let access = layout_vh_access(ctx, shape, args);
    let pinned_value = value_pin.map(|(pin, v)| {
        let now = ctx.read_native_pin(pin, v);
        ctx.unpin_native_roots(pin);
        now
    });
    let Some((addr, value_index)) = access? else {
        return Ok(None);
    };
    let mut value = match pinned_value {
        Some(v) => Value::Object(Some(v)),
        None => args.get(value_index).copied().unwrap_or(Value::Int(0)),
    };
    if shape.carrier == FFM_ADDRESS_CARRIER {
        // The value coordinate is a `MemorySegment`, not a `long` -- unwrap it
        // to the raw pointer `layout_vh_encode`'s `b'J'`-shaped fallback arm
        // already knows how to write.
        //
        // MEASURED on Temurin 25.0.4 (`AddressVhProbe`): a literal Java
        // `null` value coordinate raises `NullPointerException` -- it is NOT
        // the same as writing `MemorySegment.NULL`, which is a real,
        // non-null object whose `.address()` happens to be 0 and which this
        // arm's `Value::Object(Some(seg))` case already handles correctly by
        // reading through `segment_address`.
        let ptr = match value {
            // Round 14 wave 1 (lane ffm): a heap segment has no address to
            // store (`segment_address` answers 0 for one, a C null); JDK 25's
            // `SharedUtils.unboxSegment` refuses it, as `set(ADDRESS, ..)`
            // already does here (`panama::pe_segment_set_impl`).
            Value::Object(Some(seg))
                if layout_vh_address_heap_refusal()
                    && crate::panama::is_heap_shaped_segment(ctx, seg) =>
            {
                return Err(crate::panama::heap_segment_not_allowed(ctx, seg));
            }
            Value::Object(Some(seg)) => crate::panama_libffi::address_for_native_store(
                crate::panama_libffi::segment_address(ctx, seg),
            ),
            Value::Object(None) => {
                return Err(RuntimeError::NullPointerException {
                    message: crate::panama::ffm_null_address_npe_message(Some(
                        "VarHandle access with a null value coordinate",
                    )),
                }
                .into());
            }
            _ => 0,
        };
        value = Value::Long(ptr);
    }
    match addr {
        LayoutVhAddr::Native(a) => layout_vh_write_native(shape, a, value),
        LayoutVhAddr::Heap {
            base,
            elem_type,
            elem_width,
            byte_offset,
        } => layout_vh_write_heap(ctx, shape, base, elem_type, elem_width, byte_offset, value),
    }
    Ok(None)
}

/// The word a read-modify-write access of an FFM layout handle works on: an
/// aligned native address, or one element of a heap segment's `int[]` /
/// `long[]` whose width is the layout's.
#[derive(Clone, Copy)]
pub(crate) enum LayoutVhCell {
    Native(*mut u8),
    Heap {
        base: ObjectRef,
        index: usize,
    },
    /// JIT round 20 wave 8 (lane huntFFM): `width` payload bytes at
    /// `byte_off` of a primitive array whose element type is not the access's
    /// own -- a `float[]` / `double[]` segment under `JAVA_FLOAT` /
    /// `JAVA_DOUBLE`, a `long[]` segment under `JAVA_INT` -- as one hardware
    /// atomic on the payload (`NativeContext::primitive_array_atomic_update`,
    /// the `Unsafe` CAS's road). Built only by [`layout_vh_heap_cell`], which
    /// proved the payload flat for this call.
    HeapRaw {
        base: ObjectRef,
        byte_off: usize,
    },
}

/// JIT round 20 wave 8 (lane huntFFM): the read-modify-write cell of a HEAP
/// segment access, `width` (4 or 8) bytes at payload byte `byte_off` of the
/// primitive array `base` (element type `elem_type`). JDK 25 performs the
/// atomic on ANY primitive array the alignment rule admits (a heap segment
/// promises at most its element size, so the element is at least as wide as
/// the access): `JAVA_DOUBLE.varHandle().compareAndSet(ofArray(new double[2]),
/// ..)`, `JAVA_INT ... getAndAdd(ofArray(new long[2]), 8L, ..)` -- HotSpot
/// 25.0.3 answers both, where this VM used to raise
/// `UnsupportedOperationException` for every array but an `int[]` / `long[]`
/// of the access's own width. `None`: a reference array, a misaligned or
/// narrower-element access (which the alignment check refused before any
/// JDK-shaped road reaches here), or a payload with no flat address -- the
/// caller's `UnsupportedOperationException`, as before.
pub(crate) fn layout_vh_heap_cell(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    elem_type: ArrayElementType,
    width: usize,
    byte_off: usize,
) -> Option<LayoutVhCell> {
    if elem_type == ArrayElementType::Reference || byte_off % width != 0 {
        return None;
    }
    let elem_width = cratonvm_types::element_byte_size(elem_type);
    if elem_width < width {
        return None;
    }
    if elem_width == width
        && matches!(
            (width, elem_type),
            (4, ArrayElementType::Int) | (8, ArrayElementType::Long)
        )
    {
        return Some(LayoutVhCell::Heap {
            base,
            index: byte_off / width,
        });
    }
    // A plain atomic load proves the payload flat and the range in bounds;
    // nothing between here and the access reaches a safepoint.
    ctx.primitive_array_atomic_update(base, byte_off, width, &|_| None)?;
    Some(LayoutVhCell::HeapRaw { base, byte_off })
}

impl LayoutVhCell {
    /// The word's bits as memory holds them (the low `width` bytes).
    pub(crate) fn load(self, ctx: &dyn NativeContext, width: usize) -> u64 {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
        match self {
            // SAFETY: `layout_vh_rmw` checked the bounds (`layout_vh_access`)
            // and the `width`-alignment of the pointer.
            LayoutVhCell::Native(p) if width == 4 => {
                u64::from(unsafe { AtomicU32::from_ptr(p as *mut u32) }.load(Ordering::SeqCst))
            }
            LayoutVhCell::Native(p) => {
                unsafe { AtomicU64::from_ptr(p as *mut u64) }.load(Ordering::SeqCst)
            }
            LayoutVhCell::Heap { base, index } => match ctx.get_array_element(base, index) {
                Value::Int(v) => u64::from(v as u32),
                Value::Long(v) => v as u64,
                _ => 0,
            },
            // The starting value of a read-modify-write loop; the CAS below
            // re-reads atomically, so a racing store only costs a retry.
            LayoutVhCell::HeapRaw { base, byte_off } => {
                let mut b = [0u8; 8];
                let w = width.min(8);
                if ctx.read_primitive_array_bytes(base, byte_off, &mut b[..w]) != w {
                    return 0;
                }
                u64::from_le_bytes(b)
            }
        }
    }

    /// Compare-and-exchange: `Ok(current)` when swapped, `Err(witness)`
    /// otherwise.
    pub(crate) fn cas(
        self,
        ctx: &mut dyn NativeContext,
        width: usize,
        current: u64,
        new: u64,
    ) -> Result<u64, u64> {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
        match self {
            // SAFETY: see `load`.
            LayoutVhCell::Native(p) if width == 4 => unsafe { AtomicU32::from_ptr(p as *mut u32) }
                .compare_exchange(
                    current as u32,
                    new as u32,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .map(u64::from)
                .map_err(u64::from),
            LayoutVhCell::Native(p) => unsafe { AtomicU64::from_ptr(p as *mut u64) }
                .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst),
            LayoutVhCell::Heap { base, index } => {
                let (expected, replacement) = if width == 4 {
                    (
                        Value::Int(current as u32 as i32),
                        Value::Int(new as u32 as i32),
                    )
                } else {
                    (Value::Long(current as i64), Value::Long(new as i64))
                };
                if ctx.compare_and_swap_field(base, index, expected, replacement) {
                    Ok(current)
                } else {
                    Err(self.load(&*ctx, width))
                }
            }
            LayoutVhCell::HeapRaw { base, byte_off } => {
                let mask = if width == 4 {
                    u64::from(u32::MAX)
                } else {
                    u64::MAX
                };
                let (current, new) = (current & mask, new & mask);
                match ctx.primitive_array_atomic_update(base, byte_off, width, &|seen| {
                    (seen == current).then_some(new)
                }) {
                    Some((_, true)) => Ok(current),
                    Some((seen, false)) => Err(seen),
                    // `layout_vh_heap_cell` proved the payload flat for this
                    // call; a refusal now is not a witness, so report a value
                    // that differs from `current` (no caller loops on it).
                    None => Err(!current & mask),
                }
            }
        }
    }
}

/// A read-modify-write access mode (`compareAndSet` and the weak ones,
/// `compareAndExchange*`, `getAndSet*`, `getAndAdd*`, `getAndBitwise*`) on an
/// FFM layout handle, as one hardware atomic on the segment's memory
/// (interpreter round i1 wave 38, lane L4 follow-up).
///
/// Before it, every such mode reached the field natives' kind switch with a
/// handle that has no side-table row, so `compareAndSet` answered `false`
/// and wrote nothing, and the others answered `0`/`null`
/// (`L4W38FfmExactLayoutHandle` `cas-ok`, host run of wave 38: `false 9`
/// where HotSpot prints `true 11`). Only `get` and `set` knew layout handles.
///
/// Served: an `int`/`float` (4-byte) or `long`/`double` (8-byte) carrier;
/// `getAndAdd` on `int`/`long` only, the bitwise modes likewise, as the
/// JDK's segment handles for `float`/`double` define neither. A native
/// segment's memory is the atomic word itself (aligned: the layout's own
/// alignment was checked by `layout_vh_locate`, and it is checked again
/// here); a heap segment's word is one element of an `int[]` / `long[]` of
/// the same width, swapped under the VM's CAS lock
/// (`compare_and_swap_field`). Anything else is
/// `UnsupportedOperationException(<mode>)`, never a silent no-op. A
/// `float`/`double` compares its raw bits, as HotSpot's does. The value
/// coordinate(s) follow the offset (and the index, for a sequence handle).
pub(crate) fn layout_vh_rmw(
    ctx: &mut dyn NativeContext,
    shape: SegmentVhShape,
    args: &[Value],
    op: VhViewRmw,
    mode: &'static str,
) -> MethodCallResult {
    let unsupported = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: mode.to_string(),
        }
        .into()
    };
    let integral = matches!(shape.carrier, b'I' | b'J');
    let supported = match op {
        VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => integral,
        _ => integral || matches!(shape.carrier, b'F' | b'D'),
    };
    let width = shape.width.clamp(1, 8) as usize;
    if !supported || !(width == 4 || width == 8) {
        return Err(unsupported());
    }
    // Round 14 wave 6 (lane ffm6): after the mode refusal (the JDK asks the
    // target for the mode's handle first), before everything else.
    if let Some(refusal) = layout_vh_index_refusal(shape, args) {
        return Err(refusal);
    }
    if layout_vh_read_only_check() {
        if let Some(Value::Object(Some(seg))) = args.get(1).copied() {
            let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
                ctx,
                &[Value::Object(Some(seg))],
            )?;
            if matches!(read_only, Some(Value::Int(n)) if n != 0) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Attempt to write a read-only segment".into(),
                }
                .into());
            }
        }
    }
    let window = crate::ffm_fast::AccessWindow::open();
    let Some((addr, value_index)) = layout_vh_access(ctx, shape, args)? else {
        // No segment: `Objects.requireNonNull` on the coordinate.
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let cell = match addr {
        LayoutVhAddr::Native(a) if a % width == 0 => LayoutVhCell::Native(a as *mut u8),
        // JIT round 20 wave 8 (lane huntFFM): any primitive array at least
        // as wide as the access (`layout_vh_heap_cell`), not only an `int[]` /
        // `long[]` of its own width.
        LayoutVhAddr::Heap {
            base,
            elem_type,
            elem_width,
            byte_offset,
        } if elem_width >= width && byte_offset >= 0 => {
            match layout_vh_heap_cell(ctx, base, elem_type, width, byte_offset as usize) {
                Some(cell) => cell,
                None => return Err(unsupported()),
            }
        }
        _ => return Err(unsupported()),
    };
    let mask = if width == 4 {
        u64::from(u32::MAX)
    } else {
        u64::MAX
    };
    // A value as the bits memory holds, and back.
    let word = |v: Value| -> u64 {
        let raw = layout_vh_encode(shape, v) & mask;
        if shape.little_endian {
            raw
        } else {
            seg_swap_bytes(raw, width as i64) & mask
        }
    };
    let value = |w: u64| -> Value {
        let raw = if shape.little_endian {
            w
        } else {
            seg_swap_bytes(w, width as i64) & mask
        };
        layout_vh_decode(shape, raw)
    };
    let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Int(0));
    let result = match op {
        VhViewRmw::CompareAndSet => {
            let ok = cell
                .cas(
                    ctx,
                    width,
                    word(arg(value_index)),
                    word(arg(value_index + 1)),
                )
                .is_ok();
            drop(window);
            return Ok(Some(Value::Int(ok as i32)));
        }
        VhViewRmw::CompareAndExchange => {
            let expected = word(arg(value_index));
            let new = word(arg(value_index + 1));
            let witness = loop {
                match cell.cas(ctx, width, expected, new) {
                    Ok(w) => break w,
                    // A heap cell re-reads its witness after a failed swap;
                    // an equal one is a lost race, so try again.
                    Err(w) if w == expected => continue,
                    Err(w) => break w,
                }
            };
            value(witness)
        }
        VhViewRmw::GetAndSet | VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => {
            let operand = arg(value_index);
            let mut current = cell.load(&*ctx, width);
            loop {
                let old = value(current);
                let new = match (op, old, operand) {
                    (VhViewRmw::GetAndSet, _, v) => v,
                    (VhViewRmw::GetAndAdd, Value::Int(a), Value::Int(b)) => {
                        Value::Int(a.wrapping_add(b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Long(b)) => {
                        Value::Long(a.wrapping_add(b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Int(b)) => {
                        Value::Long(a.wrapping_add(i64::from(b)))
                    }
                    (VhViewRmw::Bitwise(bit), Value::Int(a), Value::Int(b)) => {
                        Value::Int(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    (VhViewRmw::Bitwise(bit), Value::Long(a), Value::Long(b)) => {
                        Value::Long(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    (VhViewRmw::Bitwise(bit), Value::Long(a), Value::Int(b)) => {
                        let b = i64::from(b);
                        Value::Long(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    _ => return Err(unsupported()),
                };
                match cell.cas(ctx, width, current, word(new)) {
                    Ok(_) => break old,
                    Err(witness) => current = witness,
                }
            }
        }
    };
    drop(window);
    // The old value is handed back boxed, as `layout_vh_get` boxes a read.
    Ok(Some(box_value_canonical(
        ctx,
        result,
        layout_vh_carrier_desc(shape.carrier),
    )))
}

/// SBR3 lane ffm: the read-modify-write access modes of a REAL
/// `SegmentVarHandle` (the one `ValueLayout.varHandle()` builds once the layout
/// factories and `ArenaImpl` run the JDK's own bytecode), as one atomic on the
/// segment's memory.
///
/// `get`/`set` of such a handle are served by [`segment_vh_get`] /
/// [`segment_vh_set`]; the read-modify-write modes were not, so every one of
/// them reached the field natives' kind switch with a handle that has no
/// side-table row and answered `false` / `0` / `null` having written nothing
/// (`compareAndSet` `false false 5` where HotSpot prints `true false 6`).
///
/// The JDK's rules, in its order: the mode is refused (`UnsupportedOperation
/// Exception(<mode>)`) for an unaligned layout, a byte/short/char/boolean
/// carrier, and `getAndAdd`/`getAndBitwise*` on `float`/`double`; then the
/// read-only, bounds and alignment checks, then the session. `None` when `args[0]` is
/// not a real `SegmentVarHandle`.
pub(crate) fn segment_vh_rmw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhViewRmw,
    mode: &'static str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(vh))) = args.first().copied() else {
        return None;
    };
    if !is_segment_var_handle(ctx, vh) {
        return None;
    }
    let plan = seg_vh_plan(ctx, vh)?;
    let result = segment_vh_rmw_access(ctx, vh, plan, args, op, mode);
    // JIT r23 wave 6 (lane L8, `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES`):
    // a refused read-modify-write raises through the JDK's own form body,
    // with its frames (the refusals all come before any write).
    if result.is_err() {
        if let Some(jdk) = segment_vh_raise_through_jdk(ctx, vh, args, mode) {
            return Some(jdk);
        }
    }
    Some(result)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn segment_vh_rmw_access(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    plan: SegVhPlan,
    args: &[Value],
    op: VhViewRmw,
    mode: &'static str,
) -> MethodCallResult {
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        variable,
        aligned_form,
    } = plan;
    let unsupported = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: mode.to_string(),
        }
        .into()
    };
    let width = seg_shape_width(shape);
    let w = width as usize;
    let integral = matches!(shape, SegShape::Int | SegShape::Long);
    let floating = matches!(shape, SegShape::Float | SegShape::Double);
    // JIT round 20 wave 8 (lane huntFFM): the FORM decides (the value
    // layout's alignment), not `enclosing_align`: a path handle's `enclosing`
    // is its group layout, aligned for its widest member.
    let supported = aligned_form
        && match op {
            VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => integral,
            _ => integral || floating,
        };
    if !supported {
        return Err(unsupported());
    }
    let base = vh_long_arg(args, 2);
    let offset = if variable {
        vh_long_arg(args, 3)
    } else {
        vh_offset
    };
    let value_index = if variable { 4 } else { 3 };
    // `checkSegment`'s `Objects.requireNonNull`.
    let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
        ctx,
        &[Value::Object(Some(seg))],
    )?;
    if matches!(read_only, Some(Value::Int(n)) if n != 0) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Attempt to write a read-only segment".into(),
        }
        .into());
    }
    segment_vh_check_enclosing(ctx, seg, vh, enclosing_size, enclosing_align, base)?;
    let coerce = |v: Value| -> Value {
        match (shape, v) {
            (SegShape::Long | SegShape::Address, Value::Int(i)) => Value::Long(i64::from(i)),
            (SegShape::Float, Value::Int(i)) => Value::Float(i as f32),
            (SegShape::Float, Value::Long(l)) => Value::Float(l as f32),
            (SegShape::Double, Value::Int(i)) => Value::Double(f64::from(i)),
            (SegShape::Double, Value::Float(f)) => Value::Double(f64::from(f)),
            (SegShape::Double, Value::Long(l)) => Value::Double(l as f64),
            (_, other) => other,
        }
    };
    let mask = if w == 4 {
        u64::from(u32::MAX)
    } else {
        u64::MAX
    };
    // A value as the bits memory holds, and back.
    let word = |v: Value| -> u64 {
        let raw = seg_encode_value(&coerce(v), width) & mask;
        if be {
            seg_swap_bytes(raw, width) & mask
        } else {
            raw
        }
    };
    let value = |bits: u64| -> Value {
        let raw = if be {
            seg_swap_bytes(bits, width) & mask
        } else {
            bits
        };
        seg_decode_value(shape, raw)
    };
    let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Int(0));
    let window = crate::ffm_fast::AccessWindow::open();
    let mut seg = seg;
    segment_vh_check_scope(ctx, &mut seg)?;
    let (heap_base, addr) = segment_raw_access(ctx, seg, offset, base, width)?;
    let cell = match heap_base {
        None => {
            let ptr = if crate::unsafe_arena_addr_is_tagged(addr) {
                crate::unsafe_arena_real_ptr_bounded(addr, w).ok_or_else(|| {
                    MethodCallFailed::from(RuntimeError::IllegalStateException {
                        message: "Out of bound access on off-heap MemorySegment arena handle"
                            .into(),
                    })
                })?
            } else {
                addr as usize as *mut u8
            };
            if ptr.is_null() || (ptr as usize) % w != 0 {
                return Err(unsupported());
            }
            LayoutVhCell::Native(ptr)
        }
        Some(array) => {
            let et = ctx.heap_element_type_of(array);
            if addr < 0 {
                return Err(unsupported());
            }
            // JIT round 20 wave 8 (lane huntFFM): a `double[]` / `float[]`
            // segment, or a `long[]` one under `JAVA_INT`, is one atomic on
            // the payload, as HotSpot's `Unsafe` CAS on the base array
            // (`layout_vh_heap_cell`); it raised `UnsupportedOperationException`.
            match layout_vh_heap_cell(ctx, array, et, w, addr as usize) {
                Some(cell) => cell,
                None => return Err(unsupported()),
            }
        }
    };
    let result = match op {
        VhViewRmw::CompareAndSet => {
            let ok = cell
                .cas(ctx, w, word(arg(value_index)), word(arg(value_index + 1)))
                .is_ok();
            drop(window);
            return Ok(Some(Value::Int(ok as i32)));
        }
        VhViewRmw::CompareAndExchange => {
            let expected = word(arg(value_index));
            let new = word(arg(value_index + 1));
            let witness = loop {
                match cell.cas(ctx, w, expected, new) {
                    Ok(x) => break x,
                    // A heap cell re-reads its witness after a failed swap;
                    // an equal one is a lost race, so try again.
                    Err(x) if x == expected => continue,
                    Err(x) => break x,
                }
            };
            value(witness)
        }
        VhViewRmw::GetAndSet | VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => {
            let operand = coerce(arg(value_index));
            let mut current = cell.load(&*ctx, w);
            loop {
                let old = value(current);
                let new = match (op, old, operand) {
                    (VhViewRmw::GetAndSet, _, v) => v,
                    (VhViewRmw::GetAndAdd, Value::Int(a), Value::Int(b)) => {
                        Value::Int(a.wrapping_add(b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Long(b)) => {
                        Value::Long(a.wrapping_add(b))
                    }
                    (VhViewRmw::Bitwise(bit), Value::Int(a), Value::Int(b)) => {
                        Value::Int(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    (VhViewRmw::Bitwise(bit), Value::Long(a), Value::Long(b)) => {
                        Value::Long(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    _ => return Err(unsupported()),
                };
                match cell.cas(ctx, w, current, word(new)) {
                    Ok(_) => break old,
                    Err(witness) => current = witness,
                }
            }
        }
    };
    drop(window);
    Ok(Some(box_value_canonical(
        ctx,
        result,
        seg_shape_desc(shape),
    )))
}

/// [`segment_vh_rmw`] for the access natives: only a handle with no side-table
/// row can be a real `SegmentVarHandle`, so a field / array / view handle never
/// pays the class-name compare.
#[inline]
pub(crate) fn segment_vh_rmw_unless_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    op: VhViewRmw,
    mode: &'static str,
) -> Option<MethodCallResult> {
    if matches!(row, Some(Some(_))) {
        return None;
    }
    segment_vh_rmw(ctx, args, op, mode)
}

// Descriptor helpers — reconstruct JVM descriptor from MethodType object
// ---------------------------------------------------------------------------

/// Convert a class name (as stored in a Class mirror) to its JVM descriptor form.
/// "int" → "I", "java/lang/String" → "Ljava/lang/String;", "void" → "V", etc.
///
/// Returns `Cow<'static, str>` so the primitive / array cases (the common
/// hot-path branches called from `descriptor_from_method_type` and
/// `widen_descriptor`) avoid an allocation entirely; only the
/// `L<class>;` builder produces an owned `String`.
pub(crate) fn class_name_to_descriptor(name: &str) -> Cow<'static, str> {
    match name {
        NAME_VOID => Cow::Borrowed(DESC_VOID),
        NAME_INT => Cow::Borrowed(DESC_INT),
        NAME_LONG => Cow::Borrowed(DESC_LONG),
        NAME_FLOAT => Cow::Borrowed(DESC_FLOAT),
        NAME_DOUBLE => Cow::Borrowed(DESC_DOUBLE),
        NAME_BOOLEAN => Cow::Borrowed(DESC_BOOLEAN),
        NAME_BYTE => Cow::Borrowed(DESC_BYTE),
        NAME_CHAR => Cow::Borrowed(DESC_CHAR),
        NAME_SHORT => Cow::Borrowed(DESC_SHORT),
        _ if name.starts_with('[') => Cow::Owned(name.to_string()), // already descriptor form
        _ => Cow::Owned(format!("L{name};")),
    }
}

/// Convert a JVM field descriptor to a class name for display.
/// "I" → "int", "Ljava/lang/String;" → "java/lang/String", etc.
pub(crate) fn descriptor_to_class_name(desc: &str) -> Cow<'static, str> {
    match desc {
        DESC_VOID => Cow::Borrowed(NAME_VOID),
        DESC_INT => Cow::Borrowed(NAME_INT),
        DESC_LONG => Cow::Borrowed(NAME_LONG),
        DESC_FLOAT => Cow::Borrowed(NAME_FLOAT),
        DESC_DOUBLE => Cow::Borrowed(NAME_DOUBLE),
        DESC_BOOLEAN => Cow::Borrowed(NAME_BOOLEAN),
        DESC_BYTE => Cow::Borrowed(NAME_BYTE),
        DESC_CHAR => Cow::Borrowed(NAME_CHAR),
        DESC_SHORT => Cow::Borrowed(NAME_SHORT),
        _ if desc.starts_with('L') && desc.ends_with(';') => {
            Cow::Owned(desc[1..desc.len() - 1].to_string())
        }
        _ => Cow::Owned(desc.to_string()),
    }
}

/// Read a Class mirror and extract its JVM descriptor character(s).
pub(crate) fn mirror_to_descriptor(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Cow<'static, str> {
    match resolve_class_name_robust(ctx, mirror) {
        Some(name) => class_name_to_descriptor(&name),
        None => match unnameable_mirror_descriptor(ctx, mirror) {
            Some(own) => {
                crate::mh_finder_census::event("type-render.mirror.proxy-own-name");
                Cow::Owned(own)
            }
            None => {
                crate::mh_finder_census::event("type-render.mirror.object-fallback");
                Cow::Borrowed(DESC_OBJECT)
            }
        },
    }
}

/// The descriptor of a mirror [`resolve_class_name_robust`] cannot name: a
/// lambda proxy's, minted outside the class store (interpreter round i4 wave
/// 92, lane L4; proposal
/// `i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object`;
/// `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME`, default on, kill switch `=0`).
///
/// `L<host>$$Lambda/0x<id>;` -- the proxy's internal name
/// ([`crate::lang_class::lambda_internal_name`], the `Class.getName()` of
/// [`crate::lang_class::lambda_proxy_class_name`] with `/` in the host's
/// package), the spelling HotSpot's `MethodType.toMethodDescriptorString`
/// gives a hidden class. Both renderers used to erase it: `mirror_to_descriptor`
/// to `Ljava/lang/Object;` (a proxy-typed handle passed an `(Object)` exact
/// call site, its parameter casts were casts to `Object`, its `type()` named
/// `Object`), `crate::link_to_type_descriptor` to "unrenderable" (every
/// real-handle door skipped its check). The name resolves back to the proxy
/// in [`mirror_for_descriptor_name_resolving`] and
/// [`lambda_proxy_descriptor_class`]. `None` for any other mirror, or the
/// switch off. Census: `CRATONVM_DBG=mh-finder-census` events
/// `type-render.{mirror,link}.{proxy-own-name,object-fallback,none}`.
pub(crate) fn unnameable_mirror_descriptor(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<String> {
    if !crate::nbflags().mh_unnameable_mirror_own_name {
        return None;
    }
    let cid = ctx.class_id_from_mirror(mirror)?;
    let name = crate::lang_class::lambda_internal_name(ctx, cid)?;
    Some(format!("L{name};"))
}

/// The lambda proxy an internal class name rendered by
/// [`unnameable_mirror_descriptor`] names (`pkg/Host$$Lambda/0x<id>`, no `L`
/// / `;`), if that proxy is live; `None` for any other name or the switch off.
/// Reads only.
pub(crate) fn lambda_proxy_descriptor_class(ctx: &dyn NativeContext, name: &str) -> Option<ClassId> {
    if !name.contains("$$Lambda/0x") || !crate::nbflags().mh_unnameable_mirror_own_name {
        return None;
    }
    crate::lang_class::lambda_component_class_id(ctx, name)
}

/// Split a method descriptor `(p0p1…)ret` into its parameter descriptor tokens
/// and the return token, so MethodHandle combinators can add/drop parameters
/// while preserving the exact JVM type spellings. Returns None on malformed
/// input.
///
/// `pub(crate)` since 2026-08-13: `phases_late`'s Quarkus logging mirror needs
/// the same split to fill an argument vector from a descriptor it reads off
/// the class rather than one it writes down, and a second copy of this walk is
/// a second place for the `[`/`L…;`/primitive rules to drift.
pub(crate) fn split_descriptor_params(desc: &str) -> Option<(Vec<String>, String)> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut params = Vec::new();
    while i < b.len() && b[i] != b')' {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'L' {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
            if i >= b.len() {
                return None;
            }
            i += 1; // include the ';'
        } else {
            i += 1; // primitive
        }
        params.push(desc[start..i].to_string());
    }
    if i >= b.len() {
        return None;
    }
    Some((params, desc[i + 1..].to_string()))
}

// ---------------------------------------------------------------------------
// `MethodHandle.asType` CONVERTIBILITY — the pairwise rule, and the closed
// table it rests on
// ---------------------------------------------------------------------------
//
// `asType` is the gate every JDK adapter goes through, and it had NO check at
// all here until 2026-08-17: the native wrote the requested `MethodType` into
// the receiver's `type` field and handed the receiver back, so a
// `(String)String` handle happily became a `(int,int)int` one. That is the
// single missing assertion in `RJdkProxyIface` — `MethodHandleProxies
// .asInterfaceInstance(Subtractor.class, <(String)String handle>)` returned a
// live proxy where HotSpot refuses.
//
// The rule below is a TRANSCRIPTION of `java.lang.invoke.MethodType.canConvert`
// (JDK 25 `src.zip`, lines 1078-1128) and `MethodType.isConvertibleTo` (986),
// checked cell by cell against a 613-row sweep of HotSpot 25.0.3+9-LTS on this
// host (`scratchpad/g31/AsTypeFamily.java`, `AsTypeExtra.java`; the matrices
// are printed in full in
// `docs/feature-designs/class-resolution-and-caching.md`).
//
// The three things that sweep settled, none of which is guessable:
//
//   1. **Reference -> reference is ALWAYS convertible.** `String -> Integer`,
//      `int[] -> String`, `Void -> Comparable` — every one is accepted, because
//      `null` is always dynamically valid and the cast is deferred to invoke
//      time. Only the primitive edges refuse.
//   2. **`void` is convertible in BOTH directions**, as a return type: to
//      `void` the value is dropped, from `void` a zero/null is introduced. The
//      whole `void` row and the whole `void` column of the return matrix are
//      accepts.
//   3. **`explicitCastArguments` has DIFFERENT rules and is the trap.** Its
//      324-cell return matrix and 289-cell parameter matrix are accepts in
//      EVERY cell; it refuses on arity alone. Applying this predicate there
//      would refuse 248 pairs HotSpot accepts, so [`register_p65_extras`]'s
//      body checks arity and nothing else.
//
// Everything here is a pure function of descriptor strings so it is unit
// testable without a VM, which is the only way it could be checked at all in a
// lane that may not build.

/// The wrapper class descriptor `MethodType.canConvert` boxes a primitive to.
///
/// `V` is deliberately absent: `canConvert` short-circuits `void` before it
/// ever reaches the boxing arm, and `Void` is NOT a wrapper for the purposes of
/// the reference->primitive arm (MEASURED: `Void` unboxes to no primitive —
/// every cell of its row is a refusal).
pub(crate) fn wrapper_desc_for_primitive(prim: &str) -> Option<&'static str> {
    Some(match prim {
        "Z" => "Ljava/lang/Boolean;",
        "B" => "Ljava/lang/Byte;",
        "C" => "Ljava/lang/Character;",
        "S" => "Ljava/lang/Short;",
        "I" => "Ljava/lang/Integer;",
        "J" => "Ljava/lang/Long;",
        "F" => "Ljava/lang/Float;",
        "D" => "Ljava/lang/Double;",
        _ => return None,
    })
}

/// The inverse of [`wrapper_desc_for_primitive`] — `Wrapper.isWrapperType(src)`
/// plus `Wrapper.forWrapperType(src)` in one lookup, which is exactly how
/// `canConvert`'s third reference->primitive test uses it.
pub(crate) fn primitive_desc_for_wrapper(wrapper: &str) -> Option<&'static str> {
    Some(match wrapper {
        "Ljava/lang/Boolean;" => "Z",
        "Ljava/lang/Byte;" => "B",
        "Ljava/lang/Character;" => "C",
        "Ljava/lang/Short;" => "S",
        "Ljava/lang/Integer;" => "I",
        "Ljava/lang/Long;" => "J",
        "Ljava/lang/Float;" => "F",
        "Ljava/lang/Double;" => "D",
        _ => return None,
    })
}

/// JLS 5.1.2 widening primitive conversion, plus identity — `Wrapper
/// .forPrimitiveType(dst).isConvertibleFrom(sw)`.
///
/// `boolean` widens to nothing and nothing widens to it; `char` widens to
/// `int`/`long`/`float`/`double` but NOT to `short`, and `byte`/`short` do not
/// widen to `char`. All three asymmetries are in the measured matrix.
pub(crate) fn primitive_widens_to(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    let wider: &[&str] = match from {
        "B" => &["S", "I", "J", "F", "D"],
        "S" => &["I", "J", "F", "D"],
        "C" => &["I", "J", "F", "D"],
        "I" => &["J", "F", "D"],
        "J" => &["F", "D"],
        "F" => &["D"],
        // `Z` and `V` widen to nothing.
        _ => &[],
    };
    wider.contains(&to)
}

/// `reference.isAssignableFrom(wrapper)` for the eight wrapper classes — the
/// only assignability question `canConvert` ever asks.
///
/// This is a CLOSED table, not an approximation, and that is what makes it
/// usable without a class-hierarchy walk (`NativeContext` offers `is_subclass`,
/// which cannot answer for interfaces, and `Comparable`/`Serializable`/
/// `Constable`/`ConstantDesc` are all interfaces). The wrappers are `final`,
/// so nothing outside `java.base` can ever be one of their supertypes.
/// Enumerated by reflection on HotSpot 25.0.3+9 (`scratchpad/g31/Sup.java`)
/// and cross-checked against the measured conversion matrix:
///
/// ```text
///   Boolean   <: Object Comparable Serializable Constable
///   Character <: Object Comparable Serializable Constable
///   Byte      <: Object Comparable Serializable Constable Number
///   Short     <: Object Comparable Serializable Constable Number
///   Integer   <: Object Comparable Serializable Constable Number ConstantDesc
///   Long      <: Object Comparable Serializable Constable Number ConstantDesc
///   Float     <: Object Comparable Serializable Constable Number ConstantDesc
///   Double    <: Object Comparable Serializable Constable Number ConstantDesc
///   Void      <: Object                                                (only)
/// ```
///
/// `ConstantDesc` covering four wrappers and not six is the row that would have
/// been got wrong by inspection: `Byte` and `Short` are `Constable` but NOT
/// `ConstantDesc`, and the measured matrix agrees (`ConstantDesc` accepts
/// `int`/`long`/`float`/`double` and refuses `byte`/`short`/`char`/`boolean`).
pub(crate) fn reference_accepts_wrapper(reference: &str, wrapper: &str) -> bool {
    if reference == wrapper {
        return true;
    }
    match reference {
        "Ljava/lang/Object;" => true,
        "Ljava/lang/Comparable;" | "Ljava/io/Serializable;" | "Ljava/lang/constant/Constable;" => {
            primitive_desc_for_wrapper(wrapper).is_some()
        }
        "Ljava/lang/Number;" => matches!(
            wrapper,
            "Ljava/lang/Byte;"
                | "Ljava/lang/Short;"
                | "Ljava/lang/Integer;"
                | "Ljava/lang/Long;"
                | "Ljava/lang/Float;"
                | "Ljava/lang/Double;"
        ),
        "Ljava/lang/constant/ConstantDesc;" => matches!(
            wrapper,
            "Ljava/lang/Integer;" | "Ljava/lang/Long;" | "Ljava/lang/Float;" | "Ljava/lang/Double;"
        ),
        _ => false,
    }
}

/// True when a descriptor token names one of the nine primitive types
/// (`void` included — `canConvert` handles it, so it must reach the arms).
pub(crate) fn is_primitive_descriptor(tok: &str) -> bool {
    matches!(tok, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" | "V")
}

/// `MethodType.canConvert(src, dst)`, transcribed arm for arm.
///
/// Read it against the JDK source rather than against intuition: the third
/// reference->primitive test (`isWrapperType(src) && dw.isConvertibleFrom(...)`)
/// is what makes `Byte -> short` and `Character -> int` convertible while
/// `Number -> char` is not, and dropping it silently narrows 20 accepted cells
/// into refusals.
pub(crate) fn mh_can_convert(src: &str, dst: &str) -> bool {
    // Short-circuits, in the JDK's own order.
    if src == dst || src == "Ljava/lang/Object;" || dst == "Ljava/lang/Object;" {
        return true;
    }
    if is_primitive_descriptor(src) {
        // `void` forces to an explicit null or a primitive zero.
        if src == "V" {
            return true;
        }
        let Some(sw) = wrapper_desc_for_primitive(src) else {
            return false;
        };
        if is_primitive_descriptor(dst) {
            // P -> P must widen — except to `void`, which accepts every
            // primitive. The JDK spells that as `Wrapper.VOID
            // .isConvertibleFrom(sw)`; MEASURED, the whole `void` COLUMN of
            // the return matrix is accepts, so it is spelled out here rather
            // than folded into `primitive_widens_to`, where a `V` entry would
            // wrongly claim `void` is a widening of `int` in both directions.
            if dst == "V" {
                return true;
            }
            return primitive_widens_to(src, dst);
        }
        // P -> R must box and widen.
        return reference_accepts_wrapper(dst, sw);
    }
    if is_primitive_descriptor(dst) {
        // Any value can be dropped.
        if dst == "V" {
            return true;
        }
        let Some(dw) = wrapper_desc_for_primitive(dst) else {
            return false;
        };
        // R -> P must be able to unbox from a dynamically chosen type: the
        // wrapper must be cast-compatible with the source.
        if reference_accepts_wrapper(src, dw) {
            return true;
        }
        // ... or the source is strongly typed to a wrapper whose primitive
        // widens to the destination (`Byte -> short`, `Character -> int`).
        if let Some(sp) = primitive_desc_for_wrapper(src) {
            return primitive_widens_to(sp, dst);
        }
        return false;
    }
    // R -> R always works, since null is always valid dynamically.
    true
}

/// `MethodType.isConvertibleTo` on two method descriptors — the whole-signature
/// predicate `asType` gates on.
///
/// **The parameter direction is reversed and that is not a typo.** The RETURN
/// value travels old -> new (the callee produces it, the caller receives it);
/// each PARAMETER travels new -> old (the caller supplies it, the callee
/// receives it). Getting this backwards passes the primitive-widening rows and
/// fails on every narrowing one, which is a diff that looks like an off-by-one
/// rather than a reversal.
///
/// Arity is checked first and exactly: `asType` never adds or drops a
/// parameter (`asCollector`/`asSpreader`/`bindTo` do), so all five measured
/// arity rows refuse.
pub(crate) fn method_type_is_convertible_to(old_desc: &str, new_desc: &str) -> Option<bool> {
    let (old_params, old_ret) = split_descriptor_params(old_desc)?;
    let (new_params, new_ret) = split_descriptor_params(new_desc)?;
    if old_params.len() != new_params.len() {
        return Some(false);
    }
    if !mh_can_convert(&old_ret, &new_ret) {
        return Some(false);
    }
    for (new_p, old_p) in new_params.iter().zip(old_params.iter()) {
        if !mh_can_convert(new_p, old_p) {
            return Some(false);
        }
    }
    Some(true)
}

/// One descriptor token in `Class.getSimpleName()` spelling, which is what
/// `MethodType.toString()` prints and therefore what the exception message
/// carries: `[I` -> `int[]`, `Ljava/util/Map$Entry;` -> `Entry`.
///
/// MEASURED, so the message can be transcribed rather than composed:
/// `MethodType.methodType(int[].class, String[].class, Object[][].class)`
/// prints `(String[],Object[][])int[]`, and a nested class prints its inner
/// name alone (`(Entry)Inner`).
pub(crate) fn descriptor_simple_name(tok: &str) -> Option<String> {
    descriptor_simple_name_with(tok, &|_| None)
}

/// `Class.getSimpleName()` of the LOADED class `name` (internal form), as the
/// JDK derives it: the class's own `InnerClasses` entry's `inner_name` (`""`
/// for an anonymous class, `Local` for a local `Outer$1Local`), else the
/// top-level name after the package, a `$` in it kept (`p/A$B` is `A$B`).
/// `None` when no class of that name is loaded, and the caller keeps its
/// last-`$` text. Interpreter round i1 wave 39, lane L4 (item 2 of
/// `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`).
pub(crate) fn loaded_simple_name(ctx: &dyn NativeContext, name: &str) -> Option<String> {
    simple_name_of_class(ctx, ctx.class_id_by_name(name)?, name)
}

/// [`loaded_simple_name`] of the class `id`, whose internal name is `name`.
/// `None` for a hidden class (its simple name is not derived this way).
pub(crate) fn simple_name_of_class(ctx: &dyn NativeContext, id: ClassId, name: &str) -> Option<String> {
    if ctx.is_class_hidden(id) {
        return None;
    }
    let own = ctx
        .inner_classes(id)
        .into_iter()
        .find(|(inner, _, _, _)| inner == name)
        .map(|(_, _, inner_name, _)| inner_name);
    Some(own.unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name).to_string()))
}

/// [`descriptor_simple_name`], asking `loaded` for a class's simple name
/// first (see [`loaded_simple_name`]).
pub(crate) fn descriptor_simple_name_with(
    tok: &str,
    loaded: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let dims = tok.bytes().take_while(|b| *b == b'[').count();
    let base = &tok[dims..];
    let name = match base {
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        "V" => "void".to_string(),
        other => {
            let inner = other.strip_prefix('L')?.strip_suffix(';')?;
            if inner.is_empty() {
                return None;
            }
            // A lambda proxy's own name (`unnameable_mirror_descriptor`, wave
            // 92): a top-level hidden class, whose `getSimpleName()` is its
            // name after the package -- `Host$$Lambda/0x..`, the `/` kept.
            if let Some(at) = inner.find("$$Lambda/0x") {
                let host = &inner[..at];
                let host = host.rsplit('/').next().unwrap_or(host);
                return Some(format!("{host}{}", &inner[at..]) + &"[]".repeat(dims));
            }
            match loaded(inner) {
                Some(simple) => simple,
                None => inner
                    .rsplit(['/', '$'])
                    .next()
                    .filter(|s| !s.is_empty())?
                    .to_string(),
            }
        }
    };
    Some(name + &"[]".repeat(dims))
}

/// A method descriptor in `MethodType.toString()` spelling — `(int,String)void`.
///
/// `None` when any token cannot be named, so a caller can decline to compose a
/// half-rendered message rather than print a signature with a `?` in it.
pub(crate) fn method_type_display(desc: &str) -> Option<String> {
    method_type_display_with(desc, &|_| None)
}

/// [`method_type_display`] with each LOADED class named by
/// `Class.getSimpleName()` ([`loaded_simple_name`]): what every
/// `WrongMethodTypeException` text below prints (wave 39).
pub(crate) fn method_type_display_in(ctx: &dyn NativeContext, desc: &str) -> Option<String> {
    method_type_display_with(desc, &|name| loaded_simple_name(ctx, name))
}

pub(crate) fn method_type_display_with(desc: &str, loaded: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let (params, ret) = split_descriptor_params(desc)?;
    let mut out = String::from("(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&descriptor_simple_name_with(p, loaded)?);
    }
    out.push(')');
    out.push_str(&descriptor_simple_name_with(&ret, loaded)?);
    Some(out)
}

/// The heap array kind an array whose COMPONENT descriptor is `comp` must have.
///
/// Anything that is not one of the eight primitive tokens — including `[…` and
/// `L…;` — is a reference array, which is also the safe default for a token
/// this function does not recognise.
pub(crate) fn array_element_type_of_descriptor(comp: &str) -> cratonvm_types::ArrayElementType {
    use cratonvm_types::ArrayElementType as A;
    match comp {
        DESC_BOOLEAN => A::Boolean,
        DESC_BYTE => A::Byte,
        DESC_CHAR => A::Char,
        DESC_SHORT => A::Short,
        DESC_INT => A::Int,
        DESC_LONG => A::Long,
        DESC_FLOAT => A::Float,
        DESC_DOUBLE => A::Double,
        _ => A::Reference,
    }
}

/// Apply JLS 5.3 method-invocation widening to one argument that is about to be
/// stored into a primitive array slot whose component descriptor is `comp`.
///
/// This exists because CratonVM's `MethodHandle.asType` is a passthrough shim.
/// On HotSpot the widening a collector needs is done by the `asType` the
/// combinator installs, so `asCollector(long[].class, 3).invoke(1, 2, 3)`
/// reaches the target with three `long`s; here the raw `int`s arrive at the
/// array store, and `write_prim_element`'s `Long` arm matches only
/// `Value::Long` and writes 0 for anything else. Narrowing is deliberately NOT
/// performed: `Z`/`B`/`C`/`S`/`I` all travel as `Value::Int` and
/// `write_prim_element` already truncates on the store, and a `long` handed to
/// an `int[]` collector is a type error the JDK refuses rather than silently
/// truncates.
pub(crate) fn widen_primitive_to_descriptor(v: Value, comp: &str) -> Value {
    match (comp, v) {
        (DESC_LONG, Value::Int(i)) => Value::Long(i as i64),
        (DESC_FLOAT, Value::Int(i)) => Value::Float(i as f32),
        (DESC_FLOAT, Value::Long(l)) => Value::Float(l as f32),
        (DESC_DOUBLE, Value::Int(i)) => Value::Double(i as f64),
        (DESC_DOUBLE, Value::Long(l)) => Value::Double(l as f64),
        (DESC_DOUBLE, Value::Float(f)) => Value::Double(f as f64),
        _ => v,
    }
}

/// Which primitive descriptor should drive the WRAPPER CLASS for one element
/// of a reference-component collector / varargs array, when the component
/// itself settles it — `None` when it does not.
///
/// # The measurement this is built on, and the diagnosis it corrects
///
/// F19-1 §7 N2 states that fixing the collector arms *"needs the target
/// handle's `MethodType`"*. **MEASURED on OpenJDK 25.0.3+9 (`CollBox.java`,
/// byte-identical over three runs, under `-Xint`, and under
/// `-XX:-UseCompressedOops`), that is not where the information is:**
///
/// ```text
/// coll = firstOf(Object[])Object . asCollector(Object[].class, 1)
/// coll.type()                 = (Object)Object      <-- the MethodType
/// coll.invoke(aChar)  .getClass() = java.lang.Character
/// coll.invoke(anInt)  .getClass() = java.lang.Integer
/// coll.invoke(aBool)  .getClass() = java.lang.Boolean
/// coll.asType((char)Object).invoke(aChar).getClass() = java.lang.Character
/// coll.asType((int)Object) .invoke(anInt) .getClass() = java.lang.Integer
/// ```
///
/// The handle's `MethodType` says `Object` for every one of those rows, and
/// the answers still differ. The wrapper class is chosen by the **call site's
/// static parameter type** — the descriptor of the signature-polymorphic
/// `invoke`, or the `asType` adapter's parameter when one is interposed. The
/// target's `MethodType` is `(Object[])Object` and could not distinguish them
/// either. So the `Object[]` case is NOT fixable at this arm and is nominated
/// against `vm/src/vm/vm_exec.rs`, which is where the call-site descriptor
/// exists (it is already read there as `descriptor`, by
/// `unbox_poly_return_checked`) and is not passed to the native.
///
/// # What IS settled here, and why it is total rather than a heuristic
///
/// When the array component is one of the eight wrapper classes, HotSpot
/// **refuses** every call whose static argument type is not that wrapper's
/// primitive, so the component pins the answer for every call that runs at
/// all. MEASURED (`CollBox2.java`):
///
/// | collector | from `char` | from `int` | from `long` |
/// |---|---|---|---|
/// | `Character[]` | `Character` | `WrongMethodTypeException` | — |
/// | `Integer[]` | `WrongMethodTypeException` | `Integer` | `WrongMethodTypeException` |
/// | `Long[]` | — | `WrongMethodTypeException` | `Long` |
/// | `Boolean[]`/`Byte[]`/`Short[]`/`Float[]` | — | per type | — |
/// | `Object[]`/`Comparable[]` | `Character` | `Integer` | `Long` |
///
/// `Number[]` behaves like `Object[]` for the arms it accepts and refuses the
/// rest (`Number` from `char` is a `WrongMethodTypeException`), so it is a
/// `None` here rather than a ninth row — a reference component that is not
/// itself a wrapper carries no primitive.
///
/// The `Value` variant is matched as well as the descriptor, for the same
/// reason `box_value_canonical` matches it: `native_long_value_of` reads
/// `Some(Value::Long(v))` and defaults to 0, so a `("Ljava/lang/Long;",
/// Value::Int(5))` pair routed on the descriptor alone would box **0**. That
/// pair is not a shape HotSpot accepts anyway (`LongComp.fromInt` throws), so
/// declining it costs nothing and falling back to the variant keeps today's
/// answer.
///
/// `F`/`D` are returned like the rest; the CALL SITES decide to spell those
/// two `box_value`, because `Float`/`Double` have no cache on HotSpot
/// (MEASURED `FloatComp.id` = false, matching `neg.floatValueOf` = false).
pub(crate) fn collector_element_box_desc(component: &str, v: Value) -> Option<&'static str> {
    let prim = match component {
        "Ljava/lang/Character;" => DESC_CHAR,
        "Ljava/lang/Boolean;" => DESC_BOOLEAN,
        "Ljava/lang/Byte;" => DESC_BYTE,
        "Ljava/lang/Short;" => DESC_SHORT,
        "Ljava/lang/Integer;" => DESC_INT,
        "Ljava/lang/Long;" => DESC_LONG,
        "Ljava/lang/Float;" => DESC_FLOAT,
        "Ljava/lang/Double;" => DESC_DOUBLE,
        _ => return None,
    };
    match (prim, v) {
        (DESC_CHAR | DESC_BOOLEAN | DESC_BYTE | DESC_SHORT | DESC_INT, Value::Int(_)) => Some(prim),
        (DESC_LONG, Value::Long(_)) => Some(prim),
        (DESC_FLOAT, Value::Float(_)) => Some(prim),
        (DESC_DOUBLE, Value::Double(_)) => Some(prim),
        _ => None,
    }
}

/// The call site's own static parameter type for one collected argument,
/// resolved to the primitive descriptor it names.
///
/// F29-1 §2.3 / NOMINATION 2: an `Object[]`-component collector settles
/// nothing at the array, and the wrapper class HotSpot produces comes from
/// what the CALLER wrote at the call site, not from the collector or the
/// runtime `Value` variant (`char` and `int` are the same variant carrying
/// the same bits, and HotSpot boxes them differently). `entry_call_site` is
/// the descriptor `MethodHandle.invoke`'s native armed via
/// `cratonvm_native_api::poly_call_site` / `take_entry_call_site` before
/// calling into `mh_dispatch` — the one channel a native has to that type at
/// all. A miss (no call site armed, index out of range, or a reference
/// parameter) answers `None` and the caller falls back to the `Value`
/// variant, exactly as before this existed.
pub(crate) fn collector_call_site_prim(entry_call_site: Option<&str>, index: usize) -> Option<&'static str> {
    let cs = entry_call_site?;
    let (ptypes, _) = crate::lang_class::parse_descriptor_param_and_return(cs);
    match ptypes.get(index).map(String::as_str) {
        Some("C") => Some(DESC_CHAR),
        Some("Z") => Some(DESC_BOOLEAN),
        Some("B") => Some(DESC_BYTE),
        Some("S") => Some(DESC_SHORT),
        Some("I") => Some(DESC_INT),
        _ => None,
    }
}

/// Box one element of a reference-component collector / varargs array.
///
/// The single implementation of the rule `mh_dispatch`'s `MH_KIND_COLLECT` arm
/// and `build_varargs_array`'s reference arm both need. It was two copies
/// before, which is how they came to disagree with each other in the first
/// place, and the whole of [`collector_element_box_desc`]'s doc comment is the
/// justification for both.
///
/// `call_site_prim` is [`collector_call_site_prim`]'s answer for this element
/// — `Some` only when the component itself settled nothing AND the call site
/// is known — and is consulted BEFORE the `Value`-variant fallback below, per
/// F29-1 NOMINATION 2.
pub(crate) fn box_collector_element(
    ctx: &mut dyn NativeContext,
    v: Value,
    component: &str,
    call_site_prim: Option<&'static str>,
) -> Value {
    if let Some(desc) = collector_element_box_desc(component, v) {
        // `F`/`D` stay on `box_value`: HotSpot caches neither, and
        // `box_value_canonical` would only hand them straight back here. The
        // spelling is what keeps the measured asymmetry readable at the site.
        return if desc == DESC_FLOAT || desc == DESC_DOUBLE {
            crate::lang_class::box_value(ctx, v, desc)
        } else {
            crate::lang_class::box_value_canonical(ctx, v, desc)
        };
    }
    // The component does not name a wrapper class (`Object[]` is the ordinary
    // case). The call site the caller wrote might still settle it — see
    // [`collector_call_site_prim`] — and takes priority over the runtime
    // `Value` variant fallback below, which is a KNOWN wrong answer for
    // `char`/`boolean`/`byte`/`short` (all `Value::Int`) when it is not.
    if let (Value::Int(_), Some(desc)) = (v, call_site_prim) {
        return crate::lang_class::box_value_canonical(ctx, v, desc);
    }
    // No component and no call site: the only type information left is the
    // runtime `Value` variant. It is a wrong CLASS, not a wrong identity, and
    // caching it does not make it wronger: the same object graph, one
    // allocation cheaper.
    match v {
        Value::Int(_) => crate::lang_class::box_value_canonical(ctx, v, DESC_INT),
        Value::Long(_) => crate::lang_class::box_value_canonical(ctx, v, DESC_LONG),
        Value::Float(_) => crate::lang_class::box_value(ctx, v, DESC_FLOAT),
        Value::Double(_) => crate::lang_class::box_value(ctx, v, DESC_DOUBLE),
        other => other,
    }
}

/// The JVM method descriptor of a `java.lang.invoke.MethodType` object (the
/// JDK's, or the synthetic stand-in), `None` when a component mirror cannot be
/// named. For the VM's native `LambdaMetafactory` linkage, whose static
/// argument may be a dynamic constant that answered a `MethodType`
/// (interpreter round i1 wave 41, lane L4; row 4 of
/// `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`).
pub fn method_type_object_descriptor(ctx: &dyn NativeContext, mt: ObjectRef) -> Option<String> {
    crate::link_to_method_descriptor(ctx, mt)
}

/// Read a MethodType object's effective JVM descriptor by converting its
/// `ptypes` (Class[]) and `rtype` (Class) mirrors back to descriptor tokens.
pub(crate) fn methodtype_to_descriptor(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
    if mh_type_render_memo_enabled() {
        return methodtype_to_descriptor_memoised(ctx, mt);
    }
    methodtype_to_descriptor_rendered(ctx, mt)
}

/// `CRATONVM_MH_TYPE_RENDER_MEMO` -- default OFF (JIT round 16, lane mh;
/// proposal MH16-3): [`methodtype_to_descriptor`] answers from a per-thread
/// memo ([`TYPE_RENDER_MEMO`]) keyed by the VM, the collection epoch and the
/// `MethodType`'s address, instead of resolving every parameter mirror's name
/// again. `bindTo`'s leading-parameter guard renders its target's `type` on
/// every bind (`R16MhFloor` `bind-only` / `bind-each` / `bind-static`), and the
/// ordinary doors render the declared type on every call (`exact-astyped`,
/// `iwa`); the target's `type` is one object, and with
/// `CRATONVM_MH_BOUND_TYPE_TABLE` a bound type is too. `1`/`true`/`on`/`yes`
/// turns it on; opt-in until measured. Read once.
pub(crate) fn mh_type_render_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_MH_TYPE_RENDER_MEMO"))
}

/// One direct-mapped line of [`TYPE_RENDER_MEMO`].
pub(crate) struct TypeRenderLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) mt: usize,
    pub(crate) desc: Option<std::rc::Rc<str>>,
}

/// Slots in [`TYPE_RENDER_MEMO`]; a power of two (the index is a mask).
pub(crate) const TYPE_RENDER_MEMO_SLOTS: usize = 32;

thread_local! {
    /// Per-thread memo of rendered `MethodType` descriptors (MH16-3). The
    /// rule is [`DIRECT_LANE_MEMO`]'s: within one collection epoch nothing
    /// moves or is freed, so an equal address is the same `MethodType`, and a
    /// `MethodType`'s `rtype` / `ptypes` are never rewritten after it is
    /// built (the JDK's are final; this file stores into a `MethodType` only
    /// while minting it, before any reader has it). A context reporting VM
    /// identity `0` (the mocks) is never memoised.
    pub(crate) static TYPE_RENDER_MEMO: std::cell::RefCell<Vec<TypeRenderLine>> =
        std::cell::RefCell::new(
            (0..TYPE_RENDER_MEMO_SLOTS)
                .map(|_| TypeRenderLine { vm: 0, epoch: 0, mt: 0, desc: None })
                .collect(),
        );
}

/// [`methodtype_to_descriptor`] through [`TYPE_RENDER_MEMO`]. The epoch is read
/// BEFORE the render, so a render that collected (and moved `mt`) is stored
/// under an epoch no later lookup reads. Only a rendered answer is stored.
pub(crate) fn methodtype_to_descriptor_memoised(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return methodtype_to_descriptor_rendered(ctx, mt);
    }
    let epoch = ctx.gc_address_epoch();
    let addr = mt.as_ptr() as usize;
    let slot = (addr >> 3) & (TYPE_RENDER_MEMO_SLOTS - 1);
    let hit = TYPE_RENDER_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.epoch == epoch && line.mt == addr {
            line.desc.clone()
        } else {
            None
        }
    });
    if let Some(desc) = hit {
        return Some(String::from(&*desc));
    }
    let rendered = methodtype_to_descriptor_rendered(ctx, mt)?;
    let shared: std::rc::Rc<str> = std::rc::Rc::from(rendered.as_str());
    TYPE_RENDER_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = TypeRenderLine {
            vm,
            epoch,
            mt: addr,
            desc: Some(shared),
        };
    });
    Some(rendered)
}

/// The render itself: `(` each `ptypes` mirror's descriptor `)` the `rtype`'s.
pub(crate) fn methodtype_to_descriptor_rendered(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
    let ptypes = match ctx.get_field_by_name(mt, "ptypes") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let mut s = String::from("(");
    let n = ctx.array_length(ptypes);
    for i in 0..n {
        match ctx.get_array_element(ptypes, i) {
            Value::Object(Some(p)) => s.push_str(&mirror_to_descriptor(ctx, p)),
            _ => return None,
        }
    }
    s.push(')');
    match ctx.get_field_by_name(mt, "rtype") {
        Value::Object(Some(r)) => s.push_str(&mirror_to_descriptor(ctx, r)),
        _ => s.push('V'),
    }
    Some(s)
}

/// A MethodHandle's effective `type()` descriptor — the ADAPTED MethodType
/// (which reflects receiver-prepend, bindTo/insertArguments drops, asCollector
/// spreads, …), not the raw bytecode `MH_DESC`. Falls back to `MH_DESC` when no
/// `type` MethodType has been installed. Combinators must chain off this so a
/// stack of adapters tracks arity correctly.
/// The leaf descriptor an ADAPTER over `target` records (`insertArguments`,
/// `asCollector`, `asSpreader`). A constructor handle's `MH_DESC` is the
/// constructor's own `(args)V`, which the invoke doors box to `null`, so an
/// adapter that copied it made `insertArguments(ctor, ..)` / `ctor.asSpreader(..)`
/// answer `null` where HotSpot answers the new object -- the shape Groovy's
/// indy `init` call sites build (`ThymeleafServletAutoConfigurationTests`:
/// `LayoutDialect.getProcessors` could not construct a `DecorateProcessor`).
/// Such an adapter carries the constructor's `type` descriptor (`(args)Owner`),
/// as `bindTo`'s insert adapter already does.
pub(crate) fn mh_adapter_desc(ctx: &mut dyn NativeContext, target: ObjectRef) -> Option<String> {
    let kind = match ctx.get_field(target, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    if kind == MH_KIND_CONSTRUCTOR {
        if let Some(d) = mh_type_descriptor(ctx, target) {
            return Some(d);
        }
    }
    mh_read_desc(ctx, target)
}

pub(crate) fn mh_type_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") {
        if let Some(d) = methodtype_to_descriptor(ctx, mt) {
            return Some(d);
        }
    }
    mh_read_desc(ctx, mh)
}

// ---------------------------------------------------------------------------
// Downcall carriers through the converting doors (JIT round 16 wave 11, lane
// mhbind; `r16-ffmcost-downcall-invoke-door-answers-null-FIXED-20261001.md`)
// ---------------------------------------------------------------------------
//
// A `Linker.downcallHandle` carrier (`panama::alloc_downcall_handle`) has none
// of this file's synthetic slots: no `MH_KIND` (read as VIRTUAL), no
// `MH_DESC`. The `invoke` door therefore took its first argument for a
// receiver and passed it, boxed, to the C call, and boxed the result against
// an empty descriptor: `abs.invoke(Integer.valueOf(-4))` ran `abs(0)` and
// answered `null`, `atan2.invokeWithArguments(0.0, -1.0)` answered `null`, and
// `asType` answered the carrier unchanged, so an adapted `invokeExact` was
// refused. HotSpot's carrier is an ordinary handle of its `type`: the doors now
// treat it as a STATIC handle of that type ([`mh_downcall_door_type`]), the
// dispatch converts its arguments to the C function's own types the way a
// static leaf does ([`mh_downcall_adapted_call`]), and `asType` mints a copy
// that records those types ([`mh_downcall_as_type`]). The plain `invokeExact`
// of an unadapted carrier (no entry call site, no recorded leaf) is untouched.

/// `CRATONVM_MH_DOWNCALL_INVOKE_ADAPT` -- default ON (JIT round 16 wave 11,
/// lane mhbind; a correctness fix, both modes). `0`/`false`/`off`/`no`
/// restores the old doors: boxed arguments handed to the C call, the result
/// boxed against nothing, and `asType` of a downcall a passthrough. Read once.
pub(crate) fn mh_downcall_adapt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_DOWNCALL_INVOKE_ADAPT")
    })
}

/// The slot an `asType` copy of a downcall carrier keeps its C function's own
/// method descriptor in (a `String`; [`mh_downcall_as_type`]). The first slot
/// past this file's synthetic window ([`MH_SLOT_COUNT`]) and below
/// `panama::DOWNCALL_BASE`, which `alloc_downcall_handle` null-fills, so an
/// unadapted carrier reads `null` there.
pub(crate) const MH_DOWNCALL_LEAF_DESC: usize = MH_SLOT_COUNT;

/// The C function's own descriptor an `asType` copy recorded, or `None` (an
/// unadapted carrier, whose `type` is that descriptor).
pub(crate) fn mh_downcall_leaf_desc(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if ctx.object_num_fields(mh) <= MH_DOWNCALL_LEAF_DESC {
        return None;
    }
    match ctx.get_field(mh, MH_DOWNCALL_LEAF_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// A downcall carrier's own `type` descriptor, for the `invoke` /
/// `invokeWithArguments` doors, which then treat it as a `STATIC` handle whose
/// leaf descriptor is that type; `None` for every other handle (and with the
/// switch off).
pub(crate) fn mh_downcall_door_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if !mh_downcall_adapt_enabled() || !crate::panama::is_downcall_handle(&*ctx, mh) {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

/// A real JDK `MethodHandle` with none of this model's slots (a
/// `DirectMethodHandle`, a `BoundMethodHandle` species, ... that
/// [`mh_run_foreign_vmentry`] runs), for the `invoke` / `invokeExact` /
/// `invokeWithArguments` doors, which then treat it, as a downcall carrier
/// ([`mh_downcall_door_type`]), as a `STATIC` handle whose leaf descriptor is
/// its own `type`: `asType`'s refusals are judged against that type, the
/// arguments reach the dispatch as passed (it converts them), and the result
/// is boxed or widened from that type's return. The doors used to read this
/// model's absent slots as an unbound VIRTUAL handle of no descriptor: a
/// `null` first argument was the receiver's NPE, the call site lost its first
/// parameter, and the result went back unboxed (interpreter round i2 wave 54,
/// lane L4; `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
///
/// `None` with `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY=0`, for a handle with
/// this model's slots or downcall state, under `CRATONVM_PURE_BYTECODE` (whose
/// own real-handle lanes run first), and for `MethodHandleImpl$AsVarargsCollector`,
/// whose `asType` collects at another arity. One width compare for every other
/// handle.
pub(crate) fn mh_foreign_door_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if ctx.object_num_fields(mh) > MH_BOUND {
        return None;
    }
    mh_foreign_door_type_cold(ctx, mh)
}

#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_door_type_cold(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if !crate::nbflags().mh_foreign_handle_runs_vmentry
        || cratonvm_native_api::unretire::pure_bytecode()
        || crate::panama::is_downcall_handle(&*ctx, mh)
    {
        return None;
    }
    let handle_class = ctx.class_id_by_name("java/lang/invoke/MethodHandle")?;
    let cid = ctx.class_id_of_object(mh);
    if !ctx.is_subclass(cid, handle_class)
        || ctx.class_name_arc_of_id(cid).as_deref()
            == Some("java/lang/invoke/MethodHandleImpl$AsVarargsCollector")
    {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

/// Does an adapter native (`asType`, `bindTo`) hand this receiver to the
/// JDK's own body? A real JDK `MethodHandle` with none of this model's slots,
/// not a downcall carrier, outside `CRATONVM_PURE_BYTECODE`, with
/// `CRATONVM_MH_FOREIGN_ADAPTERS_RUN_JDK` on (interpreter round i2 wave 54,
/// lane L4). One width compare for every other receiver.
pub(crate) fn mh_foreign_adapts_through_jdk(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) <= MH_BOUND && mh_foreign_adapts_through_jdk_cold(ctx, this)
}

#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_adapts_through_jdk_cold(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    if !crate::nbflags().mh_foreign_adapters_run_jdk
        || cratonvm_native_api::unretire::pure_bytecode()
        || crate::panama::is_downcall_handle(&*ctx, this)
    {
        return false;
    }
    ctx.class_id_by_name("java/lang/invoke/MethodHandle")
        .is_some_and(|handle_class| ctx.is_subclass(ctx.class_id_of_object(this), handle_class))
}

/// MethodType equality is Class identity, including hidden classes and loader
/// identity. Rendering a synthetic lambda mirror falls back to Object, so a
/// descriptor-string shortcut can silently discard a required adaptation.
pub(crate) fn method_types_same_classes(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> bool {
    if a == b {
        return true;
    }
    let (
        Value::Object(Some(ar)),
        Value::Object(Some(br)),
        Value::Object(Some(ap)),
        Value::Object(Some(bp)),
    ) = (
        ctx.get_field_by_name(a, "rtype"),
        ctx.get_field_by_name(b, "rtype"),
        ctx.get_field_by_name(a, "ptypes"),
        ctx.get_field_by_name(b, "ptypes"),
    )
    else {
        return false;
    };
    if ar != br || ctx.array_length(ap) != ctx.array_length(bp) {
        return false;
    }
    (0..ctx.array_length(ap)).all(|i| {
        matches!(
        (ctx.get_array_element(ap, i), ctx.get_array_element(bp, i)),
        (Value::Object(Some(x)), Value::Object(Some(y))) if x == y)
    })
}

/// `asType(newType)` of a real JDK handle ([`mh_foreign_adapts_through_jdk`]):
/// the receiver itself for its own type, else the JDK's own
/// `MethodHandle.asType` bytecode (`asTypeUncached` -> `makePairwiseConvert`,
/// a new handle). `None` for any other receiver.
///
/// This model's `asType` copies a handle's slots and stamps the new `type`
/// on the copy; a real handle has no slots to copy (`mh_clone_handle` answers
/// `None`), so `mh_with_stamped_type` stamped it IN PLACE: the JDK's cached
/// handle (`MethodType.invokers()`' invokers are shared) took the new type
/// with its old form, and every later caller saw the wrong `type()`
/// (`L4W54RealJdkHandleArgs` rows `astype-*`). An `AsVarargsCollector` is
/// included: the JDK's own body is the one that collects.
pub(crate) fn mh_foreign_as_type(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallResult> {
    if !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if matches!(ctx.get_field_by_name(this, "type"), Value::Object(Some(old))
        if method_types_same_classes(&*ctx, old, new_type))
    {
        return Some(Ok(Some(Value::Object(Some(this)))));
    }
    if let Some(cached) = mh_foreign_as_type_cache_hit(ctx, this, new_type) {
        return Some(Ok(Some(Value::Object(Some(cached)))));
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] asType of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    Some(ctx.invoke_virtual_bytecode_only(
        this,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(new_type))],
    ))
}

/// Interpreter round i5 wave 114, lane L4 (`CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ`,
/// default off; page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`):
/// the JDK's `asTypeCached` first test, read here -- `asTypeCache` non-null
/// and its `type` the requested `MethodType` by identity answers that
/// adapter. Pure reads, so the JDK body would answer the same object with no
/// side effect. `None` (the JDK's `asType` bytecode runs) for a miss, the
/// switch off, and the `asTypeSoftCache` arm (`SoftReference.get` updates the
/// reference's clock, so that arm stays the JDK's).
///
/// Why it pays: `MethodHandle.asType` carries its own registered `Intrinsic`
/// (this file), so `interpreter::execute`'s first-call JIT block seals it
/// `native_skip` (`calls-native-shadowed-method`) and never counts it. The
/// [`mh_foreign_as_type`] re-entry (`invoke_virtual_bytecode_only`) therefore
/// interprets the JDK body on every call: the w113 census read `asType` `cold`
/// on all 16 000 re-entries, 11.0 us each, once per `invokeWithArguments`
/// body.
pub(crate) fn mh_foreign_as_type_cache_hit(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> Option<ObjectRef> {
    if !crate::nbflags().mh_foreign_as_type_cache_read {
        return None;
    }
    let Value::Object(Some(cached)) = ctx.get_field_by_name(this, "asTypeCache") else {
        return None;
    };
    match ctx.get_field_by_name(cached, "type") {
        Value::Object(Some(t)) if t == new_type => Some(cached),
        _ => None,
    }
}

/// The JDK's own `asType(new_type)` on a real handle, as the natives that
/// adapt one call it (`invoke_virtual_bytecode_only`). Under
/// `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` the body's two pure-read answers
/// come first: `newType == type` answers the handle itself, and an
/// `asTypeCache` hit answers the cached adapter
/// ([`mh_foreign_as_type_cache_hit`]). Interpreter round i5 wave 114, lane L4.
pub(crate) fn mh_real_as_type_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_type: ObjectRef,
) -> MethodCallResult {
    if crate::nbflags().mh_foreign_as_type_cache_read {
        if matches!(ctx.get_field_by_name(this, "type"), Value::Object(Some(t)) if t == new_type) {
            return Ok(Some(Value::Object(Some(this))));
        }
        if let Some(cached) = mh_foreign_as_type_cache_hit(ctx, this, new_type) {
            return Ok(Some(Value::Object(Some(cached))));
        }
    }
    ctx.invoke_virtual_bytecode_only(
        this,
        "asType",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(new_type))],
    )
}

/// `bindTo(x)` of a real JDK handle ([`mh_foreign_adapts_through_jdk`]): the
/// JDK's own body (`leadingReferenceParameter().cast(x)`, then
/// `bindArgumentL(0, x)`, a new `BoundMethodHandle`), where this model's
/// `bindTo` read the receiver's absent slots. `None` for any other receiver.
pub(crate) fn mh_foreign_bind_to(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    value: Value,
) -> Option<MethodCallResult> {
    if !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] bindTo of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    Some(ctx.invoke_virtual_bytecode_only(
        this,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        &[value],
    ))
}

/// The rest of the adapter natives over a real JDK handle
/// ([`mh_foreign_adapts_through_jdk`], and `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK`):
/// `asVarargsCollector`, `withVarargs`, `asCollector` and `asSpreader` (both
/// arities) answer the JDK's own body (`MethodHandleImpl.makeVarargsCollector`,
/// `rebind().editor()...`, a new handle). This model's natives read the
/// receiver's absent slots: `asVarargsCollector`'s width-guarded marking was a
/// no-op, so the answer was the receiver itself, which does not collect; the
/// collector and spreader minted a carrier over a handle of no descriptor.
/// `args[0]` is the receiver, the rest the method's own arguments. `None` for
/// any other receiver. Interpreter round i2 wave 55, lane L4 (proposal
/// `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
/// `asFixedArity` / `isVarargsCollector` need no door: the JDK's base answers
/// (`this`, `false`) are this model's for a slotless receiver, and a real
/// `AsVarargsCollector` declares both itself, so its own bytecode runs.
pub(crate) fn mh_foreign_adapter_through_jdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    if ctx.object_num_fields(this) > MH_BOUND {
        return None;
    }
    mh_foreign_adapter_through_jdk_cold(ctx, this, &args[1..], name, desc)
}

#[cold]
#[inline(never)]
pub(crate) fn mh_foreign_adapter_through_jdk_cold(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    rest: &[Value],
    name: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    // `asCollector` / `asSpreader` have their own switch: the JDK's bodies
    // answer a `BoundMethodHandle` over a spun `LambdaForm`, which this VM
    // cannot run outside `CRATONVM_PURE_BYTECODE` (the held
    // `InvokerBytecodeGenerator` natives fabricate its entry), where this
    // model's carrier over the real handle runs (measured wave 55,
    // `L4W55RealJdkAdapters` rows `collector-invoke`, `spreader-invoke`).
    let on = if matches!(name, "asCollector" | "asSpreader") {
        crate::nbflags().mh_foreign_collectors_run_jdk
    } else {
        crate::nbflags().mh_foreign_varargs_run_jdk
    };
    if !on || !mh_foreign_adapts_through_jdk(ctx, this) {
        return None;
    }
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[MH_FOREIGN] {name} of {} through the JDK's own body",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default()
        );
    }
    // JIT round 21 lane reflect (correctness, unconditional; DEPTH21-3 of
    // `jit-r21-depth-proposals.md`, audit
    // `jr21-reflect-own-body-virtual-fallbacks-FIXED-20261015.md`): `MethodHandle`'s
    // OWN body, by `invokespecial`. These natives are registered on
    // `MethodHandle`'s own triples, so they run only where that method is the
    // one selected -- a receiver that does not override it, or an override's
    // `super.m()`. A virtual re-selection sent the second case straight back
    // into the override: JDK 25's `MethodHandleImpl$IntrinsicMethodHandle`
    // overrides `asCollector(Class,int)` and calls `super.asCollector` for
    // every intrinsic but `IDENTITY` (`MethodHandles.arrayElementGetter(..)
    // .asCollector(..)`), which then recursed until `StackOverflowError` under
    // `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK=1` (opt-in), wherever this
    // `Bridge` row serves the call, where HotSpot answers a handle. Every
    // other receiver reaching here selected `MethodHandle`'s own method, so
    // its answer is unchanged.
    let mut call = Vec::with_capacity(rest.len() + 1);
    call.push(Value::Object(Some(this)));
    call.extend_from_slice(rest);
    Some(ctx.invoke_special_bytecode_only("java/lang/invoke/MethodHandle", name, desc, &call))
}

/// `MH_KIND` of a handle with this model's slots, `-1` (no kind) for any other.
pub(crate) fn mh_kind_width_guarded(ctx: &dyn NativeContext, mh: ObjectRef) -> i32 {
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return -1;
    }
    match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => -1,
    }
}

/// `MH_KIND` as the doors default it (`MH_KIND_VIRTUAL` when absent), without
/// reading past a real JDK handle's last slot (a heap OOB warning per call;
/// interpreter round i2 wave 56, lane L4: a real `AsVarargsCollector`, which
/// [`mh_foreign_door_type`] leaves to the doors, reached them).
pub(crate) fn mh_kind_or_virtual(ctx: &dyn NativeContext, mh: ObjectRef) -> i32 {
    match mh_kind_width_guarded(ctx, mh) {
        -1 => MH_KIND_VIRTUAL,
        k => k,
    }
}

/// Does `mh` carry a bound value in `MH_BOUND`? `false` for a handle without
/// this model's slots (see [`mh_kind_or_virtual`]).
pub(crate) fn mh_has_bound_guarded(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    ctx.object_num_fields(mh) > MH_BOUND
        && matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
}

/// The `(MH_DESC, MH_KIND)` the `invokeWithArguments` doors read, with a
/// downcall carrier read as [`mh_downcall_door_type`] says, and a real JDK
/// handle as [`mh_foreign_door_type`] says.
pub(crate) fn mh_door_desc_and_kind(ctx: &mut dyn NativeContext, mh: ObjectRef) -> (String, i32) {
    if let Some(own) = mh_downcall_door_type(ctx, mh) {
        return (own, MH_KIND_STATIC);
    }
    if let Some(own) = mh_foreign_door_type(ctx, mh) {
        return (own, MH_KIND_STATIC);
    }
    let desc = mh_read_desc(ctx, mh).unwrap_or_default();
    (desc, mh_kind_or_virtual(&*ctx, mh))
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): does this file's door do
/// anything to a downcall carrier's `invokeExact` beyond `mh_dispatch`'s
/// downcall arm, so the JIT's bound downcall row
/// (`panama::jit_downcall_bind_target`) must refuse it? `true` for an `asType`
/// copy (a recorded leaf descriptor, [`MH_DOWNCALL_LEAF_DESC`]: the door
/// converts its arguments and result) and for any marking bit (a collector, an
/// explicit-cast stamp, a cast chain, an exact invoker: `mh_dispatch` judges
/// those). Reads only.
pub(crate) fn mh_downcall_bind_screens_refuse(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    let adapted_copy = ctx.object_num_fields(mh) > MH_DOWNCALL_LEAF_DESC
        && matches!(
            ctx.get_field(mh, MH_DOWNCALL_LEAF_DESC),
            Value::Object(Some(_))
        );
    adapted_copy || mh_marking_bits(ctx, mh) != 0
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): the handle's declared `type`
/// rendered, as the `invokeExact` door's `exact_call_site_refusal` compares it
/// with the call site ([`mh_declared_descriptor`]).
pub(crate) fn mh_downcall_bind_declared_type(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<String> {
    mh_declared_descriptor(ctx, mh)
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): is an entry call site armed
/// on this thread ([`MH_ENTRY_CALL_SITE`])? The next `mh_dispatch` takes it, so
/// a bound downcall call that finds one runs `mh_dispatch` as the door would.
pub(crate) fn mh_entry_call_site_armed() -> bool {
    MH_ENTRY_CALL_SITE.with(|f| f.borrow().is_some())
}

/// The downcall arm's call when there is something to convert, or `None` (the
/// plain `invokeExact` of an unadapted carrier, which the arm makes as before).
/// `entry_site` is the call site an `invoke` / `invokeWithArguments` door armed.
///
/// The steps are the static leaf arm's: the entry adaptation to the handle's
/// `type` (its arity `WrongMethodTypeException` and casts, [`mh_entry_adapt`]),
/// then [`adapt_leaf_args`] to the C function's own descriptor (unboxing,
/// widening, the `null`-to-primitive `NullPointerException`), the call, and for
/// an `asType` copy the result converted from the C function's return to the
/// copy's ([`mh_downcall_outer_return`]). `mh` is pinned across the
/// conversions (a refusal, a box) and re-read for the call.
pub(crate) fn mh_downcall_adapted_call(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    extra: &[Value],
    entry_site: Option<&str>,
) -> Option<MethodCallResult> {
    let recorded = mh_downcall_leaf_desc(&*ctx, mh);
    if entry_site.is_none() && recorded.is_none() {
        return None;
    }
    let outer = mh_declared_descriptor(ctx, mh)?;
    let leaf = recorded.unwrap_or_else(|| outer.clone());
    let mh_pin = ctx.pin_native_root(mh);
    let answer = mh_downcall_adapted_call_pinned(ctx, mh_pin, mh, extra, entry_site, &outer, &leaf);
    ctx.unpin_native_roots(mh_pin);
    Some(answer)
}

/// The body of [`mh_downcall_adapted_call`], `mh` rooted by `mh_pin`.
pub(crate) fn mh_downcall_adapted_call_pinned(
    ctx: &mut dyn NativeContext,
    mh_pin: usize,
    mh: ObjectRef,
    extra: &[Value],
    entry_site: Option<&str>,
    outer: &str,
    leaf: &str,
) -> MethodCallResult {
    let full = match entry_site {
        Some(site) => mh_entry_adapt(ctx, mh, site, outer, false, extra)?,
        None => extra.to_vec(),
    };
    let site = leaf_site_params(Some(entry_site.unwrap_or(outer)), 0, full.len());
    let adapted = adapt_leaf_args(ctx, &full, leaf, site.as_deref(), false)?;
    let mh = ctx.read_native_pin(mh_pin, mh);
    let mut args = Vec::with_capacity(adapted.len() + 1);
    args.push(Value::Object(Some(mh)));
    args.extend_from_slice(&adapted);
    let result = crate::panama::pe_downcall_invoke(ctx, &args);
    mh_downcall_outer_return(ctx, result, leaf, outer)
}

/// The C function's result (`leaf`'s return) as the handle's `type` returns it
/// (`outer`): unchanged when the two agree (an unadapted carrier); a primitive
/// widened or narrowed as `asType` converts it, a value boxed for a reference
/// return, a `void` answered `null` ([`box_return_against_target`]).
pub(crate) fn mh_downcall_outer_return(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
    leaf: &str,
    outer: &str,
) -> MethodCallResult {
    let (from, to) = (return_type_desc(leaf), return_type_desc(outer));
    if from == to {
        return result;
    }
    let (Some(f), Some(t)) = (from.bytes().next(), to.bytes().next()) else {
        return result;
    };
    match result {
        Ok(Some(v)) if !matches!(v, Value::Object(_)) => {
            match widen_return_value(v, f, t).or_else(|| narrow_return_value(v, f, t)) {
                Some(converted) => Ok(Some(converted)),
                // `byte` / `short` / `char` / `int` share `Value::Int`: a
                // widening between them converts nothing.
                None if b"ZBCSIJFD".contains(&t) => Ok(Some(v)),
                None => box_return_against_target(ctx, Ok(Some(v)), leaf, Some(outer)),
            }
        }
        other => box_return_against_target(ctx, other, leaf, Some(outer)),
    }
}

/// `asType(newType)` of a downcall carrier (`mt` non-null): the carrier itself
/// for its own type (HotSpot's `newType == type` line), `asType`'s
/// `WrongMethodTypeException` for a type it does not convert to
/// ([`mh_astype_refusal`]), otherwise a copy ([`mh_clone_handle`]: every slot,
/// the downcall state included) whose `type` is `mt` and which records the C
/// function's own descriptor in [`MH_DOWNCALL_LEAF_DESC`] (kept from the
/// receiver when it is itself a copy). A type that does not read keeps the old
/// passthrough.
pub(crate) fn mh_downcall_as_type(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mt: ObjectRef,
) -> MethodCallResult {
    let unchanged = Ok(Some(Value::Object(Some(this))));
    let (Some(old), Some(new)) = (
        mh_declared_descriptor(ctx, this),
        methodtype_to_descriptor(ctx, mt),
    ) else {
        return unchanged;
    };
    if old == new {
        return unchanged;
    }
    if let Some(refusal) = mh_astype_refusal(ctx, this, mt) {
        return Err(refusal);
    }
    let leaf = mh_downcall_leaf_desc(&*ctx, this).unwrap_or(old);
    let mt_pin = ctx.pin_native_root(mt);
    let Some(copy) = mh_clone_handle(ctx, this) else {
        ctx.unpin_native_roots(mt_pin);
        return unchanged;
    };
    let copy_pin = ctx.pin_native_root(copy);
    let text = ctx.create_string_uninterned(&leaf);
    let copy = ctx.read_native_pin(copy_pin, copy);
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field_by_name(copy, "type", Value::Object(Some(mt)));
    ctx.set_field(copy, MH_DOWNCALL_LEAF_DESC, Value::Object(Some(text)));
    ctx.unpin_native_roots(mt_pin);
    Ok(Some(Value::Object(Some(copy))))
}

/// The `WrongMethodTypeException` `mh.asType(newType)` must raise, or `None`
/// when the conversion is one HotSpot performs.
///
/// Every way of not knowing is an ACCEPT. The check needs four things to hold
/// before it will refuse — the receiver's `type` field must be a real
/// `MethodType`, both descriptors must parse, the raw bytecode descriptor must
/// agree that the conversion is impossible, and both signatures must render —
/// and if any of them fails this returns `None` and the passthrough proceeds
/// exactly as it did before. A missing refusal is the state this VM was already
/// in; a refusal HotSpot does not issue would break working `invokedynamic`
/// call sites, and there is no ordering of those two errors in which the second
/// is the better one.
///
/// The raw-descriptor step is the one that is not obvious: see the block
/// comment on the `asType` registration for why this body's own type mutation
/// makes a second `asType` on the same reference look unconvertible when
/// HotSpot, which hands out a fresh handle each time, would still be looking at
/// the original signature.
///
/// The message is HotSpot's, transcribed from
/// `MethodHandle.asTypeUncached` — `"cannot convert " + this + " to " + newType`
/// where `MethodHandle.toString()` is the literal `MethodHandle` followed
/// immediately by its `MethodType`, e.g.
/// `cannot convert MethodHandle(String)String to (int,int)int`. There is no
/// space after `MethodHandle`, and both signatures use simple type names.
/// Can a VARIABLE-ARITY handle of type `old_desc` be `asType`-adapted to
/// `new_desc` by collecting trailing arguments?
///
/// The JDK's rule (`MethodHandle.asVarargsCollector`'s contract, implemented in
/// `AsVarargsCollector.asTypeUncached`): the trailing array parameter absorbs
/// `newArity - collectArg` arguments, where `collectArg` is the index of that
/// array — so any requested arity from `collectArg` upwards is legal, including
/// `collectArg` itself, which collects zero into an empty array. Element-type
/// compatibility is left to dispatch, exactly as for the non-collecting path:
/// this predicate only decides whether the ARITY difference is a refusal, which
/// is the only thing `method_type_is_convertible_to` was rejecting.
pub(crate) fn varargs_astype_is_legal(old_desc: &str, new_desc: &str) -> bool {
    let (Some((old_params, _)), Some((new_params, _))) = (
        split_descriptor_params(old_desc),
        split_descriptor_params(new_desc),
    ) else {
        return false;
    };
    match old_params.last() {
        Some(last) if last.starts_with('[') => new_params.len() + 1 >= old_params.len(),
        _ => false,
    }
}

/// `CRATONVM_MH_LEAF_DESC` (interpreter round i6 wave 123, lane L4; proposal
/// `i42-L4-proposal-native-adapters-carry-hotspots-type`): is `mh`'s `type`
/// authoritative, so a raw-descriptor escape (an accept because `MH_DESC`
/// converts though `type` does not) must not be taken?
///
/// The escapes exist for the aliasing of an in-place `asType` stamp (the
/// block comment on the `asType` registration). Since `mh_with_stamped_type`
/// COPIES every handle wider than [`MH_VARARGS`] ([`mh_clone_handle`]), such a
/// handle's `type` is its own, and every native combinator mints its adapter's
/// `type` by the JDK's rule from its inputs' `type`s (`MH_DESC` keeping the
/// leaf / dispatch descriptor: the page's `MH_LEAF_DESC`). A narrower handle
/// is stamped in place and keeps the escape. `false` (take the escape, as
/// before) whenever the switch is off. Prints `[MH_LEAF_DESC]` under
/// `CRATONVM_DBG_MH_DISPATCH=1` for each escape it declines: the census of
/// handles whose `type` still differs from what dispatch reads.
pub(crate) fn mh_leaf_desc_type_is_authoritative(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    what: &str,
    type_desc: &str,
    raw_desc: &str,
    new_desc: &str,
) -> bool {
    if !crate::nbflags().mh_leaf_desc || ctx.object_num_fields(mh) <= MH_VARARGS {
        return false;
    }
    if crate::nbflags().dbg_mh_dispatch {
        let kind = match ctx.get_field(mh, MH_KIND) {
            Value::Int(k) => k,
            _ => -1,
        };
        eprintln!(
            "[MH_LEAF_DESC] {what}: kind {kind} type {type_desc} (MH_DESC {raw_desc}) to {new_desc}: refused on type"
        );
    }
    true
}

pub(crate) fn mh_astype_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallFailed> {
    // A real `MethodType` receiver only. `mh_type_descriptor` would fall back
    // to `MH_DESC`, which is the UNADAPTED signature — refusing on it would
    // refuse adapters that are already legal.
    let old_desc = match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt)?,
        _ => return None,
    };
    let new_desc = methodtype_to_descriptor(ctx, new_type)?;
    mh_astype_refusal_desc(ctx, mh, &old_desc, &new_desc)
}

/// [`mh_astype_refusal`] on descriptors already read: `old_desc` is `mh`'s
/// declared `type`. Also the check a plain invoker makes of its target
/// (`Invokers.checkGenericType`'s `asType(t)`, wave 28).
pub(crate) fn mh_astype_refusal_desc(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    old_desc: &str,
    new_desc: &str,
) -> Option<MethodCallFailed> {
    if method_type_is_convertible_to(old_desc, new_desc)? {
        return None;
    }
    // A VARIABLE-ARITY handle adapts across arity, which is the whole point of
    // being one: HotSpot's `AsVarargsCollector.asTypeUncached` reacts to a
    // longer requested type by building `asCollector(arrayType, newArity -
    // collectArg)` and adapting THAT. `method_type_is_convertible_to` compares
    // arities and so refuses every one of those, which is what made
    // `findStatic(C, "m", (String,String[])String).asType((String,String,
    // String)String)` throw where HotSpot collects (`probes/
    // VarargsCollectorProbe.java`). Dispatch already performs the collection —
    // see `collect_trailing_varargs`, whose `ACC_VARARGS` trigger is the same
    // fact the marking records — so accepting here is all that was missing.
    if mh_is_varargs_collector(ctx, mh) && varargs_astype_is_legal(old_desc, new_desc) {
        return None;
    }
    // Round 15 wave 6 (lane mh5, MH4-1): a collector asked for fewer than
    // `collectArg` parameters fails in `AsVarargsCollector.asTypeUncached`'s
    // `asCollector(arrayType, newArity - collectArg)` (a negative length's
    // `IllegalArgumentException`), which it rethrows as this message.
    if mh_is_varargs_collector(ctx, mh)
        && split_descriptor_params(old_desc)
            .is_some_and(|(params, _)| params.last().is_some_and(|p| p.starts_with('[')))
        && mh_astype_fixed_collector_enabled()
    {
        return Some(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/invoke/WrongMethodTypeException",
            "cannot build collector",
        ));
    }
    // Our own aliasing, not a conversion HotSpot refuses.
    if let Some(raw) = mh_read_desc(ctx, mh) {
        // Round 15 wave 4 (lane mh3): a BOUND static handle's `MH_DESC` still
        // names the parameter `bindTo` captured, one more than the caller
        // passes, so `bindTo("-")` of `(String,String[])String` "converted" to
        // `(String,String)String` here; its caller-side raw type drops it.
        // Round 15 wave 6 (lane mh5, MH3-6): every kind whose `MH_DESC` names
        // its capture (`bound_capture_raw_desc`), not only a static one.
        let raw = bound_capture_raw_desc(ctx, mh, &raw).unwrap_or(raw);
        if method_type_is_convertible_to(&raw, new_desc) == Some(true) {
            // `CRATONVM_MH_LEAF_DESC` (interpreter round i6 wave 123, lane
            // L4): a copyable handle's `type` is its own, so the escape is
            // not taken and the refusal is HotSpot's.
            if !mh_leaf_desc_type_is_authoritative(ctx, mh, "asType", old_desc, &raw, new_desc) {
                return None;
            }
        }
    }
    let old_shown = method_type_display_in(&*ctx, old_desc)?;
    let new_shown = method_type_display_in(&*ctx, new_desc)?;
    Some(mh_door_wmte(
        ctx,
        MhWmteRaise::AsType,
        &format!("cannot convert MethodHandle{old_shown} to {new_shown}"),
    ))
}

/// The `WrongMethodTypeException`
/// `MethodHandles.explicitCastArguments(target, newType)` must raise, or `None`.
///
/// Same shape and the same "every way of not knowing is an accept" rule as
/// [`mh_astype_refusal`], and the same reason for the raw-descriptor escape —
/// but the PREDICATE is different, and deliberately so: parameter count and
/// nothing else. See the registration's comment for the measurement.
pub(crate) fn mh_explicit_cast_refusal(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    new_type: ObjectRef,
) -> Option<MethodCallFailed> {
    let old_desc = match ctx.get_field_by_name(target, "type") {
        Value::Object(Some(mt)) => methodtype_to_descriptor(ctx, mt)?,
        _ => return None,
    };
    let new_desc = methodtype_to_descriptor(ctx, new_type)?;
    let (old_params, _) = split_descriptor_params(&old_desc)?;
    let (new_params, _) = split_descriptor_params(&new_desc)?;
    if old_params.len() == new_params.len() {
        return None;
    }
    if let Some(raw) = mh_read_desc(ctx, target) {
        // Round 15 wave 6 (lane mh5, MH3-6): a bound handle's `MH_DESC` that
        // names its capture is one parameter longer than any call to it.
        let raw = bound_capture_raw_desc(ctx, target, &raw).unwrap_or(raw);
        if let Some((raw_params, _)) = split_descriptor_params(&raw) {
            if raw_params.len() == new_params.len()
                && !mh_leaf_desc_type_is_authoritative(
                    ctx,
                    target,
                    "explicitCastArguments",
                    &old_desc,
                    &raw,
                    &new_desc,
                )
            {
                return None;
            }
        }
    }
    let old_shown = method_type_display_in(&*ctx, &old_desc)?;
    let new_shown = method_type_display_in(&*ctx, &new_desc)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("cannot explicitly cast MethodHandle{old_shown} to {new_shown}"),
    ))
}

/// WP2.9 — Robust class-name extraction that survives the descriptor-coercion
/// drift where `Class` mirrors store `Object(name_str)` in slot 1 but read
/// back as `Int(ptr_lo)` / `Long(ptr_bits)` because the field-coercion path
/// only handles Object/Int(0)/Long(0)/Double — not arbitrary integer
/// bit-patterns smuggled through compact storage.
///
/// Strategy:
///   1. Try the reverse map (mirror → ClassId via `class_id_from_mirror`),
///      then look up the name via `class_name_of_id`.
///   2. Fall back to `mirror_class_name` (slot-1 read) for primitive mirrors
///      and other cases the reverse map doesn't cover.
///   3. Last resort: try to reconstruct the slot-1 string pointer from the
///      smuggled bits (Long or Int(non-zero)) and read it as a string.
pub(crate) fn resolve_class_name_robust(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<String> {
    // 1. Reverse-map lookup (preferred — never hits the bad coercion path).
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        if let Some(name) = ctx.class_name_of_id(cid) {
            return Some(name);
        }
    }
    // 2. Standard slot-1 read (works for primitive mirrors and clean paths).
    if let Some(name) = crate::lang_class::mirror_class_name(ctx, mirror) {
        if !name.is_empty() {
            return Some(name);
        }
    }
    // 3. C20 — REMOVED: previously this fell back to reconstructing a
    // String `ObjectRef` from the slot-1 raw bits (Long or Int(non-zero))
    // after only an alignment+48-bit-range check. That was the
    // memory-safety footgun called out in review item #5: arbitrary
    // Java-controlled scalars satisfying the bit-pattern predicate would
    // be fed to `ctx.read_string(...)`, which dereferences the pointer to
    // read the header.
    //
    // `NativeContext` does not expose an `is_heap_addr` probe (the
    // primitive the C7 interpreter fix uses on `&shared.heap`), and we
    // have no reverse-map for arbitrary String objects analogous to
    // `class_id_from_mirror` for Class mirrors. The remaining safe
    // recovery is steps 1-2 above; if both miss, the right answer is
    // `None` rather than fabricating a wild reference.
    None
}

/// WP2.9 — Robust mirror-from-slot recovery: gets a Class mirror from a slot
/// that may have been compacted to Long/Int bits by the descriptor-coercion
/// path. Returns the original ObjectRef when slot value is Object, otherwise
/// reconstructs from heap-aligned 48-bit bit patterns.
///
/// C20 — heap-membership filter on Java-controlled `ObjectRef::from_raw`
/// reconstruction. Mirrors the `vm::interpreter::pop_object_ref_ctx_with`
/// C7 fix: alignment + 48-bit-range alone are not sufficient to prove a
/// Long/Int slot holds a smuggled mirror — an honest scalar value can
/// satisfy them too. Because `NativeContext` does not expose a raw
/// `is_heap_addr` probe, we use `ctx.class_id_from_mirror(candidate)` as
/// the heap-membership oracle: it consults the VM's
/// `class_mirrors_reverse` map (a hashmap lookup keyed on the
/// `ObjectRef`'s raw address — no dereference) and returns `Some(_)`
/// only when the candidate is a registered Class mirror. On miss we
/// return `None`, matching the function's existing "miss" idiom rather
/// than fabricating a wild `ObjectRef` for downstream `read_string` /
/// `heap_kind_of` calls to dereference.
pub(crate) fn recover_class_mirror_from_slot(
    ctx: &dyn NativeContext,
    value: Value,
) -> Option<ObjectRef> {
    match value {
        Value::Object(Some(o)) => Some(o),
        Value::Object(None) => None,
        Value::Long(l) => {
            let bits = l as u64;
            if bits != 0 && (bits & 0x7) == 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                // SAFETY: the alignment+range pre-checks above narrow the
                // candidate to addresses our heap arenas could in principle
                // hand out. `ObjectRef::from_raw` is a typed wrapper that
                // does not dereference; the immediately-following
                // `class_id_from_mirror` lookup is a hashmap probe on the
                // raw address (no header read), so a wild candidate is
                // rejected without ever being touched as a pointer.
                let candidate = unsafe { ObjectRef::from_raw(ptr) };
                if ctx.class_id_from_mirror(candidate).is_some() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Value::Int(i) if i != 0 => {
            // Lower 32 bits of a heap pointer.  Bit-aligned and within 48-bit
            // range checks both pass for heap-allocated ObjectRefs whose top
            // 16 bits are zero (true on 64-bit Windows for our arenas).
            let bits = i as u32 as u64;
            if (bits & 0x7) == 0 && bits != 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                // SAFETY: see Long-arm comment above. Same `class_id_from_mirror`
                // hashmap probe gates the candidate before any heap access.
                let candidate = unsafe { ObjectRef::from_raw(ptr) };
                if ctx.class_id_from_mirror(candidate).is_some() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Reconstruct a full JVM method descriptor from a MethodType object.
/// MethodType has field 0 = returnType (Class mirror), field 1 = paramTypes (Class[] or null).
pub(crate) fn descriptor_from_method_type(ctx: &dyn NativeContext, mt: ObjectRef) -> String {
    let mut desc = String::from("(");

    // Parameter types (field 1 = Class[] array or null)
    if let Value::Object(Some(params_arr)) = ctx.get_field(mt, 1) {
        let len = ctx.array_length(params_arr);
        for i in 0..len {
            if let Some(param_mirror) =
                recover_class_mirror_from_slot(ctx, ctx.get_array_element(params_arr, i))
            {
                desc.push_str(&mirror_to_descriptor(ctx, param_mirror));
            }
        }
    }
    desc.push(')');

    // Return type (field 0 = Class mirror) — recover from any slot encoding.
    if let Some(ret_mirror) = recover_class_mirror_from_slot(ctx, ctx.get_field(mt, 0)) {
        desc.push_str(&mirror_to_descriptor(ctx, ret_mirror));
    } else {
        desc.push('V'); // default void
    }

    desc
}

/// Build a field descriptor for a single Class mirror (used by findGetter/findSetter).
pub(crate) fn field_descriptor_from_mirror(
    ctx: &dyn NativeContext,
    type_mirror: ObjectRef,
) -> Cow<'static, str> {
    mirror_to_descriptor(ctx, type_mirror)
}

// ---------------------------------------------------------------------------
// `MethodType` interning
// ---------------------------------------------------------------------------
//
// `MethodType` is specified to be INTERNED: `MethodType.methodType(...)`,
// `fromMethodDescriptorString(...)` and an `ldc CONSTANT_MethodType` must all
// hand back the *same instance* for the same descriptor, so `==` holds and
// `MethodHandle` call-site matching stays identity-based (JVMS §5.4.3.5,
// `java.lang.invoke.MethodType`'s class javadoc).
//
// The four `methodType` registrations below are `Bridge` natives that SHADOW
// the real JDK bytecode — the `--jdk-only` census reports them as
// `bridge-ran-over-bytecode` — and each one minted a fresh 2-field carrier.
// So on a real image every `methodType()` result was a NEW object:
//
// ```text
//   probes/PMt.java, --jdk-only, one binary          CratonVM   HotSpot 25
//     MethodType.methodType(String,String) == itself  false      true
//     fromMethodDescriptorString == itself            true       true   <- interns
//     fromMethodDescriptorString == methodType(..)    false      true
// ```
//
// which is the `RJdkProxyIface.ldcMethodType` failure: the `ldc` decoder
// (`vm/src/runtime/interpreter/constants.rs`) already does the right thing and
// prefers the real interning factory — the NON-interned side of that `==` was
// the fixture's right-hand `MethodType.methodType(String.class, String.class)`,
// i.e. this file.
//
// Retiring the four shadows (re-tagging them `SyntheticStub` so `--jdk-only`
// refuses them, as `native-api/src/retired_shadow.rs` does for JUL) is the
// structurally right cure and is NOMINATED — it cannot be done here, because
// four rows moving `Bridge` -> `SyntheticStub` also move three frozen
// baselines that can only be re-frozen from a Linux/JDK-25 census
// (`the native-registry census` runs `SLACK = 0`). What this file can do — in BOTH modes,
// with no census motion — is stop fabricating and delegate to the JDK's own
// interning factories.

/// Class name of the interning factory chain's receiver.
pub(crate) const MT_CLASS: &str = "java/lang/invoke/MethodType";
/// `MethodType.genericMethodType(int)` — an interned all-`Object` type. Used as
/// the CHAIN BASE because it takes no strings and no `ClassLoader`: the JDK
/// answers `genericMethodType(0)` out of its own `objectOnlyTypes` cache, so
/// there is no descriptor to parse and no class name to resolve.
pub(crate) const MT_GENERIC: &str = "(I)Ljava/lang/invoke/MethodType;";
/// `MethodType.changeReturnType(Class)`.
pub(crate) const MT_CHANGE_RETURN: &str = "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;";
/// `MethodType.insertParameterTypes(int, Class...)`. Called with index 0 on the
/// empty base rather than `appendParameterTypes`, which would route through the
/// `parameterCount()` shim below for no reason.
pub(crate) const MT_INSERT_PARAMS: &str = "(I[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;";

/// The real JDK's interned `MethodType` for `rtype`/`ptypes`, or `None` when
/// the class library does not carry the factory chain (synthetic-JDK builds),
/// in which case the caller falls back to the 2-field carrier.
///
/// **Why this chain and not `fromMethodDescriptorString`.** That factory is
/// what the `ldc` decoder and `StackFrame.getMethodType()` use, and it interns
/// — but it takes a *descriptor string plus a `ClassLoader`*, and the caller
/// here holds `Class` MIRRORS, whose loaders may differ from each other. Going
/// through a descriptor would re-resolve every parameter by NAME under one
/// guessed loader, which is both slower and wrong the moment two loaders each
/// define a class of the same name. `genericMethodType(0)` ->
/// `insertParameterTypes(0, ptypes)` -> `changeReturnType(rtype)` consumes the
/// mirrors as they are: no strings, no loader, no name resolution, and every
/// step lands in `MethodType.makeImpl(..., true)`, i.e. the intern table.
///
/// Verified end to end on the SHIPPING binary before the change, by running
/// that exact sequence from Java (probes/PMt2.java): under `--jdk-only` the
/// composed type is `==` to `fromMethodDescriptorString`'s for `(String)String`
/// and `()void`, is stable across calls, and answers `returnType`/
/// `parameterType(i)` with the identical mirrors — so what is being delegated
/// to is known to work in this VM, not assumed to.
///
/// None of the three methods is registered anywhere in this tree, so invoking
/// them reaches real bytecode instead of re-entering this file.
pub(crate) fn method_type_interned_via_jdk(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if !ctx.method_exists(MT_CLASS, "genericMethodType", MT_GENERIC)
        || !ctx.method_exists(MT_CLASS, "changeReturnType", MT_CHANGE_RETURN)
        || !ctx.method_exists(MT_CLASS, "insertParameterTypes", MT_INSERT_PARAMS)
    {
        return Ok(None);
    }
    // GC-safety: every `invoke*` below can collect and relocate the caller's
    // mirrors, which are bare `ObjectRef`s in Rust locals. Pin them for the
    // whole chain and re-read through the handles after each call.
    let rtype_pin = ctx.pin_native_root(rtype);
    let ptypes_pin = ptypes.map(|p| ctx.pin_native_root(p));
    let out = method_type_interned_chain(ctx, rtype, rtype_pin, ptypes, ptypes_pin);
    ctx.unpin_native_roots(rtype_pin);
    out
}

/// The pinned body of [`method_type_interned_via_jdk`].
///
/// Failure policy, following the same split
/// `vm/src/runtime/interpreter/constants.rs` established for the `ldc` path: a
/// Java-visible exception from `insertParameterTypes` / `changeReturnType` IS
/// the answer (`void` as a parameter type, a null element, too many argument
/// slots — HotSpot throws `IllegalArgumentException`/`NullPointerException`
/// there too), so it propagates rather than being replaced by a fabricated
/// type. Anything else — a missing return value, an internal error, or a
/// failure of the BASE call, which validates nothing and cannot be the user's
/// answer — falls back to the carrier this file has always built.
pub(crate) fn method_type_interned_chain(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    rtype_pin: usize,
    ptypes: Option<ObjectRef>,
    ptypes_pin: Option<usize>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mut mt = match ctx.invoke(MT_CLASS, "genericMethodType", MT_GENERIC, &[Value::Int(0)]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(None),
    };
    let mut mt_pin = ctx.pin_native_root(mt);
    if let (Some(p), Some(p_pin)) = (ptypes, ptypes_pin) {
        let arr = ctx.read_native_pin(p_pin, p);
        if ctx.array_length(arr) > 0 {
            let recv = ctx.read_native_pin(mt_pin, mt);
            match ctx.invoke_virtual(
                recv,
                "insertParameterTypes",
                MT_INSERT_PARAMS,
                &[Value::Int(0), Value::Object(Some(arr))],
            ) {
                Ok(Some(Value::Object(Some(o)))) => {
                    mt = o;
                    mt_pin = ctx.pin_native_root(mt);
                }
                Err(e @ MethodCallFailed::ExceptionThrown(_)) => return Err(e),
                _ => return Ok(None),
            }
        }
    }
    let recv = ctx.read_native_pin(mt_pin, mt);
    let rt = ctx.read_native_pin(rtype_pin, rtype);
    match ctx.invoke_virtual(
        recv,
        "changeReturnType",
        MT_CHANGE_RETURN,
        &[Value::Object(Some(rt))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => Ok(Some(o)),
        Err(e @ MethodCallFailed::ExceptionThrown(_)) => Err(e),
        _ => Ok(None),
    }
}

/// One `MethodType` for the four `methodType(...)` overloads: the real JDK's
/// interned instance when the image has the factory chain, otherwise the
/// 2-field `(returnType, parameterArray)` carrier this file has always minted,
/// with its fabricated `MethodTypeForm`.
///
/// `ptypes` is consumed as given; `None` (an absent or null parameter array)
/// becomes a 0-length one on the fallback path, so `parameterCount()` and
/// `parameterArray()` never meet a null slot.
/// A `Class[]` of `len` (i6-L2): the declared type of `MethodType.ptypes`
/// and `parameterArray()`, never an untyped `Object[]`. Same GC contract as
/// `new_array`: pin what is held across it.
pub(crate) fn new_class_array(ctx: &mut dyn NativeContext, len: usize) -> ObjectRef {
    let component = crate::lang_class::reflection_component_id(ctx, "java/lang/Class");
    ctx.new_ref_array(component, len)
}

pub(crate) fn make_method_type(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    if let Some(interned) = method_type_interned_via_jdk(ctx, rtype, ptypes)? {
        return Ok(Some(Value::Object(Some(interned))));
    }
    make_method_type_carrier(ctx, rtype, ptypes)
}

/// [`make_method_type`] for an UNTRUSTED `ptypes` array -- the caller's own
/// `Class[]` handed to `methodType(Class, Class[])` (interpreter round i3 wave
/// 79, lane L4; page
/// `i76-L4-synthetic-method-type-keeps-the-callers-parameter-array`): the
/// fallback carrier keeps a COPY, as HotSpot's `makeImpl(rtype, ptypes, false)`
/// clones an untrusted array. The carrier stored the caller's array itself, so
/// `a[0] = Integer.class` after `methodType(int.class, a)` changed the type.
/// Over a real JDK the factory chain answers first and copies on its own
/// (`insertParameterTypes`); only the fallback pays the copy.
pub(crate) fn make_method_type_copying(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    if let Some(interned) = method_type_interned_via_jdk(ctx, rtype, ptypes)? {
        return Ok(Some(Value::Object(Some(interned))));
    }
    let Some(params) = ptypes else {
        return make_method_type_carrier(ctx, rtype, None);
    };
    let n = ctx.array_length(params);
    // GC-safety: the copy's allocation can relocate both the mirror and the
    // caller's array.
    let rtype_pin = ctx.pin_native_root(rtype);
    let params_pin = ctx.pin_native_root(params);
    let copy = new_class_array(ctx, n);
    let copy_pin = ctx.pin_native_root(copy);
    let params = ctx.read_native_pin(params_pin, params);
    let copy = ctx.read_native_pin(copy_pin, copy);
    if !ctx.bulk_array_copy(params, 0, copy, 0, n) {
        for i in 0..n {
            let v = ctx.get_array_element(params, i);
            ctx.set_array_element(copy, i, v);
        }
    }
    let copy = ctx.read_native_pin(copy_pin, copy);
    let rtype = ctx.read_native_pin(rtype_pin, rtype);
    ctx.unpin_native_roots(rtype_pin);
    // Nothing collects between the unpin and the carrier's own pins.
    make_method_type_carrier(ctx, rtype, Some(copy))
}

/// The synthetic 2-field carrier [`make_method_type`] mints when the JDK's
/// factory chain does not answer (a synthetic class library), over `ptypes`
/// as given.
pub(crate) fn make_method_type_carrier(
    ctx: &mut dyn NativeContext,
    rtype: ObjectRef,
    ptypes: Option<ObjectRef>,
) -> MethodCallResult {
    // GC-safety: the allocation and the empty-array creation below can each
    // relocate the caller's mirrors and the carrier itself.
    let rtype_pin = ctx.pin_native_root(rtype);
    let ptypes_pin = ptypes.map(|p| ctx.pin_native_root(p));
    let obj = try_alloc_concurrent_synthetic(ctx, MT_CLASS, 6)?;
    let obj_pin = ctx.pin_native_root(obj);
    let rt = ctx.read_native_pin(rtype_pin, rtype);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(rt)));
    let params = match (ptypes, ptypes_pin) {
        (Some(p), Some(pin)) => ctx.read_native_pin(pin, p),
        _ => new_class_array(ctx, 0),
    };
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 1, Value::Object(Some(params)));
    let mut obj = obj;
    populate_method_type_form(ctx, &mut obj)?;
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(rtype_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 76, lane L4: `--compatible`'s `MethodType.methodType`
// factories yield to the JDK's bytecode (`CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`)
// ---------------------------------------------------------------------------
//
// Over a real JDK the four `methodType` `Bridge`s below build nothing of their
// own: `make_method_type` first asks `method_type_interned_via_jdk`, which runs
// `genericMethodType(0)` -> `insertParameterTypes(0, ptypes)` ->
// `changeReturnType(rtype)` from Rust and answers the JDK's interned instance.
// Per call that is a native crossing, three by-name `method_exists` probes, up
// to three native-to-Java re-entries and two intern probes, where the JDK's own
// body is ONE `makeImpl(rtype, ptypes, true)`. And a `null` component throws
// from the native (`obj_arg`) without the JDK's `makeImpl` frame (page
// `docs/feature-designs/native-dispatch-architecture.md`).
// Under `--jdk-only` the bridges already lose to the bytecode (they are rows
// of `RETIRED_SHADOW_SBR_MHLOOKUP_TRIPLES`), so behind the switch both modes
// run the same Java and answer the same objects. A synthetic JDK has no
// factory chain and keeps the bridges (and their 2-field carrier). The four
// are registered as `Bridge`s and the registry re-tags them `SyntheticStub`
// (retired shadows), so the switch only REMOVES stubs: the frozen baselines of
// `the native-registry census` are measured with it off (default)
// and are unchanged.
//
// Every Rust caller that needs a `MethodType` already goes through
// `make_method_type` / `method_type_interned_via_jdk` or
// `fromMethodDescriptorString` directly, never through these registrations.

/// `CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE` -- default OFF, `truthy_word`,
/// read at registration.
pub(crate) fn mh_compat_method_type_bytecode_enabled() -> bool {
    crate::nbflags().mh_compat_method_type_bytecode
}

/// Does `r`'s VM leave the four `methodType` factories to the JDK's bytecode?
/// The switch, over a real JDK.
pub(crate) fn mh_compat_method_type_bytecode_applies(r: &NativeMethodRegistry) -> bool {
    mh_compat_method_type_bytecode_enabled() && r.real_jdk()
}

/// The four `MethodType.methodType` overloads as `Bridge`s over
/// [`make_method_type`]. Sets its own category and restores the caller's.
pub(crate) fn register_method_type_factory_bridges(r: &mut NativeMethodRegistry, mt: &str) {
    let prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let params = match args.get(1) {
                Some(Value::Object(p)) => *p,
                _ => None,
            };
            // The caller's array: copied by the fallback carrier (wave 79).
            make_method_type_copying(ctx, ret, params)
        },
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            make_method_type(ctx, ret, None)
        },
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let param = obj_arg(args, 1)?;
            // GC-safety: `new_array` can collect and relocate both mirrors.
            let ret_pin = ctx.pin_native_root(ret);
            let param_pin = ctx.pin_native_root(param);
            let arr = new_class_array(ctx, 1);
            let param = ctx.read_native_pin(param_pin, param);
            ctx.set_array_element(arr, 0, Value::Object(Some(param)));
            let ret = ctx.read_native_pin(ret_pin, ret);
            let out = make_method_type(ctx, ret, Some(arr));
            ctx.unpin_native_roots(ret_pin);
            out
        },
    );
    // methodType(Class rtype, Class ptype0, Class... morePtypes)
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let ptype0 = obj_arg(args, 1)?;
            // `morePtypes` may be EMPTY; it may not be NULL. HotSpot's
            // `methodType(rtype, ptype0, ptypes)` runs the whole array through
            // `checkPtypes`, which dereferences it -- a null there is an NPE,
            // not "no more parameters". The comment this replaces said "may be
            // null or an array" and the code obliged, so
            // `methodType(int.class, String.class, (Class<?>[]) null)` built
            // `(String)int` where HotSpot throws.
            let more = match args.get(2) {
                Some(Value::Object(Some(a))) => Some(*a),
                Some(Value::Object(None)) => {
                    return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(
                            cratonvm_types::error::RuntimeError::NullPointerException {
                                message: Some(
                                    "MethodType.methodType: ptypes must not be null".to_string(),
                                ),
                            },
                        ),
                    ));
                }
                _ => None,
            };
            let more_len = more.map(|a| ctx.array_length(a)).unwrap_or(0);
            // GC-safety: `new_array` and the element copies can each collect;
            // pin every reference the loop below still needs afterwards.
            let ret_pin = ctx.pin_native_root(ret);
            let ptype0_pin = ctx.pin_native_root(ptype0);
            let more_pin = more.map(|a| ctx.pin_native_root(a));
            let total = 1 + more_len;
            let arr = new_class_array(ctx, total);
            let arr_pin = ctx.pin_native_root(arr);
            let ptype0 = ctx.read_native_pin(ptype0_pin, ptype0);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, 0, Value::Object(Some(ptype0)));
            if let (Some(m), Some(m_pin)) = (more, more_pin) {
                for i in 0..more_len {
                    let m = ctx.read_native_pin(m_pin, m);
                    let elem = ctx.get_array_element(m, i);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, 1 + i, elem);
                }
            }
            let ret = ctx.read_native_pin(ret_pin, ret);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let out = make_method_type(ctx, ret, Some(arr));
            ctx.unpin_native_roots(ret_pin);
            out
        },
    );
    r.set_category(prev);
}

// ---------------------------------------------------------------------------
// java.lang.invoke — MethodHandle, MethodType, MethodHandles (stubs)
// ---------------------------------------------------------------------------
pub fn register_phase54_method_handle(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- MethodType (2-field: returnType=0, paramTypes=1) ---
    let mt = "java/lang/invoke/MethodType";
    // Interpreter round i3 wave 76, lane L4
    // (`CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`, default off): over a real JDK
    // the four `methodType` factories are left to the JDK's bytecode.
    if !mh_compat_method_type_bytecode_applies(r) {
        register_method_type_factory_bridges(r, mt);
    }
    r.register(mt, "returnType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(mt, "parameterCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    // `parameterType(i)` is bounds-checked. HotSpot indexes its own `ptypes`
    // array and so raises `ArrayIndexOutOfBoundsException`; this used to cast a
    // NEGATIVE index straight to `usize` -- `-1` becoming 18 446 744 073 709
    // 551 615 -- and hand it to `get_array_element`, which answered a null the
    // caller then dereferenced, so the exception a caller saw was an NPE from
    // somewhere else entirely.
    r.register(mt, "parameterType", "(I)Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            let len = ctx.array_length(arr) as i64;
            if i64::from(raw) < 0 || i64::from(raw) >= len {
                return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                            index: raw,
                            message: Some(format!("Index {raw} out of bounds for length {len}")),
                        },
                    ),
                ));
            }
            Ok(Some(ctx.get_array_element(arr, raw as usize)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });
    // `MethodType.parameterArray()` returns a COPY. The JDK's whole body is
    // `return ptypes.clone();`, and the clone is the contract: `MethodType` is
    // specified immutable AND interned, so handing back the live `ptypes` lets
    // any caller rewrite a type that other callers already hold.
    //
    // MEASURED before this, against jdk-25.0.3.9-hotspot:
    //
    //   MethodType mt = methodType(int.class, String.class, long.class);
    //   mt.parameterArray()[0] = int.class;
    //   mt                        HotSpot (String,long)int   CratonVM (int,long)int
    //   mt.parameterType(0)               java.lang.String              int
    //
    // One write, and the type is a different type for good. It took six other
    // rows down with it in `probes/L8InvokeLookupSweep.java` -- `toString`,
    // `descriptorString`, `changeReturnType`, `appendParameterTypes`, `wrap`
    // and `unwrap` all read the corrupted array afterwards and were NOT six
    // more defects, which is why the probe asks `isCopy` BEFORE it asks any of
    // them.
    r.register(mt, "parameterArray", "()[Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Value::Object(Some(params)) = ctx.get_field(this, 1) else {
            let empty = new_class_array(ctx, 0);
            return Ok(Some(Value::Object(Some(empty))));
        };
        let n = ctx.array_length(params);
        // GC-safety: `new_array` can collect and relocate `params`, so pin it
        // across the allocation and read it back before the copy.
        let params_pin = ctx.pin_native_root(params);
        let out = new_class_array(ctx, n);
        let out_pin = ctx.pin_native_root(out);
        let params = ctx.read_native_pin(params_pin, params);
        let out = ctx.read_native_pin(out_pin, out);
        if !ctx.bulk_array_copy(params, 0, out, 0, n) {
            for i in 0..n {
                let v = ctx.get_array_element(params, i);
                ctx.set_array_element(out, i, v);
            }
        }
        let out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(params_pin);
        Ok(Some(Value::Object(Some(out))))
    });
    // `Class.getSimpleName()`'s spelling of a binary class name.
    //
    // The last-segment-after-`.`-or-`$` rule this used to apply is right for a
    // plain class and wrong for every ARRAY class: an array's binary name is
    // `[Ljava/lang/String;` or `[I`, whose last segment is `String;` and `[I`,
    // where the JDK prints `String[]` and `int[]`. `MethodType.toString()` is
    // the message text of every `WrongMethodTypeException` and
    // `MethodHandle` linkage error, so the difference read as
    //
    // ```text
    //   HotSpot   (String,String[])String
    //   CratonVM  (String,String;)String
    // ```
    //
    // in a diagnostic whose entire job is to be compared, character for
    // character, against the type the caller asked for
    // (`probes/MhCombinatorProbe.java` / `probes/VarargsCollectorProbe.java`).
    fn class_display_simple_name(raw: &str) -> String {
        let name = raw.hidden_dotted();
        let dims = name.bytes().take_while(|b| *b == b'[').count();
        let base = &name[dims..];
        let suffix = "[]".repeat(dims);
        // A hidden class (`/0x<hex>` tail) is top-level to `getSimpleName()`:
        // `getName()` cut at its last `.` only (`Foo$$Lambda/0x12`), never at
        // a `$` (wave 15 review, rev-w14-native item 5).
        let simple_tail = |n: &str| -> String {
            if crate::lang_class::hidden_name_suffix_split(n).is_some() {
                n.rsplit('.').next().unwrap_or(n).to_string()
            } else {
                n.rsplit(['.', '$']).next().unwrap_or(n).to_string()
            }
        };
        if dims == 0 {
            return simple_tail(base);
        }
        match base.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            Some(inner) => {
                let tail = simple_tail(inner);
                format!("{tail}{suffix}")
            }
            // A primitive element type is a one-letter descriptor, not a name.
            None => {
                let elem = match base {
                    "B" => "byte",
                    "C" => "char",
                    "D" => "double",
                    "F" => "float",
                    "I" => "int",
                    "J" => "long",
                    "S" => "short",
                    "Z" => "boolean",
                    other => other,
                };
                format!("{elem}{suffix}")
            }
        }
    }

    // `MethodType.toString()` is specified as `(P1,P2,…)R` using each type's
    // SIMPLE name — `(Bean)int`, not `MethodType(1 params)`, which is what this
    // returned until 2026-08-04 and which no JDK ever prints.
    //
    // It matters beyond cosmetics: this string is what `WrongMethodTypeException`
    // and every `MethodHandle` linkage error carry as their message, so a
    // placeholder here turns a diagnosable "expected (Bean)int, found (int)int"
    // into two identical strings.
    //
    // The receiver may have either layout, so read the two fields by shape.
    // Synthetic `MethodType` is the 2-field `(returnType, parameterArray)` this
    // file mints; the real JDK class names them `rtype` / `ptypes` and has
    // several more fields behind them. Falling back to the placeholder when
    // neither read works keeps this total — a `toString` that raises would be a
    // worse failure than a vague one.
    r.register(mt, "toString", "()Ljava/lang/String;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let synthetic = ctx.object_num_fields(this) <= 2;
        let read_types = |ctx: &dyn cratonvm_native_api::NativeContext, this: ObjectRef| {
            if synthetic {
                (ctx.get_field(this, 0), ctx.get_field(this, 1))
            } else {
                (
                    ctx.get_field_by_name(this, "rtype"),
                    ctx.get_field_by_name(this, "ptypes"),
                )
            }
        };
        let (mut rtype, mut ptypes) = read_types(&*ctx, this);

        // JIT round 17 wave 8, lane mh7 (third pass): the JDK body asks
        // `getSimpleName()` of the return type, then each parameter type, so a
        // nested class whose outer class disagrees on `InnerClasses` throws
        // `IncompatibleClassChangeError` here (`R17Mh7Inner
        // methodtype-toString`, `--compatible`). `CRATONVM_CLASS_INNER_CLASSES_CHECK`.
        if crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(&*ctx)) {
            let mut ids: Vec<ClassId> = Vec::new();
            if let Value::Object(Some(r)) = rtype {
                ids.extend(mirror_class_id(&*ctx, r));
            }
            if let Value::Object(Some(arr)) = ptypes {
                for i in 0..ctx.array_length(arr) {
                    if let Value::Object(Some(m)) = ctx.get_array_element(arr, i) {
                        ids.extend(mirror_class_id(&*ctx, m));
                    }
                }
            }
            // The check can load a class (and so collect): keep `this` and
            // read the two fields again afterwards.
            let this_pin = ctx.pin_native_root(this);
            let icce = mh_wmte_message_icce(ctx, &ids);
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            if let Some(icce) = icce {
                return Err(icce);
            }
            (rtype, ptypes) = read_types(&*ctx, this);
        }

        // `int` / `java.lang.String` -> `int` / `String`, which is what
        // `Class.getSimpleName()` yields and what the JDK's own `toString`
        // uses. Nested classes render after the last `$`, matching it.
        fn simple_name(
            ctx: &dyn cratonvm_native_api::NativeContext,
            mirror: Value,
        ) -> Result<String, MethodCallFailed> {
            let Value::Object(Some(m)) = mirror else {
                return Ok("?".to_string());
            };
            match resolve_class_name_robust(ctx, m) {
                Some(name) => {
                    // `Class.getSimpleName()` of the loaded class (wave 39):
                    // a local `Outer$1Local` is `Local`, an anonymous class
                    // `""`, a top-level `A$B` keeps its `$`.
                    let internal = name.replace('.', "/");
                    let dims = internal.bytes().take_while(|b| *b == b'[').count();
                    let loaded = if dims == 0 {
                        ctx.class_id_from_mirror(m)
                            .and_then(|id| simple_name_of_class(ctx, id, &internal))
                    } else {
                        internal[dims..]
                            .strip_prefix('L')
                            .and_then(|s| s.strip_suffix(';'))
                            .and_then(|elem| loaded_simple_name(ctx, elem))
                            .map(|s| s + &"[]".repeat(dims))
                    };
                    Ok(loaded.unwrap_or_else(|| class_display_simple_name(&name)))
                }
                // A lambda proxy's mirror names no class: its `getSimpleName()`
                // is its `getName()` past the package, `/0x..` suffix and all
                // (interpreter round i2 wave 61, lane L4).
                None => Ok(match lk_lambda_proxy_of_mirror(ctx, m) {
                    Some((_, name)) => name.rsplit('.').next().unwrap_or(&name).to_string(),
                    None => "?".to_string(),
                }),
            }
        }

        let mut out = String::from("(");
        // Tracked as a flag, not inferred from the rendered string: `()void` is
        // a perfectly good `MethodType`, so an empty parameter list must not be
        // read as "the field could not be read".
        let mut ptypes_ok = false;
        let mut param_count = 0usize;
        if let Value::Object(Some(arr)) = ptypes {
            ptypes_ok = true;
            param_count = ctx.array_length(arr);
            for i in 0..param_count {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&simple_name(ctx, ctx.get_array_element(arr, i))?);
            }
        }
        out.push(')');
        let ret = simple_name(ctx, rtype)?;
        let s = if !ptypes_ok || ret == "?" {
            // A field could not be read — do not invent a signature.
            ctx.create_string(&format!("MethodType({param_count} params)"))
        } else {
            out.push_str(&ret);
            ctx.create_string(&out)
        };
        Ok(Some(Value::Object(Some(s))))
    });

    // --- MethodHandle (1-field: name=0 for debugging) ---
    let mh = "java/lang/invoke/MethodHandle";
    // `type()` is NOT registered here. It was, with a body that returned
    // `ctx.get_field(this, 0)` raw, and it never ran once:
    // `register_t4_method_handle_invoke` runs after this registrar on every
    // boot arm (`vm/src/vm/vm_init.rs` :2594 and :3298, and
    // `native-builtins/src/lib.rs`'s synthetic chain) and registers the same
    // triple with the C19 body that prefers the real-JDK `type` field and
    // falls back to the synthetic descriptor.
    //
    // MEASURED, `--dump-native-registry --explain-jdk-only --jdk-only`:
    // `java/lang/invoke/MethodHandle.type()Ljava/lang/invoke/MethodType;`
    // appears twice, `owns_slot=false` here and `owns_slot=true` there.
    // Deleting the loser is inert; leaving it in place is a landmine, because a
    // lane retiring the WINNER on a census row would promote this raw slot-0
    // read in its place.
    // `MethodHandle.toString()` is `standardString()`: `"MethodHandle" +
    // type()` (`MethodHandle(int)int`, measured on JDK 25), which this
    // answered as the bare word `MethodHandle` for every handle until wave 47
    // (lane L4, probe `L4W47MethodHandleToString`). A handle whose `type`
    // cannot be read back keeps the bare word.
    r.register(mh, "toString", "()Ljava/lang/String;", |ctx, args| {
        let text = match args.first() {
            Some(Value::Object(Some(this))) => mh_declared_descriptor(ctx, *this)
                .and_then(|desc| method_type_display_in(&*ctx, &desc))
                .map(|shown| format!("MethodHandle{shown}")),
            _ => None,
        }
        .unwrap_or_else(|| "MethodHandle".to_string());
        let s = ctx.create_string_uninterned(&text);
        Ok(Some(Value::Object(Some(s))))
    });

    // --- MemberName accessor overrides ---
    // The JDK bytecode for `MemberName.getMethodType()` reads its `type:Object`
    // field and, when null, invokes `expandFromVM` (a JVM-internal native) to
    // populate it. We don't implement expandFromVM, so when downstream code
    // (revealDirect, LambdaMetafactory) reads getMethodType the JDK falls
    // through to the null-return branch even after we populate the slot —
    // the JDK Object-field read may be intercepted by an injected vtable that
    // re-reads from a hidden vmtarget slot. Bypassing the Java logic entirely
    // with a native that returns slot 2 directly avoids the issue and matches
    // what `MemberName.type` would yield once `expandFromVM` had populated it.
    let mn_cls = "java/lang/invoke/MemberName";
    r.register(
        mn_cls,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(mn_type_slot_converted(ctx, this)))
        },
    );
    // `getMethodOrFieldType` is what `InfoFromMemberName.getMethodType()` and
    // several other JDK paths actually call. The JDK's own body answers the
    // method type for a method or constructor (slot 2 already holds it) and,
    // for a FIELD, the ACCESSOR's type: `methodType(fieldType)` for a getter,
    // `methodType(void.class, fieldType)` for a setter. This native returned
    // slot 2 raw for a field too -- a `Class` from a method declared to return
    // a `MethodType` -- so `revealDirect(findGetter(C, "i", int.class))
    // .getMethodType()` printed `class java.lang.Integer` where HotSpot prints
    // `()int` (wave 25, `tools/probes/interp/L4/L4W25RevealDirect.java`). A
    // field whose type cannot be spelled exactly keeps the old answer.
    r.register(
        mn_cls,
        "getMethodOrFieldType",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ty = mn_type_slot_converted(ctx, this);
            if let Value::Object(Some(field_type)) = ty {
                let ref_kind = match ctx.get_field(this, 3) {
                    Value::Int(flags) => (flags >> 24) & 0x0F,
                    _ => 0,
                };
                let is_class = ctx
                    .class_name_of_id(ctx.class_id_of_object(field_type))
                    .as_deref()
                    == Some("java/lang/Class");
                if (1..=4).contains(&ref_kind) && is_class {
                    if let Some(d) = mhn_strict_type_descriptor(&*ctx, field_type) {
                        // 1, 2: getField / getStatic; 3, 4: putField / putStatic.
                        let accessor = if ref_kind <= 2 {
                            format!("(){d}")
                        } else {
                            format!("({d})V")
                        };
                        if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &accessor) {
                            return Ok(Some(Value::Object(Some(mt))));
                        }
                    }
                }
            }
            Ok(Some(ty))
        },
    );
    r.register(
        mn_cls,
        "getFieldType",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(mn_cls, "getModifiers", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Modifiers = low 16 bits of flags.
        let flags = match ctx.get_field(this, 3) {
            Value::Int(f) => f,
            _ => 0,
        };
        Ok(Some(Value::Int(flags & 0xFFFF)))
    });
    r.register(mn_cls, "getReferenceKind", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // flags is at declared-slot 3; refKind is the top byte
        let flags = match ctx.get_field(this, 3) {
            Value::Int(f) => f,
            _ => 0,
        };
        Ok(Some(Value::Int((flags >> 24) & 0x0F)))
    });
    r.register(
        mn_cls,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(mn_cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // --- MethodHandles (static utility) ---
    //
    // `lookup()` and `privateLookupIn(Class, Lookup)` are NOT registered here,
    // and the reason is the header comment thirty lines below this one, which
    // already stated the rule: *"we do not provide stub overrides here — the
    // real ones take precedence and a stub would only run if the late phase is
    // not also invoked"*. These two were violations of it, and they were the
    // dangerous kind.
    //
    // Both stubs allocated a bare `MethodHandles$Lookup` and returned it with
    // slot 0 — the lookup class — NEVER WRITTEN. That is verbatim defect #1 in
    // `native-builtins/tests/duplicate_registration_gate.rs`'s header: *"a
    // placeholder registered late shadowed the real implementation; the
    // returned Lookup's slot 0 was never written, so lookupClass() answered
    // null. Broke RJdkHidden AND RJdkStrict."*
    //
    // MEASURED, `--dump-native-registry --explain-jdk-only --jdk-only`: both
    // triples appear twice, `owns_slot=false` here and `owns_slot=true` in
    // `register_p63_method_handles_lookup`, which runs immediately after this
    // registrar on every boot arm — `vm/src/vm/vm_init.rs` :2588 and :3295,
    // and `phases_late.rs::register_phase63_natives` for the synthetic chain.
    // So the deletion is inert TODAY and removes the promotion hazard for
    // tomorrow.
    let mhs = "java/lang/invoke/MethodHandles";
    let _ = mhs;

    // NOT REGISTERED HERE: `MethodHandles.arrayElementVarHandle`.
    //
    // It is registered in `phases_late/reflect_invoke.rs`'s
    // `register_p59_varhandle`, which reaches the registry only through
    // `register_synthetic_overrides` — synthetic-JDK mode only. The obvious
    // move is to mirror it onto this (live in every mode) path so `--jdk-only`
    // gets a side-table entry for array handles. Measurement says do NOT:
    //
    //  * The real JDK bytecode for this factory ALREADY runs correctly in
    //    real-JDK mode and hands back a genuine `VarHandleInts$Array`.
    //    `vm/tests/clinit_first_call_compile_order.rs` is the witness — its
    //    probe does `arrayElementVarHandle(int[].class)`, `vh.set(arr,3,42)`,
    //    `vh.get(arr,3)` under `--java-home <real jdk>` and asserts `42`.
    //  * The accessors do not need the side table for the array case: every
    //    one of them routes through [`vh_array_call`], which recognises the
    //    access from the ARGUMENT SHAPE (`[vh, array, int]`) and never reads a
    //    synthetic slot off the receiver.
    //  * That same probe exists because `arrayElementVarHandle` is the cheapest
    //    trigger for the `VarForm` -> `MethodType` -> `sun/invoke/util/Wrapper`
    //    -> `ConstantDescs.<clinit>` chain. A native here short-circuits the
    //    chain and the regression test goes green while covering nothing.
    //
    // So the real object stays, and the two things it was missing are supplied
    // without replacing it: the bounds check lives on the `vh_array_call`
    // funnel, and `varType()`/`coordinateTypes()` read the real handle's own
    // identity in [`real_array_var_handle_descriptors`].

    // byteArrayViewVarHandle(<T>[].class, ByteOrder) — view a byte[] as a wider
    // primitive at a BYTE index. The real JDK VarHandle's get/set route through
    // our `varhandle_get`/`_set`, but those would mis-detect it as an
    // array-ELEMENT access (same [vh, array, index] arg shape) and read/write a
    // single byte — which silently zeroed SHA3/SHAKE XOF squeeze and thus
    // ML-DSA/ML-KEM keygen. Return a synthetic VarHandle tagged with byte-view
    // meta (element type from the array Class, endianness from the ByteOrder) so
    // get/set do the correct multi-byte LE/BE conversion. Also fixes any other
    // byteArrayViewVarHandle user (NIO-style serialization, crypto, …).
    r.register(
        mhs,
        "byteArrayViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let elem = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    let cid = ctx.class_id_from_mirror(*mirror);
                    let name = cid.and_then(|c| ctx.class_name_of_id(c));
                    // array class name is "[J" / "[I" / … → element descriptor is byte 1
                    name.and_then(|n| n.as_bytes().get(1).copied())
                        .unwrap_or(b'J')
                }
                _ => b'J',
            };
            let le = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field_by_name(*bo, "name") {
                    Value::Object(Some(s)) => ctx
                        .read_string(s)
                        .map(|n| n.contains("LITTLE"))
                        .unwrap_or(true),
                    _ => true,
                },
                _ => true,
            };
            let vh =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            vh_meta_put(
                ctx,
                vh,
                VarHandleMeta {
                    kind: if le {
                        VH_KIND_BYTE_VIEW_LE
                    } else {
                        VH_KIND_BYTE_VIEW_BE
                    },
                    class_name: String::new(),
                    field_name: String::new(),
                    field_desc: (elem as char).to_string(),
                    field_index: -1,
                    class_id: 0,
                    read_only: false,
                    exact: None,
                    ref_type_class_id: 0,
                    holder_class_id: 0,
                },
            );
            Ok(Some(Value::Object(Some(vh))))
        },
    );

    // byteBufferViewVarHandle(<T>[].class, ByteOrder) -- view a ByteBuffer as
    // wider primitives at a BYTE index. Netty 4.2 uses these VarHandles for
    // direct ByteBuf multi-byte headers when USE_VAR_HANDLE is true.
    r.register(
        mhs,
        "byteBufferViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let elem = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    let cid = ctx.class_id_from_mirror(*mirror);
                    let name = cid.and_then(|c| ctx.class_name_of_id(c));
                    name.and_then(|n| n.as_bytes().get(1).copied())
                        .unwrap_or(b'J')
                }
                _ => b'J',
            };
            let le = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field_by_name(*bo, "name") {
                    Value::Object(Some(s)) => ctx
                        .read_string(s)
                        .map(|n| n.contains("LITTLE"))
                        .unwrap_or(true),
                    _ => true,
                },
                _ => true,
            };
            let vh =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            vh_meta_put(
                ctx,
                vh,
                VarHandleMeta {
                    kind: if le {
                        VH_KIND_BYTE_BUFFER_VIEW_LE
                    } else {
                        VH_KIND_BYTE_BUFFER_VIEW_BE
                    },
                    class_name: String::new(),
                    field_name: String::new(),
                    field_desc: (elem as char).to_string(),
                    field_index: -1,
                    class_id: 0,
                    read_only: false,
                    exact: None,
                    ref_type_class_id: 0,
                    holder_class_id: 0,
                },
            );
            Ok(Some(Value::Object(Some(vh))))
        },
    );

    // --- MethodHandles.Lookup ---
    //
    // Note: findVirtual/findStatic/findGetter/findSetter/findConstructor and
    // friends are registered by the real implementations in
    // `register_p63_method_handles_lookup` (late phase). We do not provide
    // stub overrides here — the real ones take precedence and a stub would
    // only run if the late phase is not also invoked (which would indicate
    // a broken VM startup).
    //
    // `MethodHandles$Lookup.lookupClass()` is NOT registered here either — the
    // third of the same set, `owns_slot=false` in the dump, overwritten by
    // `register_p63_method_handles_lookup`. It is the READ side of the null
    // slot 0 the two deleted stubs above produced, so all three went together.

    // Lookup.defineHiddenClass: NOT registered here, deliberately.
    //
    // This file's registrar runs LATE (`register_phase54_method_handle`,
    // vm_init.rs:2648) and the triple table is last-write-wins, so the
    // placeholder that used to live here SHADOWED the real WP2.3-B
    // implementation in `lookup_define.rs:684` (wired on the essential path
    // at vm_init.rs:2120). The placeholder ignored its arguments and returned
    // a `MethodHandles$Lookup` whose slot 0 (`lookupClass`) was never written,
    // so `lookupClass()` answered null and every caller NPE'd on the result —
    // regression-suite RJdkHidden.defineNestmate:91 and RJdkStrict
    // .generatedClassesStillAllowed:247, in BOTH --real-jdk and --jdk-only.
    //
    // Note the header comment above this block already stated the intended
    // rule ("we do not provide stub overrides here — the real ones take
    // precedence"); this registration was the one violation of it.

    // --- VarHandle real ops ---
    let vh = "java/lang/invoke/VarHandle";

    // VarHandle.withInvokeExactBehavior() / withInvokeBehavior() — JDK builders
    // that toggle exact-invocation mode and return a VarHandle.
    // `sun.security.provider.SHA3.<clinit>` chains
    // `byteArrayViewVarHandle(...).withInvokeExactBehavior()`, which would
    // otherwise hit AbstractMethodError on our synthetic VarHandle. A handle
    // CratonVM minted gets a distinct copy with the other behaviour, as the
    // JDK's builders return (interpreter round i1 wave 37, lane L4;
    // `varhandle_with_exactness`) -- in `--compatible` too since JIT round 15
    // wave 1 and interpreter wave 49 (`CRATONVM_VH_EXACT_COMPATIBLE=0`
    // restores `this` there); a handle with no side-table row keeps the old
    // answer: `this`.
    //
    // DO NOT RETIRE — these two and `accessModeTypeUncached` below are
    // `declared, NO Code, not ACC_NATIVE` on all nine supported images
    // (MEASURED, `javap -p --system <image> java.lang.invoke.VarHandle`;
    // re-derivable with `scripts/jdk-only-no-image-methods.py`). The standard
    // retirement argument — "real JDK bytecode is behind it, so deleting the
    // native leaves something to run" — is FALSE for them: this registration is
    // the only implementation that exists for the receiver, and the sentence
    // above about `AbstractMethodError` is what the retirement would restore.
    // `H14-1` 4 sized that bucket at 2 rows because the corpus dispatched 2;
    // `H25-2` measured the registry and found 1,405 over 193 classes. These are
    // three of them, marked per `H25-2` N3.
    r.register(
        vh,
        "withInvokeExactBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |ctx, args| varhandle_with_exactness(ctx, args, true),
    );
    r.register(
        vh,
        "withInvokeBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |ctx, args| varhandle_with_exactness(ctx, args, false),
    );
    // Round 15 wave 1 (lane vh): `MethodHandles.collectReturnValue`, its kind
    // stated at its own row (see the block "`IndirectVarHandle`: round 15 wave 1").
    register_r15_vh_indirect_natives(r);

    r.register(
        vh,
        "accessModeTypeUncached",
        "(Ljava/lang/invoke/VarHandle$AccessType;)Ljava/lang/invoke/MethodType;",
        varhandle_access_mode_type_uncached,
    );

    // VarHandle.varType() / coordinateTypes() — the describe-yourself pair.
    //
    // Both are CONCRETE bytecode in JDK 25, so they need the `check_override`
    // entry in `vm/src/vm/vm_exec.rs` (`class_name ==
    // "java/lang/invoke/VarHandle" && ("varType", "()Ljava/lang/Class;") |
    // ("coordinateTypes", "()Ljava/util/List;")`) to be consulted at all. The
    // real bodies cannot run here: both call `accessModeType`, which reads
    // `this.vform` and walks a `VarForm`/`MethodType` chain — and a CratonVM
    // VarHandle is `alloc_concurrent_synthetic("java/lang/invoke/VarHandle",
    // VH_FIELD_COUNT)` with CratonVM's own meaning imposed on the slots, so
    // slot 0 (`vform`) holds an `Int` kind tag. See RJdkHandles:244-245.
    //
    // REGISTERED HERE, not in `phases_late/reflect_invoke.rs`'s
    // `register_p59_varhandle`, and that is the whole point: `p59` reaches the
    // registry only through `register_phase59_natives` ->
    // `register_synthetic_overrides`, which `vm_init.rs` calls ONLY under
    // `config.use_synthetic_jdk`. In `--real-jdk`/`--jdk-only` the live
    // VarHandle surface is this file's (`register_phase54_method_handle` plus
    // `register_p63_method_handles_lookup`, both called from the real-JDK arm),
    // so a `varType` registered over there is dead in exactly the mode the
    // strict corpus runs. The answers come from the WP4.2 side table
    // (`VarHandleMeta`), which `findVarHandle`/`findStaticVarHandle` already
    // populate with the field descriptor and the declaring class name — no
    // layout change needed.
    r.register(vh, "varType", "()Ljava/lang/Class;", varhandle_var_type);
    r.register(
        vh,
        "coordinateTypes",
        "()Ljava/util/List;",
        varhandle_coordinate_types,
    );

    // VarHandle.toMethodHandle(AccessMode) -> MethodHandle.
    //
    // Real JDK bytecode resolves this through `VarForm.memberName_table`, and
    // CratonVM's post-clinit fixup installs a VarForm stub whose four tables
    // are all null (`vm/src/vm/vm_util.rs`) -- so the real path produced a
    // MemberName with an EMPTY method name and the call died as
    // `NoSuchMethodError: java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite.`
    // (note the missing method name). jboss-threads' `JDKSpecific$ThreadAccess`
    // (the JDK-24+ multi-release copy) does exactly this to null out
    // `Thread.threadLocals`, so EVERY worker-thread teardown reported
    // "terminated with error".
    //
    // Our VarHandles are side-table backed, so build the equivalent synthetic
    // getter/setter MethodHandle directly from that meta -- the same object
    // `Lookup.findGetter`/`findSetter` hand out, which this VM can dispatch.
    r.register(
        vh,
        "toMethodHandle",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;",
        varhandle_to_method_handle,
    );

    // VarHandle.getMethodHandleUncached(int) -> MethodHandle: a reviewed
    // `Intrinsic` over concrete JDK bytecode (AGENTS.md), registered only
    // under `CRATONVM_VH_SYNTHETIC_MH_TABLE` (interpreter round i2 wave 57,
    // lane L4). A synthetic `VarHandle` has no `vform` for the JDK body to
    // read; every other receiver runs that body. Removal: when this VM's
    // `VarHandle`s carry a real `VarForm` (the proposal's design 1). See
    // `varhandle_get_method_handle_uncached`.
    if crate::nbflags().vh_synthetic_mh_table {
        r.register_with_kind(
            vh,
            "getMethodHandleUncached",
            "(I)Ljava/lang/invoke/MethodHandle;",
            varhandle_get_method_handle_uncached,
            NativeKind::Intrinsic,
        );
    }

    // VarHandle.isAccessModeSupported(AccessMode) -> boolean.
    //
    // The JDK body asks `vform.getMemberNameOrNull(mode.ordinal())`, and a
    // CratonVM-minted handle has no real `vform`, so it was a
    // `NullPointerException` on every handle from `findVarHandle`,
    // `findStaticVarHandle`, `unreflectVarHandle` and the byte views
    // (interpreter round i1 wave 31). Answered from the side-table meta with
    // the rules the access natives enforce.
    r.register_with_kind(
        vh,
        "isAccessModeSupported",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Z",
        varhandle_is_access_mode_supported,
        NativeKind::Bridge,
    );

    // VarHandle.describeConstable() -> Optional<VarHandleDesc>. The JDK's
    // field and array handle classes override it; `VarHandle`'s own body,
    // the one a CratonVM-minted handle runs, answers `Optional.empty()`
    // (interpreter round i1 wave 45, lane L4, `L4W45VarHandleQueries`).
    r.register_with_kind(
        vh,
        "describeConstable",
        "()Ljava/util/Optional;",
        varhandle_describe_constable,
        NativeKind::Bridge,
    );

    // VarHandle.get(Object...) → Object
    // For instance fields: args = [receiver]; for static: args = []
    r.register_with_kind(
        vh,
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get,
        NativeKind::Bridge,
    );

    // VarHandle.set(Object...) → void
    r.register_with_kind(
        vh,
        "set",
        "([Ljava/lang/Object;)V",
        varhandle_set,
        NativeKind::Bridge,
    );

    // VarHandle.compareAndSet(Object...) → boolean
    r.register_with_kind(
        vh,
        "compareAndSet",
        "([Ljava/lang/Object;)Z",
        varhandle_compare_and_set,
        NativeKind::Bridge,
    );

    // VarHandle.getAndSet(Object...) → Object
    r.register_with_kind(
        vh,
        "getAndSet",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get_and_set,
        NativeKind::Bridge,
    );

    // VarHandle.getVolatile(Object...) → Object (same as get for now — no hardware fences)
    r.register_with_kind(
        vh,
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getVolatile"),
        NativeKind::Bridge,
    );

    // VarHandle.setVolatile(Object...) → void. Its own callback, not
    // `varhandle_set`: a volatile store needs the StoreLoad fence the plain
    // `set`/`setOpaque`/`setRelease` do not (see `varhandle_set_volatile`).
    r.register_with_kind(
        vh,
        "setVolatile",
        "([Ljava/lang/Object;)V",
        varhandle_set_volatile,
        NativeKind::Bridge,
    );

    // VarHandle.getOpaque / getAcquire — read semantics
    r.register_with_kind(
        vh,
        "getOpaque",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getOpaque"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_mode(ctx, args, "getAcquire"),
        NativeKind::Bridge,
    );

    // VarHandle.setOpaque / setRelease — write semantics
    r.register_with_kind(
        vh,
        "setOpaque",
        "([Ljava/lang/Object;)V",
        |ctx, args| varhandle_set_mode(ctx, args, "setOpaque"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "setRelease",
        "([Ljava/lang/Object;)V",
        |ctx, args| varhandle_set_mode(ctx, args, "setRelease"),
        NativeKind::Bridge,
    );

    // VarHandle.compareAndExchange
    r.register_with_kind(
        vh,
        "compareAndExchange",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_compare_and_exchange,
        NativeKind::Bridge,
    );

    // VarHandle.getAndAdd and its ordering variants — single Object-return
    // registration per name. The polymorphic dispatcher in vm_exec.rs tries
    // the generic Object descriptor first and unbox_poly_return coerces the
    // result back to the call-site numeric type (H2 uses [III)I).
    r.register_with_kind(
        vh,
        "getAndAdd",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAdd"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndAddAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAddAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndAddRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_add_mode(ctx, args, "getAndAddRelease"),
        NativeKind::Bridge,
    );

    // VarHandle.getAndBitwise{Or,And,Xor} (+ ordering variants). java.net.Socket
    // updates its `state` int field via `STATE.getAndBitwiseOr(this, flag)`.
    r.register_with_kind(
        vh,
        "getAndBitwiseOr",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOr"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseOrAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOrAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseOrRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOrRelease"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseAnd",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAnd"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseAndAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAndAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseAndRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAndRelease"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseXor",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXor"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseXorAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXorAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndBitwiseXorRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXorRelease"),
        NativeKind::Bridge,
    );

    // C38: Ordering variants of compareAndSet / compareAndExchange / getAndSet.
    // The real JDK maps each to a distinct native; we use the same underlying
    // CAS / CAX / xchg implementation (we don't emit hardware fences). Needed
    // by Jackson's ConcurrentLinkedQueue which calls weakCompareAndSet.
    r.register_with_kind(
        vh,
        "weakCompareAndSet",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSet"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "weakCompareAndSetPlain",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetPlain"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "weakCompareAndSetAcquire",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "weakCompareAndSetRelease",
        "([Ljava/lang/Object;)Z",
        |ctx, args| varhandle_compare_and_set_mode(ctx, args, "weakCompareAndSetRelease"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "compareAndExchangeAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchangeAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "compareAndExchangeRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchangeRelease"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndSetAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_set_mode(ctx, args, "getAndSetAcquire"),
        NativeKind::Bridge,
    );
    r.register_with_kind(
        vh,
        "getAndSetRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| varhandle_get_and_set_mode(ctx, args, "getAndSetRelease"),
        NativeKind::Bridge,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// VarHandle operation implementations
// ---------------------------------------------------------------------------

/// JDK-ONLY-LAYOUT: does this `VarHandle` object actually have OUR six-slot
/// layout, or is it a real `java.lang.invoke.VarHandle`?
///
/// `alloc_concurrent_synthetic` asks for `VH_FIELD_COUNT` slots, but when the
/// real class is loaded the object it returns has the **real** layout —
/// `vform` (a `VarForm` reference) at 0 and `exact` (a `boolean`) at 1 on
/// JDK 21+. Writing our `VH_KIND` `Int` to slot 0 is then coerced to `null`,
/// **destroying `vform`**, and writing our `VH_CLASS` `String` to slot 1 is
/// coerced to a number, corrupting `exact`.
///
/// Measured 2026-08-04 with `CRATONVM_DBG_OVERLAY=1`: 52 hits on each of those
/// two slots from `MhUtil.findVarHandle`, reached from the `<clinit>` of
/// `java.util.concurrent.atomic.AtomicBoolean`, `AtomicReference` and
/// `java.io.ObjectInputFilter$Config`. Those are real JDK classes whose
/// `static final VarHandle` fields real bytecode uses — so a null `vform` is
/// handed straight back to the JDK. It does not fault today only because our
/// natives intercept every `VarHandle` operation and read the side table
/// instead; the moment §7 step 3 hands one of those calls to real bytecode,
/// `vform.getMethodHandle(...)` is an NPE.
///
/// The metadata is not lost by skipping the writes: `vh_meta_put` is called on
/// every allocation path and every reader consults it first (see
/// [`vh_field_desc`]). The slot writes are the fallback for VarHandles
/// allocated outside our path, which by definition do not have our layout
/// either.
/// **Ask by NAME, not by field count.** The first version of this predicate was
/// `object_num_fields(vh) >= VH_FIELD_COUNT` and was completely inert: an A/B
/// against the pre-fix binary counted the same 8 overlay writes with and
/// without it. `alloc_concurrent_synthetic` returns an object with at least the
/// requested slot count either way, so a count test cannot tell the two layouts
/// apart — it only looks as though it can.
///
/// A real `java.lang.invoke.VarHandle` declares an instance field literally
/// named `vform`; a VM-fabricated stub has generated placeholder fields and
/// does not. That is the difference, so that is what is tested.
///
/// If a future JDK renames `vform`, this reverts to today's behaviour (writing
/// the slots) rather than to something new, and `CRATONVM_DBG_OVERLAY=1` still
/// reports it — a loud failure mode, not a silent one.
pub(crate) fn vh_has_synthetic_layout(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(vh);
    !mhn_declared_fields_named(&*ctx, class_id, "vform")
        .0
        .iter()
        .any(|f| !f.is_static && f.name == "vform")
}

/// Store the three name strings of a synthetic-layout VarHandle
/// (`VH_CLASS`, `VH_FIELD`, `VH_FIELD_DESC`) and hand back `vh`'s
/// post-allocation address.
///
/// gc-common w19-c (stale-handle triage; the `common-w18b` page's second
/// instance): `alloc_instance_var_handle` / `alloc_static_var_handle` held `vh`
/// raw across all three `create_string`s. Native allocation does not collect
/// today, so this was latent; the handle is rooted here so it stays correct
/// when an allocation can.
pub(crate) fn vh_store_names(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let vh_h = scope.root(vh);
    for (slot, text) in [
        (VH_CLASS, class_name),
        (VH_FIELD, field_name),
        (VH_FIELD_DESC, field_desc),
    ] {
        let s = scope.create_string(text);
        let vh_now = scope.get(&vh_h);
        scope.set_field(vh_now, slot, Value::Object(Some(s)));
    }
    scope.get(&vh_h)
}

/// Allocate a VarHandle for an instance field.
/// `field_index` is `i32`, not `usize`, because `-1` is a MEANINGFUL value here:
/// it is the "not resolved yet" sentinel that `varhandle_get`/`varhandle_set`
/// branch on to re-resolve the field BY NAME (see their `field_idx >= 0` test).
/// Both call sites used to launder a failed `resolve_field_index` through
/// `.unwrap_or(0)`, which is not "unresolved" — it is slot 0, a real field — so
/// the by-name fallback could never run and every access silently hit the
/// receiver's first field instead.
///
/// # JVMS 6.5 census: `VarHandle` is abstract too
///
/// Everything the `MethodHandle` block on [`alloc_method_handle`] records
/// applies here, with the same verdict (leave it, keep the WARN) and one extra
/// hazard of its own. AUDITED 2026-09-01.
///
/// `java.lang.invoke.VarHandle` is `ACC_ABSTRACT`; on a stock `cratonvm Hello`
/// the census names `alloc_static_var_handle` below simply because a static
/// field handle is the first of this file's four `VarHandle` mints that boot
/// reaches. The objects are real receivers -- returned from
/// `Lookup.findVarHandle` / `findStaticVarHandle`, held in `static final
/// VarHandle` fields of real JDK classes (`AtomicBoolean`, `AtomicReference`,
/// `ObjectInputFilter$Config`), and used as the receiver of every
/// signature-polymorphic `get` / `set` / `compareAndSet` / `getAndAdd` variant.
///
/// **(a) a concrete JDK subclass is worse here than for `MethodHandle`.** The
/// candidates (`IndirectVarHandle`, the generated `VarHandle*s$Field*` species)
/// all declare their own instance fields, and [`vh_has_synthetic_layout`] asks
/// `declared_fields(class_id)` for a field literally named `vform` -- a name
/// that lives on `VarHandle` itself, not on a subclass. A subclass receiver
/// would therefore be judged to have OUR layout and the six slot writes below
/// would run over the subclass's real fields. That predicate is in this file
/// and could be widened to walk the superclass chain, but the rest of the
/// price -- `vm_exec.rs`'s
/// `is_var_handle_signature_polymorphic_receiver`, a `typecheck.rs` cast arm,
/// and initialising the JDK's `java.lang.invoke` machinery on first use -- is
/// not.
///
/// **(b)** a `cratonvm/internal/...` stand-in loses `instanceof VarHandle` and
/// `checkcast VarHandle`, which currently answer correctly precisely because
/// the class is the abstract type. Same trade as the `MethodHandle` site, same
/// refusal.
///
/// **(c)** and the WARN cannot be silenced from this file even if both were
/// done: `java/lang/invoke/VarHandle` is minted at nine further sites in
/// `native-builtins/src/phases_late/foreign_ffm.rs` and
/// `native-builtins/src/phases_late/reflect_invoke.rs`, and the per-class dedupe
/// would just re-point `requester=` at one of them.
///
/// `class_name` / `class_id` are the handle's coordinate; `holder` is the
/// class the field was resolved in ([`VarHandleMeta::holder_class_id`]).
///
/// nio21 wave 11 L1 re-read this audit against HEAD and left the mint as it
/// is: the Spring Boot sample's `--compatible` census still names it, and the
/// way out (a `JdkInvoke` alignment row, so the JDK builds the handle as under
/// `--jdk-only`) is recorded on
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(crate) fn alloc_instance_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
    field_index: i32,
    class_id: cratonvm_types::ClassId,
    holder: Option<cratonvm_types::ClassId>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut vh: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Only on OUR layout — see `vh_has_synthetic_layout`. On a real
    // `VarHandle` these six writes null `vform` and corrupt `exact`.
    if vh_has_synthetic_layout(ctx, vh) {
        ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_INSTANCE));
        vh = vh_store_names(ctx, vh, class_name, field_name, field_desc);
        ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(field_index));
        ctx.set_field(vh, VH_CLASS_ID, Value::Int(class_id.as_u32() as i32));
    }
    // Resolved once, here, for the JIT's thin write/CAS helpers — see
    // `VarHandleMeta::ref_type_class_id`. Both handle-minting callers pass a
    // `Class` mirror of the field type, so the type is already loaded; the
    // lookup only names it and never loads anything.
    // For a one-dimensional reference-array field it is the COMPONENT's id
    // instead (rt proposal W8-2); the plan tells the two apart by descriptor.
    let ref_type_class_id = vh_ref_type_lookup_name(field_desc)
        .or_else(|| vh_ref_array_component_lookup_name(field_desc))
        .and_then(|name| ctx.class_id_by_name(name))
        .map_or(0, |cid| cid.as_u32());
    // WP4.2: also stash in the side table so the descriptor-aware setter
    // on real-JDK VarHandle layout doesn't drop our metadata.
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: class_name.to_string(),
            field_name: field_name.to_string(),
            field_desc: field_desc.to_string(),
            field_index,
            class_id: class_id.as_u32(),
            read_only: false,
            exact: None,
            ref_type_class_id,
            holder_class_id: holder.map_or(0, |cid| cid.as_u32()),
        },
    );
    Ok(vh_install_real_varform(ctx, vh))
}

/// Allocate a VarHandle for a static field.
///
/// The JVMS 6.5 audit of this abstract-class mint is on
/// [`alloc_instance_var_handle`]; this is the site the boot WARN happens to
/// name, and the verdict there covers it unchanged.
pub(crate) fn alloc_static_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
    owner: Option<ClassId>,
) -> Result<ObjectRef, MethodCallFailed> {
    let owner_raw = owner.map_or(0, |cid| cid.as_u32());
    let mut vh: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Only on OUR layout — see `vh_has_synthetic_layout`.
    if vh_has_synthetic_layout(ctx, vh) {
        ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_STATIC));
        vh = vh_store_names(ctx, vh, class_name, field_name, field_desc);
        ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(-1)); // resolved lazily
        ctx.set_field(vh, VH_CLASS_ID, Value::Int(owner_raw as i32));
    }
    // WP4.2: side table for descriptor-aware-coercion-safe metadata access.
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_STATIC,
            class_name: class_name.to_string(),
            field_name: field_name.to_string(),
            field_desc: field_desc.to_string(),
            field_index: -1,
            class_id: owner_raw,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    Ok(vh_install_real_varform(ctx, vh))
}

// ---------------------------------------------------------------------------
// i57-L4 stages 1 and 2: a real `VarForm` for a minted field handle
// ---------------------------------------------------------------------------
//
// Interpreter round i6 wave 123, lane L4 (`CRATONVM_VH_REAL_VARFORM`, default
// off; `--compatible` only; proposal
// `docs/feature-designs/native-dispatch-architecture.md`).
//
// A field `VarHandle` this VM mints is the abstract `java/lang/invoke/VarHandle`
// with `vform == null`, so every JDK body that reads `vform` needs a native in
// front of it. Under the switch, a PLAIN (not exact) instance or static field
// handle gets a real `VarForm` built by the JDK's own constructor,
// `VarForm(implClass, receiver, value, intermediates...)`, exactly as
// `VarHandle<T>s.FieldInstanceReadOnly.FORM` is on HotSpot. Its `implClass` is
// one of the embedded classes below (stage 1): static methods named after the
// access modes, of the form's erased type with the `VarHandle` first, each one
// signature-polymorphic call on the handle (the natives serve it). A mode with
// no method is unsupported: `VarForm.resolveMemberName` answers `null`, which is
// what makes the JDK's `isAccessModeSupported` and `getMemberName`'s
// `UnsupportedOperationException` the JDK's own. One form per (class, shape).
//
// Interpreter round i7 wave 136, lane L4 (page
// `i135-L4-real-varform-classes-are-application-visible`): the classes are
// defined HIDDEN (JEP 371), so `Class.forName` never finds them and a trace
// never names a `cratonvm.internal` frame (a hidden class's frames are hidden).
// HotSpot lists its form method's frame there
// (`java.lang.invoke.VarHandleInts$FieldInstanceReadOnly.get`); this VM now
// lists nothing in its place, a missing line instead of a wrong one (the
// residual is recorded on the page). The read-write class links to its read-only
// superclass by id (`DefineClassFull::superclass_id_override`). Their ids and
// the shared forms (as global roots) live in a per-VM record
// ([`VhRealVarFormRecord`], `NativeContext::vh_real_varform_slot`), not in
// the classes' `FORM_INSTANCE` / `FORM_STATIC` statics, which stay `null`.
//
// The mint is reached only where this VM mints a field handle: under
// `--compatible` with the `jdk-invoke` alignment row OFF
// (`CRATONVM_COMPAT_ALIGN=-jdk-invoke`), since that row (default ON) leaves
// `Lookup.findVarHandle` / `findStaticVarHandle` / `unreflectVarHandle` to the
// JDK, whose handles carry the JDK's own forms.
//
// Not given a form, and so served as before: an exact copy
// (`withInvokeExactBehavior`; the helpers' erased call sites would be refused
// by its exactness check), array element and byte-view handles, and every
// handle under `--jdk-only` (the JDK's factories run there).
//
// Sources and the generator: `native-builtins/src/class_bytes/vhform/`.

/// One variable type's pair of embedded `implClass`es: the read-only class
/// (the read modes) and the read-write class that extends it.
pub(crate) struct VhFormClass {
    pub(crate) letter: u8,
    pub(crate) primitive: Option<&'static str>,
    pub(crate) ro_name: &'static str,
    pub(crate) ro_bytes: &'static [u8],
    pub(crate) rw_name: &'static str,
    pub(crate) rw_bytes: &'static [u8],
}

pub(crate) static VH_REAL_VARFORM_CLASSES: [VhFormClass; 9] = [
    VhFormClass {
        letter: b'Z',
        primitive: Some("boolean"),
        ro_name: "cratonvm/internal/VhFormBooleanReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormBooleanReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormBoolean",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormBoolean.class"),
    },
    VhFormClass {
        letter: b'B',
        primitive: Some("byte"),
        ro_name: "cratonvm/internal/VhFormByteReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormByteReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormByte",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormByte.class"),
    },
    VhFormClass {
        letter: b'C',
        primitive: Some("char"),
        ro_name: "cratonvm/internal/VhFormCharReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormCharReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormChar",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormChar.class"),
    },
    VhFormClass {
        letter: b'S',
        primitive: Some("short"),
        ro_name: "cratonvm/internal/VhFormShortReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormShortReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormShort",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormShort.class"),
    },
    VhFormClass {
        letter: b'I',
        primitive: Some("int"),
        ro_name: "cratonvm/internal/VhFormIntReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormIntReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormInt",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormInt.class"),
    },
    VhFormClass {
        letter: b'J',
        primitive: Some("long"),
        ro_name: "cratonvm/internal/VhFormLongReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormLongReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormLong",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormLong.class"),
    },
    VhFormClass {
        letter: b'F',
        primitive: Some("float"),
        ro_name: "cratonvm/internal/VhFormFloatReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormFloatReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormFloat",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormFloat.class"),
    },
    VhFormClass {
        letter: b'D',
        primitive: Some("double"),
        ro_name: "cratonvm/internal/VhFormDoubleReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormDoubleReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormDouble",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormDouble.class"),
    },
    VhFormClass {
        letter: b'L',
        primitive: None,
        ro_name: "cratonvm/internal/VhFormReferenceReadOnly",
        ro_bytes: include_bytes!("../class_bytes/vhform/VhFormReferenceReadOnly.class"),
        rw_name: "cratonvm/internal/VhFormReference",
        rw_bytes: include_bytes!("../class_bytes/vhform/VhFormReference.class"),
    },
];

/// The prefix every `VH_REAL_VARFORM_CLASSES` name shares. Since wave 136
/// [`vh_has_real_varform_of_ours`] compares ids with the per-VM record instead.
pub(crate) const VH_REAL_VARFORM_PREFIX: &str = "cratonvm/internal/VhForm";

/// The embedded classes for a field descriptor's variable type (`L` and `[`
/// share the reference class: the form's types are erased).
pub(crate) fn vh_real_varform_class(field_desc: &str) -> Option<&'static VhFormClass> {
    let letter = match field_desc.as_bytes().first()? {
        b'L' | b'[' => b'L',
        other => *other,
    };
    VH_REAL_VARFORM_CLASSES.iter().find(|c| c.letter == letter)
}

/// The per-VM record of `CRATONVM_VH_REAL_VARFORM` (interpreter round i7 wave
/// 136, lane L4; page `i135-L4-real-varform-classes-are-application-visible`),
/// in `NativeContext::vh_real_varform_slot`: the hidden form classes this VM
/// defined and its shared forms. Atomics only (no lock is held across a
/// `NativeContext` call).
#[derive(Default)]
pub(crate) struct VhRealVarFormRecord {
    /// Per `VH_REAL_VARFORM_CLASSES` row, `[read-only, read-write]`: the
    /// hidden class's raw `ClassId` plus one (`0`: not defined yet).
    pub(crate) ids: [[std::sync::atomic::AtomicU32; 2]; 9],
    /// Per row, `[read-only, read-write]`, `[static, instance]`: the global
    /// root (`NativeContext::add_global_root`) of the shared form (`0`: none
    /// yet). A global root lives as long as the VM and moves with the form.
    pub(crate) forms: [[[std::sync::atomic::AtomicUsize; 2]; 2]; 9],
    /// A define of form classes is in flight: another minter declines (its
    /// handle is served as before) rather than wait, so no thread blocks here
    /// and a re-entry cannot deadlock.
    pub(crate) defining: std::sync::atomic::AtomicBool,
}

/// THIS VM's record, created at the first call; `None` for a context with no
/// per-VM slot (the mocks: no form is built).
pub(crate) fn vh_real_varform_record(ctx: &dyn NativeContext) -> Option<&VhRealVarFormRecord> {
    let slot = ctx.vh_real_varform_slot()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(VhRealVarFormRecord::default())
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<VhRealVarFormRecord>()
}

/// `class`'s row in `VH_REAL_VARFORM_CLASSES` (and in the record).
pub(crate) fn vh_real_varform_row(class: &VhFormClass) -> Option<usize> {
    VH_REAL_VARFORM_CLASSES
        .iter()
        .position(|c| c.letter == class.letter)
}

/// The recorded id of row `row`'s read-only (`rw == false`) or read-write class.
pub(crate) fn vh_real_varform_recorded_id(ctx: &dyn NativeContext, row: usize, rw: bool) -> Option<ClassId> {
    let raw = vh_real_varform_record(ctx)?.ids.get(row)?[usize::from(rw)]
        .load(std::sync::atomic::Ordering::Acquire);
    raw.checked_sub(1).map(ClassId::new)
}

/// Define one embedded class HIDDEN, under its own name (unique in this VM
/// unless something else took the name: then the class manager mangles it and
/// a read-write define over it is refused, fail-closed), linked to
/// `superclass` by id when given. Runs no Java (a hidden define is offered to
/// no transformer); a GC point.
pub(crate) fn vh_real_varform_define_hidden(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: &[u8],
    superclass: Option<ClassId>,
) -> Option<ClassId> {
    ctx.define_class_full(
        name,
        bytes,
        0,
        cratonvm_native_api::DefineClassFull {
            override_name: Some(name.to_string()),
            hidden: true,
            superclass_id_override: superclass,
            ..Default::default()
        },
    )
    .ok()
}

/// The hidden class of row `row` (`rw`: the read-write one), defined once per
/// VM: the read-only class first, since it is the read-write class's
/// superclass. `None` when a define fails or another thread is defining (the
/// handle then stays as it is).
pub(crate) fn vh_real_varform_define(
    ctx: &mut dyn NativeContext,
    row: usize,
    class: &VhFormClass,
    rw: bool,
) -> Option<ClassId> {
    use std::sync::atomic::Ordering;
    if let Some(cid) = vh_real_varform_recorded_id(&*ctx, row, rw) {
        return Some(cid);
    }
    vh_real_varform_record(&*ctx)?
        .defining
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let defined = vh_real_varform_define_held(ctx, row, class, rw);
    if let Some(record) = vh_real_varform_record(&*ctx) {
        record.defining.store(false, Ordering::Release);
    }
    defined
}

/// [`vh_real_varform_define`]'s defines, run while it holds the record's
/// `defining` word: each class not recorded yet is defined and recorded.
pub(crate) fn vh_real_varform_define_held(
    ctx: &mut dyn NativeContext,
    row: usize,
    class: &VhFormClass,
    rw: bool,
) -> Option<ClassId> {
    use std::sync::atomic::Ordering;
    let ro = match vh_real_varform_recorded_id(&*ctx, row, false) {
        Some(ro) => ro,
        None => {
            let ro = vh_real_varform_define_hidden(ctx, class.ro_name, class.ro_bytes, None)?;
            vh_real_varform_record(&*ctx)?.ids[row][0]
                .store(ro.as_u32().checked_add(1)?, Ordering::Release);
            ro
        }
    };
    if !rw {
        return Some(ro);
    }
    match vh_real_varform_recorded_id(&*ctx, row, true) {
        Some(cid) => Some(cid),
        None => {
            let cid = vh_real_varform_define_hidden(ctx, class.rw_name, class.rw_bytes, Some(ro))?;
            vh_real_varform_record(&*ctx)?.ids[row][1]
                .store(cid.as_u32().checked_add(1)?, Ordering::Release);
            Some(cid)
        }
    }
}

/// The recorded shared form of row `row`, `rw`, `instance`, where it is now.
pub(crate) fn vh_real_varform_recorded_form(
    ctx: &dyn NativeContext,
    row: usize,
    rw: bool,
    instance: bool,
) -> Option<ObjectRef> {
    let handle = vh_real_varform_record(ctx)?.forms.get(row)?[usize::from(rw)]
        [usize::from(instance)]
    .load(std::sync::atomic::Ordering::Acquire);
    if handle == 0 {
        return None;
    }
    ctx.resolve_global_root(handle)
}

/// File `form` as the shared form of row `row`, `rw`, `instance`, unless
/// another thread filed one first (then that one is answered and `form` is
/// dropped: two valid forms, one kept). Allocates nothing; no GC point.
pub(crate) fn vh_real_varform_file_form(
    ctx: &mut dyn NativeContext,
    row: usize,
    rw: bool,
    instance: bool,
    form: ObjectRef,
) -> ObjectRef {
    use std::sync::atomic::Ordering;
    let handle = ctx.add_global_root(form);
    if handle == 0 {
        return form;
    }
    let filed = vh_real_varform_record(&*ctx).map(|record| {
        record.forms[row][usize::from(rw)][usize::from(instance)].compare_exchange(
            0,
            handle,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    });
    match filed {
        Some(Ok(_)) => form,
        Some(Err(winner)) => {
            ctx.remove_global_root(handle);
            ctx.resolve_global_root(winner).unwrap_or(form)
        }
        None => {
            ctx.remove_global_root(handle);
            form
        }
    }
}

/// The shared `VarForm` of `class` (read-only or read-write) for an instance
/// (`(Object)T` get type) or static (`()T`) field: the one in this VM's record,
/// or a new one built by the JDK's constructor and recorded. `None` when
/// anything cannot be defined or built (the handle then stays as it is).
/// Allocates and runs Java: the caller pins what it holds.
pub(crate) fn vh_real_varform_shared(
    ctx: &mut dyn NativeContext,
    class: &VhFormClass,
    read_only: bool,
    instance: bool,
) -> Option<ObjectRef> {
    let row = vh_real_varform_row(class)?;
    let rw = !read_only;
    if let Some(form) = vh_real_varform_recorded_form(&*ctx, row, rw, instance) {
        return Some(form);
    }
    let cid = vh_real_varform_define(ctx, row, class, rw)?;
    // No `<clinit>` (the statics have no initializer): this only marks the
    // class initialized, so the first call through a form meets no
    // initialization barrier. (Its `FORM_INSTANCE` / `FORM_STATIC` statics are
    // no longer written: the forms live in the record.)
    ctx.ensure_class_initialized_with_class_id(cid).ok()?;
    let object_id = ctx.class_id_by_name("java/lang/Object")?;
    let class_class_id = ctx.class_id_by_name("java/lang/Class")?;
    // Every mirror and the empty `Class[]` are pinned across the allocations
    // that follow them and the constructor call.
    let impl_mirror = ctx.get_class_mirror(cid);
    let base = ctx.pin_native_root(impl_mirror);
    let object_mirror = ctx.get_class_mirror(object_id);
    let object_pin = ctx.pin_native_root(object_mirror);
    let value_mirror = match class.primitive {
        Some(name) => ctx.primitive_class_mirror(name),
        None => ctx.read_native_pin(object_pin, object_mirror),
    };
    let value_pin = ctx.pin_native_root(value_mirror);
    let none = ctx.new_ref_array(class_class_id, 0);
    let none_pin = ctx.pin_native_root(none);
    let impl_mirror = ctx.read_native_pin(base, impl_mirror);
    let object_mirror = ctx.read_native_pin(object_pin, object_mirror);
    let value_mirror = ctx.read_native_pin(value_pin, value_mirror);
    let none = ctx.read_native_pin(none_pin, none);
    // `VarForm(implClass, receiver, value, intermediate...)`: a `null`
    // receiver is a static field's form (no coordinate), as the JDK's
    // `FieldStaticReadOnly.FORM` is built.
    let receiver = if instance {
        Value::Object(Some(object_mirror))
    } else {
        Value::Object(None)
    };
    let built = ctx.new_object_initialized(
        "java/lang/invoke/VarForm",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)V",
        &[
            Value::Object(Some(impl_mirror)),
            receiver,
            Value::Object(Some(value_mirror)),
            Value::Object(Some(none)),
        ],
    );
    ctx.unpin_native_roots(base);
    let form = match built {
        Ok(Some(Value::Object(Some(form)))) => form,
        _ => return None,
    };
    Some(vh_real_varform_file_form(ctx, row, rw, instance, form))
}

/// Give `vh`, a handle this VM just minted (or just marked read-only), the
/// real `VarForm` of its row under `CRATONVM_VH_REAL_VARFORM` (see the block
/// comment above). Answers `vh`'s current address (building a form is a GC
/// point). A no-op when the switch is off, under `--jdk-only`, for a
/// synthetic-layout handle, an exact one, or any kind but an instance or static
/// field; and when the form cannot be built (the handle keeps `vform == null`
/// and the natives in front serve it, as before).
pub(crate) fn vh_install_real_varform(ctx: &mut dyn NativeContext, vh: ObjectRef) -> ObjectRef {
    if !crate::nbflags().vh_real_varform || ctx.is_jdk_only() || vh_has_synthetic_layout(ctx, vh)
    {
        return vh;
    }
    let Some(meta) = vh_meta_get(ctx, vh) else {
        return vh;
    };
    if meta.exact.is_some() || !(meta.kind == VH_KIND_INSTANCE || meta.kind == VH_KIND_STATIC) {
        return vh;
    }
    let Some(class) = vh_real_varform_class(&meta.field_desc) else {
        return vh;
    };
    let (read_only, instance) = (meta.read_only, meta.kind == VH_KIND_INSTANCE);
    drop(meta);
    let pin = ctx.pin_native_root(vh);
    let form = vh_real_varform_shared(ctx, class, read_only, instance);
    let vh = ctx.read_native_pin(pin, vh);
    ctx.unpin_native_roots(pin);
    let Some(form) = form else {
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!("[VH_REAL_VARFORM] no form for a {} handle: served as before", class.rw_name);
        }
        return vh;
    };
    let class_id = ctx.class_id_of_object(vh);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "vform") {
        ctx.set_field(vh, slot, Value::Object(Some(form)));
        if crate::nbflags().dbg_mh_dispatch {
            let name = if read_only {
                class.ro_name
            } else {
                class.rw_name
            };
            let shape = if instance { "instance" } else { "static" };
            eprintln!("[VH_REAL_VARFORM] {shape} field handle: VarForm over {name}");
        }
    }
    vh
}

/// `vh` carries a `VarForm` [`vh_install_real_varform`] gave it (its
/// `implClass` is one of `VH_REAL_VARFORM_CLASSES`), so the JDK's own bodies
/// of `isAccessModeSupported` / `toMethodHandle` answer it. `false` whenever
/// `CRATONVM_VH_REAL_VARFORM` is off. Reads only.
pub(crate) fn vh_has_real_varform_of_ours(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    if !crate::nbflags().vh_real_varform {
        return false;
    }
    let Value::Object(Some(form)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return false;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, form, "java/lang/invoke/VarForm", "implClass")
    else {
        return false;
    };
    // Wave 136 (page `i135-L4-real-varform-classes-are-application-visible`):
    // by id against this VM's record, not by name prefix, so a class an
    // application defined under a `cratonvm/internal/VhForm` name is never
    // taken for one of ours.
    let Some(raw) = ctx
        .class_id_from_mirror(impl_mirror)
        .and_then(|cid| cid.as_u32().checked_add(1))
    else {
        return false;
    };
    vh_real_varform_record(&*ctx).is_some_and(|record| {
        record
            .ids
            .iter()
            .flatten()
            .any(|id| id.load(std::sync::atomic::Ordering::Acquire) == raw)
    })
}

#[cfg(test)]
mod i57_w123_real_varform_tests {
    use super::*;

    /// Every embedded class is a Java 25 class file under the shared prefix,
    /// and every field descriptor maps to its variable type's pair.
    #[test]
    fn every_embedded_form_class_is_a_class_file_and_every_type_maps() {
        for c in &VH_REAL_VARFORM_CLASSES {
            for (name, bytes) in [(c.ro_name, c.ro_bytes), (c.rw_name, c.rw_bytes)] {
                assert!(name.starts_with(VH_REAL_VARFORM_PREFIX), "{name}");
                assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "{name}");
                assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 69, "{name}");
            }
            assert_eq!(c.primitive.is_none(), c.letter == b'L');
        }
        for (desc, rw) in [
            ("I", "cratonvm/internal/VhFormInt"),
            ("Z", "cratonvm/internal/VhFormBoolean"),
            ("Ljava/lang/String;", "cratonvm/internal/VhFormReference"),
            ("[I", "cratonvm/internal/VhFormReference"),
            ("D", "cratonvm/internal/VhFormDouble"),
        ] {
            assert_eq!(vh_real_varform_class(desc).map(|c| c.rw_name), Some(rw), "{desc}");
        }
        assert!(vh_real_varform_class("").is_none());
        assert!(vh_real_varform_class("V").is_none());
    }

    /// Interpreter round i7 wave 136, lane L4: every class pair has its own
    /// row in the per-VM record, and a fresh record holds no id and no form.
    #[test]
    fn i136_l4_every_form_class_has_a_record_row() {
        let rows: Vec<usize> = VH_REAL_VARFORM_CLASSES
            .iter()
            .map(|c| vh_real_varform_row(c).expect("row"))
            .collect();
        assert_eq!(rows, (0..VH_REAL_VARFORM_CLASSES.len()).collect::<Vec<_>>());
        let record = VhRealVarFormRecord::default();
        assert_eq!(record.ids.len(), VH_REAL_VARFORM_CLASSES.len());
        assert_eq!(record.forms.len(), VH_REAL_VARFORM_CLASSES.len());
        use std::sync::atomic::Ordering;
        assert!(record.ids.iter().flatten().all(|id| id.load(Ordering::Relaxed) == 0));
        assert!(record
            .forms
            .iter()
            .flatten()
            .flatten()
            .all(|f| f.load(Ordering::Relaxed) == 0));
        assert!(!record.defining.load(Ordering::Relaxed));
    }
}

/// Resolve a STATIC VarHandle's storage slot: `(class_id, static-block index)`.
///
/// The value of a static field lives in the class's static-field storage, NOT
/// in the `Class` mirror object's instance slots. The earlier STATIC branches
/// used `get_field_by_name(mirror, field)` / `set_field_by_name(mirror, field)`,
/// which read/write the *mirror object* — so the lookup found no such field and
/// silently returned the default (0) / dropped the write. Callers must instead
/// go through `get_static_field` / `set_static_field` with the index this
/// resolves. Returns `Ok(None)` if the class isn't loaded or has no such
/// static field, and the `<clinit>` failure (`ExceptionInInitializerError`,
/// then `NoClassDefFoundError`) as `Err`, as `getstatic` / `putstatic` would
/// throw it; the handle used to answer a default and drop the write (wave 17).
///
/// `live` holds the caller's operand values (the value to store, the CAS
/// witness): the holder's `<clinit>` is a GC point, so they are pinned across
/// it and refreshed in place (only when the holder is not yet initialized).
///
/// `init` names the access (wave 59, lane L4): a recorded holder's
/// initialization failure lists `LazyInitializingVarHandle`'s frames
/// ([`vh_static_init_chain`]).
pub(crate) fn vh_static_slot(
    ctx: &mut dyn NativeContext,
    class: &str,
    field: &str,
    owner: Option<ClassId>,
    live: &mut [Value],
    init: VhStaticInit,
) -> Result<Option<(ClassId, usize)>, MethodCallFailed> {
    // Mirror HotSpot: the first ACCESS through a static-field VarHandle triggers
    // the holder class's `<clinit>` (as `getstatic`/`putstatic` would). Without
    // this, a static field read via `findStaticVarHandle().get()` on a not-yet-
    // initialized class returns the field's default (`null`/0) instead of the
    // value its `<clinit>` assigns — e.g. Caffeine's `LocalCacheFactory.newFactory`
    // reads `<generated>.FACTORY` (set in that class's `<clinit>`) and got `null`,
    // failing every Hibernate SessionFactory build. Idempotent + cheap once the
    // class is initialized; the recursive-init guard handles the in-`<clinit>`
    // case (the initializing thread may access the field during its own clinit).
    // Strict mode records the exact class the handle was minted against; two
    // loaders that define one name are two classes, and by-name resolution
    // reads and initialises the wrong one (see `mh_static_owner_of_mirror`).
    //
    // An INHERITED static (`findStaticVarHandle(C.class, "f", ..)` for an `f`
    // that `P` declares) lives in, and initializes, its declaring class: the
    // by-name arm below found no `f` in `C` and answered nothing (wave 16).
    let named = match owner {
        Some(cid) => Some(cid),
        None => ctx.class_id_by_name(class),
    };
    let inherited = named.and_then(|n| inherited_static_field_holder(&*ctx, n, field));
    if let Some(holder) = inherited {
        vh_initialize_static_holder(ctx, holder, live, init)?;
        return Ok(ctx
            .static_field_index_by_name(holder, field)
            .map(|idx| (holder, idx)));
    }
    let cid = match owner {
        Some(cid) => {
            vh_initialize_static_holder(ctx, cid, live, init)?;
            cid
        }
        None => {
            let settled = named.is_some_and(|c| ctx.is_class_initialized(c));
            let (base, handles) = if settled {
                (usize::MAX, Vec::new())
            } else {
                pin_mh_args(ctx, live)
            };
            // A holder not yet initialized lists `LazyInitializingVarHandle`'s
            // frames (wave 59, lane L4); a settled one costs what it did.
            let init = if !settled && vh_static_mode_method(init.mode, init.variable).is_some() {
                ctx.ensure_class_initialized_served(class, &|| {
                    vh_static_init_chain(init).unwrap_or_else(|| {
                        cratonvm_native_api::ServedRaiseChain {
                            throwable_class: "java/lang/ExceptionInInitializerError",
                            hops: Vec::new(),
                        }
                    })
                })
            } else {
                ctx.ensure_class_initialized(class)
            };
            vh_refresh_live(ctx, base, &handles, live);
            // A class that does not load has no slot (the old answer); a
            // class that loads and fails its `<clinit>` throws.
            let Some(cid) = ctx.class_id_by_name(class) else {
                return Ok(None);
            };
            init?;
            cid
        }
    };
    Ok(ctx
        .static_field_index_by_name(cid, field)
        .map(|idx| (cid, idx)))
}

/// Initialize a static VarHandle's holder, keeping `live` valid across the
/// `<clinit>` (see [`vh_static_slot`]). Pins only when the holder is not yet
/// initialized, so a steady-state access costs one state read.
pub(crate) fn vh_initialize_static_holder(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    live: &mut [Value],
    site: VhStaticInit,
) -> Result<(), MethodCallFailed> {
    if ctx.is_class_initialized(holder) {
        return ctx.ensure_class_initialized_with_class_id(holder);
    }
    vh_initialize_static_holder_cold(ctx, holder, live, site)
}

#[cold]
#[inline(never)]
pub(crate) fn vh_initialize_static_holder_cold(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    live: &mut [Value],
    site: VhStaticInit,
) -> Result<(), MethodCallFailed> {
    let (base, handles) = if live.is_empty() {
        (usize::MAX, Vec::new())
    } else {
        pin_mh_args(ctx, live)
    };
    let init = match vh_static_init_chain(site) {
        Some(chain) => ctx.initialize_class_served(holder, chain),
        None => ctx.ensure_class_initialized_with_class_id(holder),
    };
    vh_refresh_live(ctx, base, &handles, live);
    init
}

/// The access a static VarHandle's holder is initialized for (wave 59, lane
/// L4): the access mode's name and the variable's descriptor byte (`0`: not
/// known, no frames are listed).
#[derive(Clone, Copy, Debug)]
pub(crate) struct VhStaticInit {
    pub(crate) mode: &'static str,
    pub(crate) variable: u8,
}

/// [`VhStaticInit`] of the access `mode` through the handle whose row is
/// `meta`.
#[inline]
pub(crate) fn vh_static_init(mode: &'static str, meta: Option<&VarHandleMeta>) -> VhStaticInit {
    VhStaticInit {
        mode,
        variable: meta
            .and_then(|m| m.field_desc.as_bytes().first().copied())
            .unwrap_or(0),
    }
}

// ── j27 wave 1 (lane FFMVH; proposal
// `docs/known-issues/jit/j27-static-varhandle-slot-memo-proposal-20261028.md`,
// page `jr26-static-varhandle-rmw-is-a-native-call-FIXED-20261029.md`): the static
// arms' `(holder, slot)` memoised per handle ──────────────────────────────────
//
// Every static `VarHandle` access through the natives cloned the row's class
// and field names and re-ran `vh_static_slot` -- the inherited-holder walk,
// the holder's initialization test and a by-name static-field lookup -- for
// an answer that cannot change once the holder is initialized: the handle's
// class and field are final, and a static's slot is fixed for the class's
// life (redefinition may not change a class's fields).
//
// A line is filled only after `vh_static_slot` answered for an INITIALIZED
// holder, so the `<clinit>` paths (and their `LazyInitializingVarHandle`
// frames, `ExceptionInInitializerError` / `NoClassDefFoundError`) always run
// through `vh_static_slot` exactly as before. The line is keyed as
// [`vh_memo_lookup`]'s lines are -- VM identity, identity hash, ADDRESS and
// [`VH_META_GENERATION`] (moved by every moving collection and every change
// to the handle tables) -- plus the VM's count of unloaded classes, so an
// unload anywhere discards every line (a `ClassId` is never served past the
// unload of its class). A hit is also re-checked against
// `is_class_initialized`, which a class reset in place (a stub upgraded,
// `ClassInitBits::note_init_reset`) clears. No line is kept for a context
// without a VM identity (`0`: mock contexts share it). Per thread, per VM: no
// process state, and nothing a dropped VM's line could pin (ids only).

/// `CRATONVM_VH_STATIC_SLOT_MEMO` (j27 wave 1, lane FFMVH; perf, default ON,
/// `=0` keeps every static `VarHandle` access on the full by-name resolution).
/// Read only when a line would be FILLED (once per handle, thread and
/// generation), never on a hit: with the switch off no line is ever filled,
/// so no hit can happen.
pub(crate) fn vh_static_slot_memo_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_STATIC_SLOT_MEMO")
}

/// One memoised static slot (see above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhStaticSlotLine {
    pub(crate) class_id: ClassId,
    pub(crate) slot: usize,
    /// The VM's `unloaded_class_count` when the line was filled.
    pub(crate) unloaded: u64,
}

thread_local! {
    /// Per-thread memo of [`vh_static_slot_of`].
    pub(crate) static VH_STATIC_SLOT_MEMO: std::cell::RefCell<Vec<VhMemoLine<VhStaticSlotLine>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The key of `vh`'s line, read BEFORE the resolution (which can run a
/// `<clinit>`, and so a collection that moves `vh`: the generation then moves
/// on and the line filled under this key is never served).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhStaticSlotKey {
    vm: usize,
    hash: i32,
    addr: usize,
    generation: u64,
    unloaded: u64,
}

pub(crate) fn vh_static_slot_key(ctx: &dyn NativeContext, vh: ObjectRef) -> VhStaticSlotKey {
    VhStaticSlotKey {
        vm: ctx.vm_identity(),
        hash: ctx.identity_hash_code(vh),
        addr: vh.as_ptr() as usize,
        generation: VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire),
        unloaded: ctx.unloaded_class_count(),
    }
}

/// The memoised `(holder, slot)` under `key`, or `None`.
pub(crate) fn vh_static_slot_memo_get(
    ctx: &dyn NativeContext,
    key: VhStaticSlotKey,
) -> Option<(ClassId, usize)> {
    if key.vm == 0 {
        return None;
    }
    let index = (key.hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let line = VH_STATIC_SLOT_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[index];
        if line.vm == key.vm
            && line.key == key.hash
            && line.addr == key.addr
            && line.generation == key.generation
        {
            line.value.filter(|v| v.unloaded == key.unloaded)
        } else {
            None
        }
    })?;
    ctx.is_class_initialized(line.class_id)
        .then_some((line.class_id, line.slot))
}

/// File `(class_id, slot)` under `key` (a no-op without a VM identity).
pub(crate) fn vh_static_slot_memo_put(key: VhStaticSlotKey, class_id: ClassId, slot: usize) {
    if key.vm == 0 {
        return;
    }
    let index = (key.hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    VH_STATIC_SLOT_MEMO.with(|memo| {
        memo.borrow_mut()[index] = VhMemoLine {
            vm: key.vm,
            key: key.hash,
            addr: key.addr,
            generation: key.generation,
            value: Some(VhStaticSlotLine {
                class_id,
                slot,
                unloaded: key.unloaded,
            }),
        };
    });
}

/// [`vh_static_slot`] for the static `VarHandle` `this` (its row `meta`, or
/// its own `VH_CLASS` / `VH_FIELD` strings), through the per-handle memo
/// above. Same answers, same errors, same `live` contract: a hit touches
/// nothing (no `<clinit>` can run for an initialized holder).
pub(crate) fn vh_static_slot_of(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    live: &mut [Value],
    init: VhStaticInit,
) -> Result<Option<(ClassId, usize)>, MethodCallFailed> {
    let key = vh_static_slot_key(&*ctx, this);
    if let Some(hit) = vh_static_slot_memo_get(&*ctx, key) {
        return Ok(Some(hit));
    }
    let (class, field) = vh_static_class_field(ctx, this, meta);
    let owner = vh_static_owner(meta);
    let found = vh_static_slot(ctx, &class, &field, owner, live, init)?;
    if let Some((class_id, slot)) = found {
        if ctx.is_class_initialized(class_id) {
            if key.vm != 0 && vh_static_slot_memo_enabled() {
                vh_static_slot_memo_put(key, class_id, slot);
            }
            vh_static_note_target(&*ctx, this, meta, class_id, slot);
        }
    }
    Ok(found)
}

/// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): tell the VM which
/// static the handle `this` names, once its holder is initialized -- the
/// filing of the compiled static constant-handle cells
/// (`vm/src/jit/vh_static_cells.rs`). Asked where the memo above fills a line
/// (once per handle, thread and generation), so a hit costs nothing. Only a
/// handle with a row may be written inline: its row says read-write and not
/// invoke-exact.
fn vh_static_note_target(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
    class_id: ClassId,
    slot: usize,
) {
    let writable = meta.is_some_and(|m| !m.read_only && m.exact.is_none());
    let desc = || match meta.map(|m| m.field_desc.as_str()) {
        Some("I") => Some(b'I'),
        Some("J") => Some(b'J'),
        _ => None,
    };
    cratonvm_native_api::NativeSystemAccess::note_static_varhandle_target(
        ctx, this, class_id, slot, writable, &desc,
    );
}

#[cfg(test)]
mod j27_static_slot_memo_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const INIT: VhStaticInit = VhStaticInit {
        mode: "getAndAdd",
        variable: b'I',
    };

    /// A row-less static carrier handle naming `p/J27S.f` (slot 0), in a
    /// context with its own VM identity.
    pub(super) fn setup(vm: usize) -> (MockNativeContext, ClassId, ObjectRef) {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        let s = ctx
            .ensure_class_initialized("p/J27S")
            .unwrap_or(ClassId::new(0));
        ctx.set_declared_fields(
            s,
            vec![FieldMetadata {
                name: "f".to_string(),
                descriptor: "I".to_string(),
                access_flags: 0x0008,
                slot_index: 0,
                declaring_class_id: s,
                is_static: true,
            }],
        );
        let vh = ctx.alloc_object(ClassId::new(0), VH_FIELD_COUNT);
        let class = ctx.create_string("p/J27S");
        let field = ctx.create_string("f");
        ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_STATIC));
        ctx.set_field(vh, VH_CLASS, Value::Object(Some(class)));
        ctx.set_field(vh, VH_FIELD, Value::Object(Some(field)));
        (ctx, s, vh)
    }

    /// The second access is served from the memo (it no longer reads the
    /// handle's names), and a generation move discards the line.
    #[test]
    fn a_resolved_static_slot_is_memoised_until_the_generation_moves() {
        let (mut ctx, s, vh) = setup(0x27F1_0001);
        let name = ctx.create_string("f");
        // Another test thread may move the shared generation between the two
        // calls (a legal miss), so a few rounds are allowed for one hit.
        let mut served = false;
        for _ in 0..64 {
            ctx.set_field(vh, VH_FIELD, Value::Object(Some(name)));
            let first = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
            assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
            // Unname the field: only a memo hit can still answer slot 0.
            ctx.set_field(vh, VH_FIELD, Value::Object(None));
            let hit = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
            assert!(
                matches!(hit, Ok(Some((c, 0))) if c == s) || matches!(hit, Ok(None)),
                "{hit:?}"
            );
            if matches!(hit, Ok(Some(_))) {
                served = true;
                break;
            }
        }
        assert!(served, "the memo never served the resolved slot");
        vh_meta_bump_generation();
        let miss = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(matches!(miss, Ok(None)), "the stale line was served: {miss:?}");
    }

    /// `CRATONVM_VH_STATIC_SLOT_MEMO=0` fills no line; a context without a
    /// VM identity (`0`) never keeps one either.
    #[test]
    fn the_kill_switch_and_an_anonymous_vm_keep_no_line() {
        let (mut ctx, s, vh) = setup(0x27F1_0002);
        let first = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_VH_STATIC_SLOT_MEMO", Some("0"))],
            || vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT),
        );
        assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
        ctx.set_field(vh, VH_FIELD, Value::Object(None));
        let again = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(matches!(again, Ok(None)), "a line was kept with the switch off: {again:?}");

        let (mut anon, s, vh) = setup(0);
        let first = vh_static_slot_of(&mut anon, vh, None, &mut [], INIT);
        assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
        anon.set_field(vh, VH_FIELD, Value::Object(None));
        let again = vh_static_slot_of(&mut anon, vh, None, &mut [], INIT);
        assert!(matches!(again, Ok(None)), "a line was kept for VM 0: {again:?}");
    }
}

#[cfg(test)]
mod j27_vhcomp_static_note_tests {
    use super::j27_static_slot_memo_tests::setup;
    use super::*;

    const INIT: VhStaticInit = VhStaticInit {
        mode: "getAndAdd",
        variable: b'I',
    };

    fn row(read_only: bool, exact: bool, desc: &str) -> VarHandleMeta {
        VarHandleMeta {
            kind: VH_KIND_STATIC,
            class_name: "p/J27S".to_string(),
            field_name: "f".to_string(),
            field_desc: desc.to_string(),
            field_index: -1,
            class_id: 0,
            read_only,
            exact: exact.then(|| Arc::new(std::array::from_fn(|_| String::new()))),
            ref_type_class_id: 0,
            holder_class_id: 0,
        }
    }

    /// A resolved access reports its target once, on the memo fill; the hit
    /// that follows reports nothing. A row-less handle is never writable.
    #[test]
    fn the_fill_reports_the_target_and_a_hit_does_not() {
        let (mut ctx, s, vh) = setup(0x27F2_0001);
        let first = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
        let notes = ctx.static_vh_notes.borrow().clone();
        assert_eq!(notes, vec![(vh.as_ptr() as usize, s.as_u32(), 0, false, None)]);
        // A hit (when the shared generation did not move meanwhile) adds none.
        let _ = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(ctx.static_vh_notes.borrow().len() <= 2);
    }

    /// Only a read-write, non-exact `int` / `long` row is writable with a kind.
    #[test]
    fn the_row_decides_writable_and_kind() {
        let (ctx, s, vh) = setup(0x27F2_0002);
        for (meta, want) in [
            (row(false, false, "I"), (true, Some(b'I'))),
            (row(false, false, "J"), (true, Some(b'J'))),
            (row(true, false, "I"), (false, Some(b'I'))),
            (row(false, true, "I"), (false, Some(b'I'))),
            (row(false, false, "S"), (true, None)),
        ] {
            ctx.static_vh_notes.borrow_mut().clear();
            vh_static_note_target(&ctx, vh, Some(&meta), s, 3);
            let notes = ctx.static_vh_notes.borrow().clone();
            assert_eq!(notes, vec![(vh.as_ptr() as usize, s.as_u32(), 3, want.0, want.1)]);
        }
    }
}

/// `VarHandle<T>s$FieldStatic{ReadOnly,ReadWrite}` and the descriptor of its
/// static method for the access `mode` of a variable of descriptor byte
/// `variable` (JDK 25: the read modes are declared in `ReadOnly`, every other
/// mode in `ReadWrite`; a reference variable is the `References` family's
/// `Object`). `None` for a mode or variable it does not know.
pub(crate) fn vh_static_mode_method(mode: &str, variable: u8) -> Option<(&'static str, &'static str)> {
    vh_field_mode_method(mode, variable, false)
}

/// [`vh_static_mode_method`] for an INSTANCE field's handle (`instance`):
/// `VarHandle<T>s$FieldInstance{ReadOnly,ReadWrite}`, whose methods take the
/// holder as an `Object` after the `VarHandle`. j27 wave 9, lane W9EXC.
pub(crate) fn vh_field_mode_method(
    mode: &str,
    variable: u8,
    instance: bool,
) -> Option<(&'static str, &'static str)> {
    macro_rules! family {
        ($t:literal, $v:literal) => {
            if instance {
                (
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldInstanceReadOnly"),
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldInstanceReadWrite"),
                    [
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, ")V"),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, $v, ")Z"),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, $v, ")", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;", $v, ")", $v),
                    ],
                )
            } else {
                (
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldStaticReadOnly"),
                    concat!("java/lang/invoke/VarHandle", $t, "$FieldStaticReadWrite"),
                    [
                        concat!("(Ljava/lang/invoke/VarHandle;)", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, ")V"),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, $v, ")Z"),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, $v, ")", $v),
                        concat!("(Ljava/lang/invoke/VarHandle;", $v, ")", $v),
                    ],
                )
            }
        };
    }
    let (read_only, read_write, descriptors) = match variable {
        b'Z' => family!("Booleans", "Z"),
        b'B' => family!("Bytes", "B"),
        b'S' => family!("Shorts", "S"),
        b'C' => family!("Chars", "C"),
        b'I' => family!("Ints", "I"),
        b'J' => family!("Longs", "J"),
        b'F' => family!("Floats", "F"),
        b'D' => family!("Doubles", "D"),
        b'L' | b'[' => family!("References", "Ljava/lang/Object;"),
        _ => return None,
    };
    let shape = match mode {
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => 0,
        "set" | "setVolatile" | "setOpaque" | "setRelease" => 1,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => 2,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => 3,
        m if m.starts_with("getAnd") => 4,
        _ => return None,
    };
    Some((
        if shape == 0 { read_only } else { read_write },
        descriptors[shape],
    ))
}

/// The served-raise chain of a static VarHandle's holder initialization
/// (JDK 25.0.3, `L4W59HandleClassInitFrames` rows `vh-static-*`): the caller's
/// `VarHandle.<mode>`, unlisted, then `VarHandle<T>s$FieldStatic*.<mode>` ->
/// `LazyInitializingVarHandle.target` -> `ensureInitialized` -> `initialize`
/// -> `Unsafe.ensureClassInitialized` -> `ensureClassInitialized0`. `None`
/// when the access is not known. Interpreter round i2 wave 59, lane L4.
pub(crate) fn vh_static_init_chain(site: VhStaticInit) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const LAZY: &str = "java/lang/invoke/LazyInitializingVarHandle";
    const TARGET: &str = "()Ljava/lang/invoke/VarHandle;";
    const VOID: &str = "()V";
    let (class, descriptor) = vh_static_mode_method(site.mode, site.variable)?;
    let last = |name: &'static str, descriptor: &'static str| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    let mut hops = vec![
        ServedRaiseHop {
            class: "java/lang/invoke/VarHandle",
            name: site.mode,
            descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
            site: ServedRaiseHopSite::Hidden,
        },
        ServedRaiseHop {
            class,
            name: site.mode,
            descriptor,
            site: last("target", TARGET),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "target",
            descriptor: TARGET,
            site: last("ensureInitialized", VOID),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "ensureInitialized",
            descriptor: VOID,
            site: last("initialize", VOID),
        },
        ServedRaiseHop {
            class: LAZY,
            name: "initialize",
            descriptor: VOID,
            site: last("ensureClassInitialized", "(Ljava/lang/Class;)V"),
        },
    ];
    hops.extend(crate::lang_class::class_init_chain_tail(
        "jdk/internal/misc/Unsafe",
    ));
    Some(ServedRaiseChain {
        throwable_class: "java/lang/ExceptionInInitializerError",
        hops,
    })
}

/// The served-raise chain of an instance-field VarHandle access with a null
/// holder (JDK 25.0.3, `J27NpeHiddenRows` rows `vh-get-null-holder` /
/// `vh-set-null-holder`): the caller's `VarHandle.<mode>`, unlisted (with the
/// hidden `VarHandleGuards.guard_*`), then `VarHandle<T>s$FieldInstance*.<mode>`
/// at its `Objects.requireNonNull(holder)` -> `Objects.requireNonNull`, whose
/// frame is the trace's top. `None` when the access is not known. j27 wave 9,
/// lane W9EXC.
pub(crate) fn vh_null_holder_chain(
    mode: &str,
    variable: u8,
) -> Option<cratonvm_native_api::ServedRaiseChain> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const REQUIRE_NON_NULL: &str = "requireNonNull";
    const OBJECT_TO_OBJECT: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let mode = vh_access_mode_name(mode)?;
    let (class, descriptor) = vh_field_mode_method(mode, variable, true)?;
    Some(ServedRaiseChain {
        throwable_class: "java/lang/NullPointerException",
        hops: vec![
            ServedRaiseHop {
                class: "java/lang/invoke/VarHandle",
                name: mode,
                descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
                site: ServedRaiseHopSite::Hidden,
            },
            ServedRaiseHop {
                class,
                name: mode,
                descriptor,
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
    })
}

/// `mode` as the `'static` name of one of `VarHandle.AccessMode`'s 31 methods.
fn vh_access_mode_name(mode: &str) -> Option<&'static str> {
    const MODES: [&str; 31] = [
        "get", "set", "getVolatile", "setVolatile", "getAcquire", "setRelease", "getOpaque",
        "setOpaque", "compareAndSet", "compareAndExchange", "compareAndExchangeAcquire",
        "compareAndExchangeRelease", "weakCompareAndSetPlain", "weakCompareAndSet",
        "weakCompareAndSetAcquire", "weakCompareAndSetRelease", "getAndSet", "getAndSetAcquire",
        "getAndSetRelease", "getAndAdd", "getAndAddAcquire", "getAndAddRelease",
        "getAndBitwiseOr", "getAndBitwiseOrRelease", "getAndBitwiseOrAcquire",
        "getAndBitwiseAnd", "getAndBitwiseAndRelease", "getAndBitwiseAndAcquire",
        "getAndBitwiseXor", "getAndBitwiseXorRelease", "getAndBitwiseXorAcquire",
    ];
    MODES.iter().copied().find(|&m| m == mode)
}

/// Re-read `live` through the pins [`pin_mh_args`] took, then release them.
pub(crate) fn vh_refresh_live(
    ctx: &mut dyn NativeContext,
    base: usize,
    handles: &[usize],
    live: &mut [Value],
) {
    for (v, h) in live.iter_mut().zip(handles.iter()) {
        *v = read_pinned_mh_arg(&*ctx, *h, *v);
    }
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
}

/// Witness comparison used by VarHandle `compareAndSet` / `compareAndExchange`:
/// objects by identity (pointer), primitives by bit pattern.
pub(crate) fn vh_values_match(current: &Value, expected: &Value) -> bool {
    match (current, expected) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        (Value::Object(a), Value::Object(b)) => match (a, b) {
            (Some(ra), Some(rb)) => ra.as_ptr() == rb.as_ptr(),
            (None, None) => true,
            _ => false,
        },
        _ => false,
    }
}

/// LOW-finding fix (VarHandle RMW atomicity): run `body` while holding the
/// monitor of `cid`'s class mirror so that read-modify-write VarHandle
/// operations on a *static* field are linearizable w.r.t. each other.
///
/// Instance- and array-element RMWs use the VM's `compare_and_swap_field`
/// (a real per-object CAS lock), but the trait exposes no CAS primitive for
/// static slots, so we serialize them on the per-class mirror object — a
/// stable, unique lock shared by every VarHandle targeting that class's
/// statics (`get_class_mirror` returns the same `Class` object each call).
/// JIT round 21 (lane staticcas): the trait now has one
/// (`compare_and_exchange_static_field`, a hardware CAS), and every caller's
/// body uses it, so the monitor no longer carries the atomicity (it never
/// excluded `putstatic` or the `Unsafe` natives); dropping it is proposal
/// STATICCAS21-1. The monitor is always released (the closure here only does infallible
/// `Value` get/set, so it cannot unwind, but we keep enter/exit balanced).
///
/// `monitor_enter` can block, and a blocked thread is a GC point: the mirror
/// and the caller's operands (`live`: the witness, the value to store) are
/// pinned across it and `body` receives the current operands. The mirror used
/// to be handed to `monitor_exit` and the operands stored from their
/// pre-block addresses (wave 17).
///
/// The operands are pinned BEFORE the mirror is fetched: `get_class_mirror`
/// mints a class's mirror on first use (`get_or_create_class_mirror`), and
/// that allocation is a GC point too. The declaring class of an INHERITED
/// static (`vh_static_slot`'s wave-16 arm) is the likeliest class never to
/// have had one. The operands used to be pinned only after it, from
/// addresses that allocation could already have moved (gc-common w33-b).
pub(crate) fn vh_with_static_lock<R>(
    ctx: &mut dyn NativeContext,
    cid: ClassId,
    live: &[Value],
    body: impl FnOnce(&mut dyn NativeContext, &[Value]) -> R,
) -> R {
    let (live_base, handles) = pin_mh_args(ctx, live);
    // JIT round 23 wave 2, lane L6 (`CRATONVM_STATIC_RMW_VM_LOCK`, default
    // ON): a VM-private lock per class instead of the mirror's Java monitor,
    // which `synchronized (Foo.class)` shares and thread dumps show; HotSpot's
    // static RMWs never touch it. The operands stay pinned across the wait
    // (a blocking region may see a moving GC) and are re-read under the lock.
    if let Some(guard) = crate::lang_class::static_rmw_lock(ctx, cid) {
        let fresh = read_pinned_mh_args(&*ctx, &handles, live);
        let r = body(&mut *ctx, &fresh);
        drop(guard);
        if live_base != usize::MAX {
            ctx.unpin_native_roots(live_base);
        }
        return r;
    }
    let lock = ctx.get_class_mirror(cid);
    let lock_pin = ctx.pin_native_root(lock);
    ctx.monitor_enter(lock);
    let lock = ctx.read_native_pin(lock_pin, lock);
    let fresh = read_pinned_mh_args(&*ctx, &handles, live);
    // Reborrow so `ctx` is still usable for `monitor_exit` after the closure
    // (a bare `body(ctx)` would move the `&mut dyn` reference).
    let r = body(&mut *ctx, &fresh);
    ctx.monitor_exit(lock);
    // Every read is done: release the OLDEST pin of this group once (the
    // first operand's when one was pinned, else the mirror's), which truncates
    // every newer one. `live_base` is `usize::MAX` when no operand was pinned.
    ctx.unpin_native_roots(live_base.min(lock_pin));
    r
}

/// Read class+field names for a STATIC VarHandle: prefer the meta side table,
/// fall back to the synthetic VH_CLASS / VH_FIELD string slots.
/// The class a static `VarHandle` was minted against, when strict mode recorded it
/// (`alloc_static_var_handle`); `None` means resolve the name, as before.
pub(crate) fn vh_static_owner(meta: Option<&VarHandleMeta>) -> Option<ClassId> {
    meta.filter(|m| m.class_id != 0)
        .map(|m| ClassId::new(m.class_id))
}

pub(crate) fn vh_static_class_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
) -> (String, String) {
    match meta {
        Some(m) => (m.class_name.clone(), m.field_name.clone()),
        None => (
            vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
            vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
        ),
    }
}

/// The slot of an INSTANCE `VarHandle`'s field, resolved by name for a handle
/// minted before its field could be resolved (`field_index == -1`): in the
/// field's holder ([`VarHandleMeta::holder_class_id`]) by class id, else in
/// `class_name` by name, as before (wave 46, lane L4).
pub(crate) fn vh_instance_field_by_name(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    meta: Option<&VarHandleMeta>,
) -> Option<usize> {
    match meta {
        Some(m) if m.holder_class_id != 0 => {
            ctx.resolve_field_index_by_class_id(ClassId::new(m.holder_class_id), &m.field_name)
        }
        Some(m) => ctx.resolve_field_index(&m.class_name, &m.field_name),
        None => {
            let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
            let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
            ctx.resolve_field_index(&class, &field)
        }
    }
}

/// Read VarHandle metadata helpers.
pub(crate) fn vh_read_string(ctx: &mut dyn NativeContext, vh: ObjectRef, field: usize) -> Option<String> {
    match ctx.get_field(vh, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Byte width of a byte-array-view element descriptor.
pub(crate) fn byte_view_width(elem: u8) -> usize {
    match elem {
        b'J' | b'D' => 8,
        b'I' | b'F' => 4,
        b'S' | b'C' => 2,
        _ => 1,
    }
}

pub(crate) fn byte_view_desc(elem: u8) -> &'static str {
    match elem {
        b'J' => DESC_LONG,
        b'D' => DESC_DOUBLE,
        b'I' => DESC_INT,
        b'F' => DESC_FLOAT,
        b'S' => DESC_SHORT,
        b'C' => DESC_CHAR,
        b'B' => DESC_BYTE,
        b'Z' => DESC_BOOLEAN,
        _ => DESC_BYTE,
    }
}

pub(crate) fn array_element_desc(ctx: &dyn NativeContext, arr: ObjectRef) -> &'static str {
    match ctx.heap_element_type_of(arr) {
        ArrayElementType::Boolean => DESC_BOOLEAN,
        ArrayElementType::Byte => DESC_BYTE,
        ArrayElementType::Char => DESC_CHAR,
        ArrayElementType::Short => DESC_SHORT,
        ArrayElementType::Int => DESC_INT,
        ArrayElementType::Long => DESC_LONG,
        ArrayElementType::Float => DESC_FLOAT,
        ArrayElementType::Double => DESC_DOUBLE,
        ArrayElementType::Reference => DESC_REF,
    }
}

pub(crate) fn vh_access_type_ordinal(ctx: &mut dyn NativeContext, access_type: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field(access_type, 1) {
        return v;
    }
    if let Value::Int(v) = ctx.get_field_by_name(access_type, "ordinal") {
        return v;
    }
    match ctx.invoke_virtual(access_type, "ordinal", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => 0,
    }
}

pub(crate) fn vh_width_descriptor(width: i32) -> &'static str {
    match width {
        1 => DESC_BYTE,
        2 => DESC_SHORT,
        4 => DESC_INT,
        8 => DESC_LONG,
        _ => DESC_OBJECT,
    }
}

pub(crate) fn vh_sanitize_value_descriptor(desc: &str) -> Cow<'static, str> {
    if desc == DESC_REF || desc.is_empty() {
        Cow::Borrowed(DESC_OBJECT)
    } else if desc.len() == 1
        || desc.starts_with('[')
        || (desc.starts_with('L') && desc.ends_with(';'))
    {
        Cow::Owned(desc.to_string())
    } else {
        Cow::Borrowed(DESC_OBJECT)
    }
}

pub(crate) fn vh_access_mode_descriptor(access_type: i32, coords: &[String], value_desc: &str) -> String {
    let value_desc = vh_sanitize_value_descriptor(value_desc);
    let mut params = String::new();
    for coord in coords {
        params.push_str(coord);
    }
    let mut desc = String::from("(");
    match access_type {
        1 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(DESC_VOID);
        }
        2 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(DESC_BOOLEAN);
        }
        3 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(&value_desc);
        }
        4 => {
            desc.push_str(&params);
            desc.push_str(&value_desc);
            desc.push(')');
            desc.push_str(&value_desc);
        }
        _ => {
            desc.push_str(&params);
            desc.push(')');
            desc.push_str(&value_desc);
        }
    }
    desc
}

pub(crate) fn p67_memory_segment_varhandle_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    // The shape table names the CARRIER, which a width cannot: `JAVA_INT` and
    // `JAVA_FLOAT` are both four bytes and must type as `int`/`float`. It also
    // carries the index coordinate a sequence-element path added.
    if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
        return Some(vh_access_mode_descriptor(
            access_type,
            &layout_vh_coordinates(shape),
            layout_vh_carrier_desc(shape.carrier),
        ));
    }
    let slot_width = if ctx.get_field(vh, 2).as_int() == Some(3) {
        ctx.get_field(vh, 1).as_int()
    } else {
        None
    };
    if let Some(width) = slot_width {
        let coords = vec![
            "Ljava/lang/foreign/MemorySegment;".to_string(),
            DESC_LONG.to_string(),
        ];
        return Some(vh_access_mode_descriptor(
            access_type,
            &coords,
            vh_width_descriptor(width),
        ));
    }
    if is_segment_var_handle(ctx, vh) {
        // Round 13 wave 1 (lane mhffm): the form's value type and coordinate
        // count, as `SegmentVarHandle.accessModeTypeUncached` reads them
        // (`CRATONVM_FFM_SEGMENT_VH_FORM`); `enclosing` is a group layout for
        // a path handle.
        let form = if segment_vh_checks() && segment_vh_form_enabled() {
            segment_vh_form_shape(ctx, vh)
        } else {
            None
        };
        let (value_desc, variable) = match form {
            Some(shape) => (seg_shape_desc(shape), segment_vh_form_is_variable(ctx, vh)),
            None => (
                segment_vh_fields(ctx, vh)
                    .map(|(layout, _, _)| seg_shape_desc(segment_layout_shape(ctx, layout)))
                    .unwrap_or(DESC_LONG),
                false,
            ),
        };
        let mut coords = vec![
            "Ljava/lang/foreign/MemorySegment;".to_string(),
            DESC_LONG.to_string(),
        ];
        if variable {
            coords.push(DESC_LONG.to_string());
        }
        return Some(vh_access_mode_descriptor(access_type, &coords, value_desc));
    }
    None
}

/// What a CratonVM VarHandle addresses, as JVM descriptors: the variable type
/// and the coordinate types, plus whether the coordinates are the REAL ones.
///
/// Derived from the WP4.2 side table, falling back to the synthetic slots. One
/// function so `accessModeType` and `varType`/`coordinateTypes` can never
/// disagree about what a handle points at.
///
/// `coords_exact == false` means a coordinate had to be erased to
/// `java/lang/Object` because the metadata does not record the real class.
/// `accessModeType` tolerates that — an erased `MethodType` still dispatches —
/// but `coordinateTypes()` must NOT, because there its answer would be a
/// fabricated `Class` presented as fact.
///
/// `VH_KIND_ARRAY` used to be the standing example: its producer
/// (`MethodHandles.arrayElementVarHandle`) was registered only in
/// `phases_late/reflect_invoke.rs`, which does not run in real-JDK mode, so an
/// array handle reached here with no side-table entry and the leading
/// coordinate had to be erased. That factory is now registered on the live
/// path in this file and records the array class, so the erasure fires only
/// for a handle CratonVM did not mint — where refusing is the right answer.
/// Read a `Class`-typed field off a REAL JDK object and return its descriptor.
///
/// The `Object(Some(_))` match is what separates "the field is there and holds
/// a mirror" from "this is not the class I thought it was".
///
/// **Corrected 2026-08-07.** This used to say `get_field_by_name` answers
/// `Int(0)` for a field that does not exist. It does not. PRODUCTION
/// (`vm/src/vm/vm_exec.rs::get_field_by_name`) resolves the name in the
/// hierarchy and returns **`Value::Object(None)`** when it cannot — the
/// `Int(0)` is `MockNativeContext`'s (`native-builtins/src/test_utils.rs`) and
/// nobody else's; `native-api/src/test_mock.rs` agrees with production. The
/// code here is right either way (both `Object(None)` and `Int(0)` fall to the
/// `_` arm), but the reasoning was not, and the sharper hazard the old note
/// hid is that an absent field is INDISTINGUISHABLE from a real null: a
/// `matches!(…, Value::Object(_))` layout discriminator matches BOTH. Never
/// write one.
pub(crate) fn vh_real_class_field_desc(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Option<String> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(mirror)) => Some(mirror_to_descriptor(ctx, mirror).into_owned()),
        _ => None,
    }
}

/// `(element descriptor, coordinate descriptors)` for a REAL-JDK array-element
/// VarHandle — the object `MethodHandles.arrayElementVarHandle` hands back when
/// its own bytecode runs, which is what happens in `--real-jdk`/`--jdk-only`
/// (this file registers no native for that factory; see the note next to
/// `byteArrayViewVarHandle`). `None` for anything else.
///
/// Such a handle carries no WP4.2 side-table entry, so without this the
/// describe-yourself accessors refuse — and they are FORCED to run by the
/// `check_override` entry in `vm/src/vm/vm_exec.rs`, which pins
/// `VarHandle.varType`/`coordinateTypes` to the registry for every receiver.
/// The refusal would therefore replace an answer the real bytecode could have
/// given, which is a worse outcome than the gap it was written for.
///
/// The real object knows its own type; the only question is where. Measured on
/// JDK 25, `MethodHandles.arrayElementVarHandle(T[].class).getClass()`:
///
/// ```text
/// int[]      VarHandleInts$Array       varType int      coords [class [I, int]
/// long[]     VarHandleLongs$Array      varType long     coords [class [J, int]
/// byte[]     VarHandleBytes$Array      varType byte     coords [class [B, int]
/// short[]    VarHandleShorts$Array     varType short    coords [class [S, int]
/// char[]     VarHandleChars$Array      varType char     coords [class [C, int]
/// float[]    VarHandleFloats$Array     varType float    coords [class [F, int]
/// double[]   VarHandleDoubles$Array    varType double   coords [class [D, int]
/// boolean[]  VarHandleBooleans$Array   varType boolean  coords [class [Z, int]
/// String[]   VarHandleReferences$Array varType String   coords [class [Ljava.lang.String;, int]
/// int[][]    VarHandleReferences$Array varType [I       coords [class [[I, int]
/// ```
///
/// Two facts drive the shape. `varType()` is the COMPONENT type, not the array
/// type; and there are TWO coordinates, `{arrayClass, int}`, where a field
/// handle has one. The primitive families encode the element type in the class
/// NAME and declare only `abase`/`ashift`; `References$Array` is the sole
/// family that cannot, and it declares `arrayType`/`componentType` `Class`
/// fields instead (`javap -p`) — which is why it is the one arm that reads
/// fields rather than parsing a name.
pub(crate) fn real_array_var_handle_descriptors(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
) -> Option<(String, Vec<String>)> {
    let class_id = ctx.class_id_of_object(vh);
    let name = ctx.class_name_of_id(class_id)?;
    let family = name.strip_prefix("java/lang/invoke/VarHandle")?;
    let elem = match family {
        "Ints$Array" => DESC_INT,
        "Longs$Array" => DESC_LONG,
        "Bytes$Array" => DESC_BYTE,
        "Shorts$Array" => DESC_SHORT,
        "Chars$Array" => DESC_CHAR,
        "Floats$Array" => DESC_FLOAT,
        "Doubles$Array" => DESC_DOUBLE,
        "Booleans$Array" => DESC_BOOLEAN,
        "References$Array" => {
            // Both halves or neither: a coordinate list without a variable
            // type (or the reverse) would let one accessor answer while the
            // other invents.
            let array_desc = vh_real_class_field_desc(ctx, vh, "arrayType")?;
            let component_desc = vh_real_class_field_desc(ctx, vh, "componentType")?;
            return Some((component_desc, vec![array_desc, DESC_INT.to_string()]));
        }
        _ => return None,
    };
    Some((
        elem.to_string(),
        vec![format!("[{elem}"), DESC_INT.to_string()],
    ))
}

/// `LambdaMetafactory.metafactory`'s descriptor.
pub(crate) const LMF_METAFACTORY_DESC: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";
/// `LambdaMetafactory.altMetafactory`'s descriptor.
pub(crate) const LMF_ALT_METAFACTORY_DESC: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;";

/// A direct (reflective) `LambdaMetafactory.<method>` call under
/// `CRATONVM_LMF_REFLECTIVE_TO_JDK` (interpreter round i1 wave 49, lane L4;
/// `i48-L4-proposal-retire-the-reflective-lambdametafactory-bridge`): the
/// JDK's own factory body, past this registration, as HotSpot runs it. Its
/// checks raise the JDK's `LambdaConversionException`s (the bridge linked a
/// non-interface target, a wrong arity, and panicked the VM on a
/// non-convertible return), and it spins the JDK's hidden proxy class. The
/// `invokedynamic` route never reaches the bridge; this is the reflective
/// callers' route (log4j's `ServiceLoaderUtil`, Spring's `LambdaSafe`).
/// `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints one `[lmf-reflective]` line per
/// call (the positive control, probe `L4W49ReflectiveMetafactory`).
#[cold]
#[inline(never)]
pub(crate) fn lmf_reflective_to_jdk(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LAMBDA_DISPATCH") {
        eprintln!("[lmf-reflective] LambdaMetafactory.{method} -> JDK factory");
    }
    ctx.invoke_special_bytecode_only(
        "java/lang/invoke/LambdaMetafactory",
        method,
        descriptor,
        args,
    )
}

/// The JDK's own argument checks for the reflective bridge (switch off): the
/// `InnerClassLambdaMetafactory` constructor and `validateMetafactoryArgs()`
/// that `LambdaMetafactory.metafactory` runs before `buildCallSite()`, which
/// alone spins the proxy class. A `LambdaConversionException` they throw is
/// the call's (`java.lang.String is not an interface`, a wrong arity, a
/// non-adaptable type); any other failure of theirs (a handle of this VM's
/// model that `revealDirect` cannot crack) is not, and the bridge links as it
/// did. Interpreter round i1 wave 50, lane L4 (`L4W49ReflectiveMetafactory`
/// rows `not-interface` and `arity` linked a `ConstantCallSite`).
///
/// `args` are the factory's (`altMetafactory`'s `samType`, `implementation`,
/// `instantiatedType` and flags come out of its `Object[]`; its markers and
/// bridges are not passed: their checks are the JDK's `FLAG_MARKERS` loop
/// over classes the caller already holds). Answers a copy of `args` read
/// back after the checks, which may collect: the slice itself is stale then.
#[cold]
#[inline(never)]
pub(crate) fn lmf_bridge_jdk_checks(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    alt: bool,
) -> Result<Vec<Value>, MethodCallFailed> {
    let obj = |v: Option<&Value>| match v {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let mut objs = [None; 6];
    for (i, slot) in objs.iter_mut().enumerate().take(3) {
        *slot = obj(args.get(i));
    }
    let mut serializable = false;
    if alt {
        if let Some(arr) = obj(args.get(3)) {
            let len = ctx.array_length(arr);
            for k in 0..3.min(len) {
                objs[3 + k] = match ctx.get_array_element(arr, k) {
                    Value::Object(o) => o,
                    _ => None,
                };
            }
            if len > 3 {
                let flags = match ctx.get_array_element(arr, 3) {
                    Value::Int(i) => i,
                    Value::Object(Some(b)) => {
                        ctx.get_field_by_name(b, "value").as_int().unwrap_or(0)
                    }
                    _ => 0,
                };
                serializable = flags & 0x1 != 0;
            }
        }
    } else {
        for (i, slot) in objs.iter_mut().enumerate().skip(3) {
            *slot = obj(args.get(i));
        }
    }
    // The JDK's own null checks are `Objects.requireNonNull` in the factory;
    // the bridge keeps its own answer for a null argument.
    if objs.iter().any(Option::is_none) {
        return Ok(args.to_vec());
    }
    let arg_pins: Vec<Option<usize>> = args
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        })
        .collect();
    let pins: Vec<usize> = objs
        .iter()
        .flatten()
        .map(|o| ctx.pin_native_root(*o))
        .collect();
    let base = arg_pins.iter().flatten().next().copied().unwrap_or(pins[0]);
    let result = lmf_bridge_jdk_checks_run(ctx, objs, &pins, serializable);
    let fresh: Vec<Value> = args
        .iter()
        .zip(arg_pins.iter())
        .map(|(v, pin)| match (v, pin) {
            (Value::Object(Some(o)), Some(p)) => Value::Object(Some(ctx.read_native_pin(*p, *o))),
            _ => *v,
        })
        .collect();
    ctx.unpin_native_roots(base);
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let is_lce = ctx
                .class_name_of_id(ctx.class_id_of_object(exc))
                .is_some_and(|n| n == "java/lang/invoke/LambdaConversionException");
            if is_lce {
                Err(MethodCallFailed::ExceptionThrown(exc))
            } else {
                Ok(fresh)
            }
        }
        _ => Ok(fresh),
    }
}

/// The body of [`lmf_bridge_jdk_checks`]: `objs` (`[caller, invokedName,
/// factoryType, samType, implementation, instantiatedType]`) are pinned at
/// `pins`.
pub(crate) fn lmf_bridge_jdk_checks_run(
    ctx: &mut dyn NativeContext,
    objs: [Option<ObjectRef>; 6],
    pins: &[usize],
    serializable: bool,
) -> Result<(), MethodCallFailed> {
    const ICLMF: &str = "java/lang/invoke/InnerClassLambdaMetafactory";
    const CTOR: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/invoke/MethodType;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;Z[Ljava/lang/Class;[Ljava/lang/invoke/MethodType;)V";
    {
        let (Some(class_cid), Some(mt_cid)) = (
            ctx.class_id_by_name("java/lang/Class"),
            ctx.class_id_by_name("java/lang/invoke/MethodType"),
        ) else {
            return Ok(());
        };
        let no_classes = ctx.new_ref_array(class_cid, 0);
        let no_classes_pin = ctx.pin_native_root(no_classes);
        let no_types = ctx.new_ref_array(mt_cid, 0);
        let no_classes = ctx.read_native_pin(no_classes_pin, no_classes);
        let o: Vec<ObjectRef> = objs
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, o)| ctx.read_native_pin(pins[i], *o))
            .collect();
        // Constructor order: caller, factoryType, name, samType, impl, inst.
        let mf = match ctx.new_object_initialized(
            ICLMF,
            CTOR,
            &[
                Value::Object(Some(o[0])),
                Value::Object(Some(o[2])),
                Value::Object(Some(o[1])),
                Value::Object(Some(o[3])),
                Value::Object(Some(o[4])),
                Value::Object(Some(o[5])),
                Value::Int(serializable as i32),
                Value::Object(Some(no_classes)),
                Value::Object(Some(no_types)),
            ],
        )? {
            Some(Value::Object(Some(mf))) => mf,
            _ => return Ok(()),
        };
        ctx.invoke_virtual(mf, "validateMetafactoryArgs", "()V", &[])?;
        Ok(())
    }
}

/// `VarHandle.withInvokeExactBehavior()` (`exact == true`) and
/// `withInvokeBehavior()` (`false`), every mode since wave 49 (interpreter
/// round i1 wave 37, lane L4; item 3 of
/// `interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`): the
/// handle itself when it already has that behaviour, otherwise a NEW handle
/// over the same variable with it, as the JDK's builders answer.
///
/// * A handle CratonVM minted: the copy has the plain handle's side-table row
///   with [`VarHandleMeta::exact`] set (the five access mode descriptors,
///   computed here once) or cleared, and its real `exact` field set, which is
///   what `hasInvokeExactBehavior()` (JDK bytecode) reads.
/// * The JDK's own array-element handles (`arrayElementVarHandle` runs JDK
///   bytecode, so they have no row): their concrete builder, which this
///   registration stands in front of.
///
/// Minting an exact handle publishes the slot of `VarHandle.exact` to the VM
/// (`note_exact_var_handle_minted`), which turns the doors' check on
/// ([`varhandle_exact_call_site_refusal`]) early (the doors also resolve
/// it themselves since interpreter wave 49). `this` unchanged for any other
/// handle.
///
/// `--compatible` (JIT round 15 wave 1, lane vh;
/// `r15w1-vh-compatible-mode-ignores-invoke-exact-behavior-FIXED-20260930.md`): the
/// same answers, so `hasInvokeExactBehavior()` and the call-site refusal
/// match HotSpot there too. Safe to open the doors' gate in that mode: a
/// real-JDK `VarHandle` has the real layout in both policies, and the gate is
/// resolved from a real `exact` field or not opened at all (a synthetic-layout
/// copy has none), and a receiver the gate misreads is only asked
/// [`varhandle_exact_call_site_refusal`], which accepts every non-exact
/// handle. `CRATONVM_VH_EXACT_COMPATIBLE=0` restores `this` in that mode.
/// Interpreter round i1 wave 49 (lane L4) made the same change independently
/// (`L4W48VarHandleExactBehavior` row `field-cas-boxed`: a field handle's
/// `withInvokeExactBehavior()` converted as a plain one in `--compatible`).
pub(crate) fn varhandle_with_exactness(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    exact: bool,
) -> MethodCallResult {
    let this = args.first().copied().unwrap_or(Value::Object(None));
    let Value::Object(Some(vh)) = this else {
        return Ok(Some(this));
    };
    // Every mode by default (JIT round 15 wave 1, lane vh; interpreter wave
    // 49, lane L4): `--compatible` answered `this`, so a field handle's
    // `withInvokeExactBehavior()` converted as a plain one
    // (`L4W48VarHandleExactBehavior` row `field-cas-boxed`).
    // `CRATONVM_VH_EXACT_COMPATIBLE=0` restores that answer in `--compatible`.
    if !vh_exact_judged_in_this_mode(ctx) {
        return Ok(Some(this));
    }
    let Some(meta) = vh_meta_get(ctx, vh) else {
        // An `IndirectVarHandle` reached through `VarHandle`'s registration:
        // its own builder (real bytecode), not `this` (round 15 wave 1, lane
        // vh; `r14w7-ffm7-indirect-var-handle-residuals` item 1).
        if vh_indirect_exact_enabled() && is_indirect_var_handle(ctx, vh) {
            let method = if exact {
                "withInvokeExactBehavior"
            } else {
                "withInvokeBehavior"
            };
            let built = ctx.invoke_virtual_bytecode_only(
                vh,
                method,
                "()Ljava/lang/invoke/VarHandle;",
                &[],
            )?;
            indirect_vh_note_exact(ctx, &built);
            return Ok(built);
        }
        // An FFM layout handle (`ValueLayout.varHandle()`): a copy over the
        // same layout shape with the other flag, which is all it carries: an
        // exact one is judged from its shape and its real `exact` field
        // (`varhandle_exact_call_site_refusal`, wave 38), and its mint opens
        // the doors' gate like any other exact handle's.
        if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
            let now_exact = matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0);
            if now_exact == exact {
                return Ok(Some(this));
            }
            let copy =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
            if !vh_has_synthetic_layout(ctx, copy) {
                let class_id = ctx.class_id_of_object(copy);
                if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") {
                    ctx.set_field(copy, slot, Value::Int(i32::from(exact)));
                    if exact {
                        ctx.note_exact_var_handle_minted(slot);
                    }
                }
            }
            register_p67_memory_segment_var_handle(ctx, copy, shape);
            return Ok(Some(Value::Object(Some(copy))));
        }
        if real_array_var_handle_descriptors(ctx, vh).is_none() {
            return Ok(Some(this));
        }
        let method = if exact {
            "withInvokeExactBehavior"
        } else {
            "withInvokeBehavior"
        };
        let built =
            ctx.invoke_virtual_bytecode_only(vh, method, "()Ljava/lang/invoke/VarHandle;", &[])?;
        if exact {
            if let Some(Value::Object(Some(handle))) = built {
                let class_id = ctx.class_id_of_object(handle);
                if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") {
                    ctx.note_exact_var_handle_minted(slot);
                }
            }
        }
        return Ok(built);
    };
    if meta.exact.is_some() == exact {
        return Ok(Some(this));
    }
    // The access mode types, from the plain handle's own description, before
    // the allocation below (`vh` is not used after it).
    let types: Option<Arc<[String; 5]>> = if exact {
        let (_kind, value_desc, coords, coords_exact) =
            vh_value_and_coordinate_descriptors(ctx, vh);
        Some(Arc::new(std::array::from_fn(|access_type| {
            if coords_exact {
                vh_access_mode_descriptor(access_type as i32, &coords, &value_desc)
            } else {
                String::new()
            }
        })))
    } else {
        None
    };
    let copy = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT)?;
    // Real layout only: on the synthetic one slot 1 is `VH_CLASS`.
    let slot = if vh_has_synthetic_layout(ctx, copy) {
        None
    } else {
        let class_id = ctx.class_id_of_object(copy);
        ctx.resolve_field_index_by_class_id(class_id, "exact")
    };
    if let Some(slot) = slot {
        ctx.set_field(copy, slot, Value::Int(i32::from(exact)));
    }
    let mut row = (*meta).clone();
    row.exact = types;
    vh_meta_put(ctx, copy, row);
    if exact {
        if let Some(slot) = slot {
            ctx.note_exact_var_handle_minted(slot);
        }
    }
    Ok(Some(Value::Object(Some(copy))))
}

/// The access type (`VarHandle.AccessType` ordinal, as
/// [`vh_access_mode_descriptor`] numbers it) of a signature-polymorphic
/// access-mode method, or `None` for any other name.
pub(crate) fn vh_access_type_of_method(name: &str) -> Option<i32> {
    Some(match name {
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => 0,
        "set" | "setVolatile" | "setOpaque" | "setRelease" => 1,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => 2,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => 3,
        _ if name.starts_with("getAndSet")
            || name.starts_with("getAndAdd")
            || name.starts_with("getAndBitwise") =>
        {
            4
        }
        _ => return None,
    })
}

/// `VarHandle.checkAccessModeThenIsDirect`'s refusal: `vh` has invoke-exact
/// behaviour and `call_site`, the descriptor the calling instruction names
/// for `method_name`, is not exactly the access mode type, so the access is
/// `WrongMethodTypeException("handle's method type (H)int but found
/// (H)long")`, raised before anything is read or written. `None` when the
/// handle is not exact, the types agree, or the handle cannot describe itself
/// exactly (every way of not knowing is an accept, as in
/// `exact_call_site_refusal`).
///
/// A handle CratonVM minted is judged by its side-table row
/// (`VarHandleMeta::exact`, precomputed: no allocation per access); the JDK's
/// own array-element handles by their real `exact` field and their class, an
/// FFM layout handle by its real `exact` field and its `p67` shape
/// (`(MemorySegment, long[, long])` and the layout's carrier; interpreter
/// round i1 wave 38, lane L4: before it every layout handle was declined, so
/// a mistyped access to an exact one converted). A layout handle on the
/// synthetic `VarHandle` layout is not judged: its slots hold the mint's
/// width, not `exact`.
///
/// For the doors that hold the REAL call-site descriptor and have already
/// seen the receiver's `exact` field set (interpreter round i1 wave 37, lane
/// L4): the interpreter's `execute_invoke_kind` and the JIT's site-cached
/// native dispatch.
pub fn varhandle_exact_call_site_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    method_name: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    let access_type = vh_access_type_of_method(method_name)?;
    let meta = vh_meta_get(ctx, vh);
    let computed: String;
    let expected: &str = match meta.as_deref() {
        Some(row) => row.exact.as_deref()?.get(access_type as usize)?.as_str(),
        None => {
            if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
                if vh_has_synthetic_layout(ctx, vh)
                    || !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0)
                {
                    return None;
                }
                let layout_type = vh_access_mode_descriptor(
                    access_type,
                    &layout_vh_coordinates(shape),
                    layout_vh_carrier_desc(shape.carrier),
                );
                return varhandle_exact_mismatch(ctx, &layout_type, call_site);
            }
            // An exact `IndirectVarHandle` (round 15 wave 1, lane vh): its
            // own `value` / `coordinates`, the fields `accessModeTypeUncached`
            // reads on HotSpot.
            if let Some(indirect_type) = indirect_vh_exact_access_descriptor(ctx, vh, access_type) {
                return varhandle_exact_mismatch(ctx, &indirect_type, call_site);
            }
            // An exact REAL `SegmentVarHandle` (what `ValueLayout.varHandle()`
            // answers since SBR round 3 retired the FFM layouts under
            // `--jdk-only`; interpreter round i2 wave 55, lane L4, host run of
            // `L4W38FfmExactLayoutHandle`): it is neither in the synthetic
            // layout table above nor an array handle, so every mistyped access
            // was admitted and converted.
            if let Some(segment_type) = segment_vh_exact_access_descriptor(ctx, vh, access_type) {
                return varhandle_exact_mismatch(ctx, &segment_type, call_site);
            }
            let (value_desc, coords) = real_array_var_handle_descriptors(ctx, vh)?;
            if !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0) {
                return None;
            }
            computed = vh_access_mode_descriptor(access_type, &coords, &value_desc);
            computed.as_str()
        }
    };
    varhandle_exact_mismatch(ctx, expected, call_site)
}

/// The access mode type of an exact real `java.lang.invoke.SegmentVarHandle`
/// for `access_type`, as its `accessModeTypeUncached` builds it: coordinates
/// `(MemorySegment, long)` (plus a `long` when the form's get type takes three
/// parameters, a variable offset), the value the form's erased get type
/// returns. `None` for any other handle, or one without invoke-exact
/// behaviour. Reads only.
pub(crate) fn segment_vh_exact_access_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        != Some("java/lang/invoke/SegmentVarHandle")
    {
        return None;
    }
    if !matches!(
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "exact"),
        Value::Int(v) if v != 0
    ) {
        return None;
    }
    let (coords, value) = segment_vh_shape(ctx, vh)?;
    Some(vh_access_mode_descriptor(access_type, &coords, &value))
}

/// A real `java.lang.invoke.SegmentVarHandle`'s coordinate descriptors and
/// variable descriptor, from its form's erased get type (see
/// [`segment_vh_exact_access_descriptor`]); `None` for any other handle.
/// Reads only. Wave 57 (lane L4) split it out for the return-direction check
/// ([`vh_return_site_check`]) and its refusal record ([`vh_astype_refused`]).
pub(crate) fn segment_vh_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(Vec<String>, String)> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        != Some("java/lang/invoke/SegmentVarHandle")
    {
        return None;
    }
    let get_desc = match real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform") {
        Value::Object(Some(vform)) => {
            match real_object_field(ctx, vform, "java/lang/invoke/VarForm", "methodType_table") {
                Value::Object(Some(table))
                    if ctx.object_is_array(table) && ctx.array_length(table) > 0 =>
                {
                    match ctx.get_array_element(table, 0) {
                        Value::Object(Some(get_type)) => {
                            descriptor_from_method_type(&*ctx, get_type)
                        }
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    let parsed = split_descriptor_params(&get_desc);
    // Some host-JDK VarForm layouts expose the implementation class but not
    // the lazily populated MethodType table to the VM's field reader. The
    // concrete implementation class still determines the value carrier, and
    // SegmentVarHandle access modes have two or three coordinates. Keep the
    // MethodType path authoritative when it is available.
    let (value, variable) = if let Some((params, ret)) = parsed {
        (ret, params.len() == 3)
    } else {
        let shape = segment_vh_form_shape(ctx, vh)?;
        let value = match segment_vh_fields(ctx, vh) {
            Some((enclosing, _, _))
                if ctx
                    .class_name_of_id(ctx.class_id_of_object(enclosing))
                    .is_some_and(|name| name.contains("OfAddress")) =>
            {
                "Ljava/lang/foreign/MemorySegment;".to_string()
            }
            _ => seg_shape_desc(shape).to_string(),
        };
        (value, segment_vh_form_is_variable(ctx, vh))
    };
    let mut coords = vec![
        "Ljava/lang/foreign/MemorySegment;".to_string(),
        DESC_LONG.to_string(),
    ];
    if variable {
        coords.push(DESC_LONG.to_string());
    }
    Some((coords, value))
}

/// The real JDK `SegmentVarHandle`'s variable descriptor and coordinates,
/// decoded from its `VarForm`. Compatible mode can return this concrete JDK
/// handle directly from `ValueLayout.varHandle()`, so unlike a CratonVM-minted
/// layout handle it has no row in `P67_MEMORY_SEGMENT_VH_TABLE`.
pub(crate) fn real_segment_vh_descriptors(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> Option<(String, Vec<String>)> {
    let (coordinates, variable) = segment_vh_shape(ctx, vh)?;
    Some((variable, coordinates))
}

/// `invokeExact`'s `WrongMethodTypeException` -- `handle's method type
/// {handle_type} but found {call_site}`, each type as `MethodType.toString()`
/// shows it -- when `call_site` is not exactly `handle_type`; `None` when they
/// agree. JIT r23 w4 review (lane L2): the VM's interpreted lambda dispatch
/// raises it for a `REF_invokeSpecial` handle forwarder whose `invokeExact`
/// names the member's declaring class while the handle's receiver is the
/// caller (JDK-8172817).
pub fn mh_invoke_exact_type_refusal(
    ctx: &mut dyn NativeContext,
    handle_type: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    varhandle_exact_mismatch(ctx, handle_type, call_site)
}

/// The `WrongMethodTypeException` of [`varhandle_exact_call_site_refusal`]
/// when `call_site` is not exactly `expected` (an empty `expected` is an
/// unknown type and accepts).
pub(crate) fn varhandle_exact_mismatch(
    ctx: &mut dyn NativeContext,
    expected: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    if expected.is_empty() || expected == call_site {
        return None;
    }
    let shown_expected = method_type_display_in(&*ctx, expected)?;
    let shown_site = method_type_display_in(&*ctx, call_site)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/invoke/WrongMethodTypeException",
        &format!("handle's method type {shown_expected} but found {shown_site}"),
    ))
}

/// `(Ljava/lang/invoke/VarHandle;` + `desc`'s parameters + `)R`: an access
/// mode invoker's type for the access type `desc`.
pub(crate) fn vh_invoker_descriptor(desc: &str) -> Option<String> {
    let rest = desc.strip_prefix('(')?;
    Some(format!("(L{VAR_HANDLE_CLASS};{rest}"))
}

/// The refusal of a call through a `VarHandle` access mode invoker (an
/// `MH_KIND_VIRTUAL` handle on `VarHandle.<mode>`: `varHandleInvoker`,
/// `varHandleExactInvoker`, a direct handle's mode invoker) at call-site type
/// `desc` on `vh`, before anything is read or written:
///
/// * an exact invoker ([`MH_MARK_EXACT_INVOKER`] on such a handle) refuses a
///   handle whose access mode type is not `desc`, naming both with the
///   leading `VarHandle` (HotSpot's `Invokers.checkVarHandleExactType`):
///   `handle's method type (VarHandle,H)int but found (VarHandle,H)long`;
/// * any other invoker is a plain call site of type `desc`, so an
///   invoke-exact handle refuses a mismatch as `vh.get(..)` would
///   ([`varhandle_exact_call_site_refusal`]; the text has no `VarHandle`).
///
/// Interpreter round i1 wave 50, lane L4 (`L4W49VarHandleInvokers`).
#[cold]
#[inline(never)]
pub(crate) fn vh_invoker_call_refusal(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
    vh: ObjectRef,
    method_name: &str,
    desc: &str,
) -> Option<MethodCallFailed> {
    if !mh_is_exact_invoker(&*ctx, mh) {
        return varhandle_exact_call_site_refusal(ctx, vh, method_name, desc);
    }
    let access_type = vh_access_type_of_method(method_name)?;
    let own = match p67_memory_segment_varhandle_descriptor(ctx, vh, access_type) {
        Some(own) => own,
        None => {
            let (_kind, value_desc, coords, _exact) = vh_value_and_coordinate_descriptors(ctx, vh);
            vh_access_mode_descriptor(access_type, &coords, &value_desc)
        }
    };
    varhandle_exact_mismatch(
        ctx,
        &vh_invoker_descriptor(&own)?,
        &vh_invoker_descriptor(desc)?,
    )
}

/// `MethodHandles.varHandleInvoker(mode, type)` (`exact == false`) and
/// `varHandleExactInvoker(mode, type)`: a handle of type `(VarHandle,
/// type...)R` that calls access mode `mode` on its first argument at
/// call-site type `type`, as HotSpot's `Invokers.varHandleMethodInvoker`
/// does; the exact one is marked [`MH_MARK_EXACT_INVOKER`] and refuses a
/// handle of another access mode type ([`vh_invoker_call_refusal`]).
///
/// The JDK's bodies bind a `LambdaForm` invoker (`BoundMethodHandle`), whose
/// species class did not link on this VM's `java.lang.invoke` model until
/// wave 51 (interpreter round i1 wave 50, lane L4; `L4W49VarHandleInvokers`).
/// Since wave 51 it links (`L4W51BoundMethodHandleSpecies`), and the JDK
/// body was measured in place of this one (lane L4): every row of
/// `L4W49VarHandleInvokers` and `L4W50VarHandleInvokerShapes` then answered
/// a default value (`0`, `null`, `false`), because the handle it returns is
/// a real `BoundMethodHandle$Species_L` over a `LambdaForm`, which this
/// model's invoke doors cannot run. So this native stays until real
/// `LambdaForm` handles are invokable here
/// (`i50-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only`, the
/// `java.lang.invoke` rows). The handle here is the one
/// `VarHandle.toMethodHandle` binds (a direct handle's mode invoker).
pub(crate) fn mhs_var_handle_invoker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    exact: bool,
) -> MethodCallResult {
    let (Some(Value::Object(Some(mode))), Some(Value::Object(Some(mt)))) =
        (args.first(), args.get(1))
    else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let (mode, mt) = (*mode, *mt);
    let mode_name = access_mode_name(ctx, mode);
    let method = access_mode_method_name(&mode_name);
    let desc = methodtype_to_descriptor(ctx, mt).unwrap_or_default();
    if method.is_empty() || desc.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("varHandleInvoker: unreadable access mode {mode_name} or type"),
        }
        .into());
    }
    vh_invoker_model_handle(ctx, &method, &desc, exact)
}

/// [`mhs_var_handle_invoker`]'s handle for the access-mode method `method`
/// (`get`, `compareAndSet`, ...) at call-site type `desc`.
pub(crate) fn vh_invoker_model_handle(
    ctx: &mut dyn NativeContext,
    method: &str,
    desc: &str,
    exact: bool,
) -> MethodCallResult {
    let mh = alloc_method_handle(ctx, VAR_HANDLE_CLASS, method, desc, MH_KIND_VIRTUAL)?;
    let mut bits = mh_marking_bits(&*ctx, mh) | MH_MARK_VH_INVOKER;
    if exact {
        bits |= MH_MARK_EXACT_INVOKER;
    }
    mh_write_marking_bits(&*ctx, mh, bits);
    Ok(Some(Value::Object(Some(mh))))
}

// ---------------------------------------------------------------------------
// VarHandle reference returns: `asType`'s checkcast (round 15 wave 2 hand-back,
// lane vh2; VH2-3,
// `r15w2-vh2-varhandle-reference-return-not-cast-to-site-type-FIXED-20260930.md`)
// ---------------------------------------------------------------------------

/// `CRATONVM_VH_REFERENCE_RETURN_CHECKCAST` (default on, both modes): a
/// `VarHandle` access that returns its variable into a call site declaring a
/// reference type other than `Object` is cast to it, as `asType` casts on
/// HotSpot ([`varhandle_reference_return_refusal`]); the JIT does not bind a
/// thin read helper it cannot make cast. `0` restores the boxes-only rule
/// (`vm_exec::varhandle_reference_return_mismatch`, WMTE).
pub fn vh_reference_return_checkcast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_REFERENCE_RETURN_CHECKCAST")
    })
}

/// Whether `vh`'s variable has a reference type: `Some(true)` / `Some(false)`
/// for a handle that can describe its variable (an FFM layout handle's
/// carrier, a side-table row's field type, an `IndirectVarHandle`'s `value`,
/// a real array handle's component), `None` for any other. Read BEFORE the
/// access runs: the caller holds `vh` only as long as nothing moves it.
pub fn varhandle_variable_is_reference(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<bool> {
    let desc: String = if let Some(shape) = p67_segment_vh_shape(&*ctx, vh) {
        layout_vh_carrier_desc(shape.carrier).to_string()
    } else if let Some(meta) = vh_meta_get(ctx, vh) {
        meta.field_desc.clone()
    } else if is_indirect_var_handle(ctx, vh) {
        let value = indirect_vh_field(ctx, vh, "value")?;
        strict_mirror_descriptor(&*ctx, value)?
    } else {
        real_array_var_handle_descriptors(ctx, vh)?.0
    };
    match desc.as_bytes().first()? {
        b'L' | b'[' => Some(true),
        b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => Some(false),
        _ => None,
    }
}

/// The `ClassCastException` `asType` raises when a `VarHandle` access returns
/// `value` into a call site whose declared return is `site_ret` (a reference
/// descriptor other than `Ljava/lang/Object;`) and `value` is positively not
/// one, else `None`. HotSpot's cast is `Class.cast`'s, hence the wording
/// ([`reference_cast_failure`]).
///
/// * A class-typed site is judged by [`reference_arg_admitted`], which fails
///   OPEN (a stand-in, a proxy, a lambda, an unnameable class all pass).
/// * An array-typed site refuses a non-array, a primitive array of another
///   type and a reference array whose component `checkcast` refuses
///   ([`reference_array_target_admitted`], wave 60; "cannot tell" passes).
/// * A boxed primitive refused here is a CCE only when the variable is known
///   to be a reference (`variable_is_reference`, from
///   [`varhandle_variable_is_reference`]). For a primitive variable `asType`
///   refuses the conversion statically, a `WrongMethodTypeException`, which
///   `vm_exec::unbox_poly_return_checked`'s W6-1 rule raises; an undescribed
///   variable keeps that rule too.
pub fn varhandle_reference_return_refusal(
    ctx: &mut dyn NativeContext,
    value: Value,
    site_ret: &str,
    variable_is_reference: Option<bool>,
) -> Option<MethodCallFailed> {
    if !vh_reference_return_checkcast_enabled() {
        return None;
    }
    let Value::Object(Some(obj)) = value else {
        return None;
    };
    if site_ret.starts_with('[') {
        // `checkcast`'s array rule, component included (wave 60, lane L4).
        if reference_array_target_admitted(&*ctx, obj, site_ret) != Some(false) {
            return None;
        }
        return Some(reference_cast_failure(ctx, value, site_ret));
    }
    if reference_arg_admitted(ctx, value, site_ret) != Some(false) {
        return None;
    }
    let is_box = !ctx.object_is_array(obj)
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(obj))
            .is_some_and(|name| {
                matches!(
                    &*name,
                    "java/lang/Integer"
                        | "java/lang/Long"
                        | "java/lang/Short"
                        | "java/lang/Byte"
                        | "java/lang/Character"
                        | "java/lang/Boolean"
                        | "java/lang/Float"
                        | "java/lang/Double"
                )
            });
    if is_box && variable_is_reference != Some(true) {
        return None;
    }
    Some(reference_cast_failure(ctx, value, site_ret))
}

#[cfg(test)]
mod r15w2_vh2_reference_return_tests {
    //! VH2-3: what the mock can judge. Its hierarchy answers no by-name walk
    //! (`class_assignable_to_name` is `None`), so a class-typed site fails
    //! open here and the class half is the probe's (`R15Vh2RefReturnCast`).
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn cce_message(e: &MethodCallFailed) -> Option<String> {
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            )) => Some(message.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_non_array_returned_into_an_array_site_is_cast() {
        if !vh_reference_return_checkcast_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        let mut ctx = MockNativeContext::new();
        let s = match ctx.new_object("java/lang/String") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object: {other:?}"),
        };
        let refusal =
            varhandle_reference_return_refusal(&mut ctx, Value::Object(Some(s)), "[I", Some(true));
        assert_eq!(
            refusal.as_ref().and_then(cce_message).as_deref(),
            Some("Cannot cast java.lang.String to [I")
        );
        let ints = ctx.new_array(ArrayElementType::Int, 1);
        assert!(varhandle_reference_return_refusal(
            &mut ctx,
            Value::Object(Some(ints)),
            "[I",
            Some(true)
        )
        .is_none());
    }

    #[test]
    fn null_and_object_sites_are_never_refused() {
        let mut ctx = MockNativeContext::new();
        let s = match ctx.new_object("java/lang/String") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object: {other:?}"),
        };
        for site in ["[I", "Ljava/lang/Integer;", "Ljava/lang/Object;"] {
            assert!(
                varhandle_reference_return_refusal(&mut ctx, Value::Object(None), site, None)
                    .is_none()
            );
        }
        assert!(varhandle_reference_return_refusal(
            &mut ctx,
            Value::Object(Some(s)),
            "Ljava/lang/Object;",
            Some(true)
        )
        .is_none());
    }
}

pub(crate) fn vh_value_and_coordinate_descriptors(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
) -> (i32, String, Vec<String>, bool) {
    // An FFM layout handle describes itself out of the shape table — the
    // carrier the layout named and `(MemorySegment, long[, long])` — and must
    // be answered before the generic paths, whose slot reads would take this
    // receiver's endianness flag for a kind tag (that is the `kind 0` in the
    // refusal `RJdkForeign.layoutVarHandles` used to raise).
    if let Some(shape) = p67_segment_vh_shape(ctx, vh) {
        return (
            VH_KIND_MEMORY_SEGMENT_LAYOUT,
            layout_vh_carrier_desc(shape.carrier).to_string(),
            layout_vh_coordinates(shape),
            true,
        );
    }
    // Compatible mode can leave a concrete JDK `SegmentVarHandle` unwrapped
    // by our side table. Its VarForm is the authoritative carrier/coordinate
    // description, just as the registered shape is for handles CratonVM mints.
    if let Some((value_desc, coords)) = real_segment_vh_descriptors(ctx, vh) {
        return (VH_KIND_MEMORY_SEGMENT_LAYOUT, value_desc, coords, true);
    }
    let meta = vh_meta_get(ctx, vh);
    // A real-JDK array handle has no side-table entry and its slots belong to
    // the real class, so the fallbacks below would read `vform` as a kind tag.
    // Ask the object what it is instead. Checked only when the side table has
    // nothing: a handle CratonVM minted is described by the metadata CratonVM
    // recorded for it.
    if meta.is_none() {
        if let Some((value_desc, coords)) = real_array_var_handle_descriptors(ctx, vh) {
            return (VH_KIND_ARRAY, value_desc, coords, true);
        }
    }
    let kind = meta
        .as_deref()
        .map(|m| m.kind)
        .or_else(|| ctx.get_field(vh, VH_KIND).as_int())
        .unwrap_or(VH_KIND_INSTANCE);
    let value_desc = meta
        .as_deref()
        .map(|m| Cow::Owned(m.field_desc.clone()))
        .unwrap_or_else(|| vh_field_desc(ctx, vh));
    let mut coords_exact = true;
    let coords = match kind {
        VH_KIND_STATIC => Vec::new(),
        VH_KIND_ARRAY => {
            // `class_name` is the ARRAY class's internal name (`[I`,
            // `[Ljava/lang/String;`), written by this file's
            // `arrayElementVarHandle`. Absent it, the leading coordinate would
            // be an erasure rather than the `int[]`/`String[]` the JDK
            // reports, and `coords_exact` goes false so `coordinateTypes()`
            // refuses instead of naming `Object`.
            let arr = meta
                .as_deref()
                .map(|m| m.class_name.as_str())
                .filter(|n| !n.is_empty())
                .map(|n| class_name_to_descriptor(n).into_owned());
            match arr {
                Some(a) => vec![a, DESC_INT.to_string()],
                None => {
                    coords_exact = false;
                    vec![DESC_OBJECT.to_string(), DESC_INT.to_string()]
                }
            }
        }
        VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => {
            vec!["[B".to_string(), DESC_INT.to_string()]
        }
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => {
            vec!["Ljava/nio/ByteBuffer;".to_string(), DESC_INT.to_string()]
        }
        _ => {
            let receiver = meta
                .as_deref()
                .map(|m| m.class_name.as_str())
                .filter(|n| !n.is_empty())
                .map(|n| class_name_to_descriptor(n).into_owned());
            match receiver {
                Some(r) => vec![r],
                None => {
                    coords_exact = false;
                    vec![DESC_OBJECT.to_string()]
                }
            }
        }
    };
    (kind, value_desc.into_owned(), coords, coords_exact)
}

pub(crate) fn varhandle_access_mode_type_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let access_type = match args.get(1) {
        Some(Value::Object(Some(access_type))) => *access_type,
        _ => return Ok(None),
    };
    let access_type = vh_access_type_ordinal(ctx, access_type);

    let desc = if let Some(desc) = p67_memory_segment_varhandle_descriptor(ctx, this, access_type) {
        desc
    } else {
        let (_kind, value_desc, coords, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
        vh_access_mode_descriptor(access_type, &coords, &value_desc)
    };

    match build_method_type_from_descriptor(ctx, &desc)? {
        Some(mt) => Ok(Some(Value::Object(Some(mt)))),
        None => Ok(None),
    }
}

/// The `Class` mirror for a JVM field descriptor — `"I"` → `int.class`,
/// `"Ljava/lang/String;"` → `String.class`.
///
/// Both lookups are canonical caches (`primitive_mirrors` in `vm_object.rs`,
/// `get_or_create_class_mirror` for reference types), which is what makes
/// `vi.varType() == int.class` an IDENTITY match: the mirror the vector gets
/// from `getstatic Integer.TYPE` and the one produced here are the same object.
/// Same idiom as `build_method_type_from_descriptor` above.
pub(crate) fn vh_descriptor_mirror(ctx: &mut dyn NativeContext, desc: &str) -> ObjectRef {
    // An ARRAY descriptor has to go through `lang_class`, which knows to ask
    // the class manager to intern `[I` and falls back to a synthetic mirror.
    // The generic path below would end at `primitive_class_mirror("[I")` —
    // a fabricated PRIMITIVE mirror named `[I`, which is not `int[].class`
    // and would not compare equal to it. This matters for the array-element
    // handle's leading coordinate (`coordinateTypes()[0]`), which HotSpot
    // reports as the array class itself (measured: `[class [I, int]`).
    if desc.starts_with('[') {
        return crate::lang_class::descriptor_to_class_mirror(ctx, desc);
    }
    let name = descriptor_to_class_name(desc);
    match ctx.class_id_by_name(&name) {
        Some(cid) => ctx.get_class_mirror(cid),
        None => ctx.primitive_class_mirror(&name),
    }
}

/// The refusal both describe-yourself accessors raise when the receiver's
/// metadata cannot name what it addresses.
///
/// There is no JDK failure mode here — every real VarHandle can describe
/// itself — so any exception is a CratonVM deviation. It is still the right
/// one: the alternative is inventing a `Class`, and a fabricated variable type
/// is exactly the "plausible value where the VM does not know" shape the
/// strict corpus exists to find. `UnsupportedOperationException` is catchable
/// and cannot be mistaken for an answer.
pub(crate) fn vh_undescribable(what: &str, kind: i32) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!(
            "VarHandle.{what}: this CratonVM VarHandle carries no variable/coordinate type \
             metadata (kind {kind}); it was minted by a factory that does not record it, so \
             the answer is unknown rather than absent"
        ),
    }
    .into()
}

/// Build the `List<Class<?>>` that `coordinateTypes()` returns.
///
/// `List.of(Object[])` is the JDK-owned immutable-list construction, so the
/// result `equals` the `List.of(...)` a caller compares it against
/// (`RJdkHandles:245` asserts exactly that). `Arrays.asList` is the
/// synthetic-JDK fallback, matching `lang_system.rs`'s `boxed_int_list`.
///
/// Every mirror is pinned across the allocations: `new_array` and the `List.of`
/// invocation can both collect, and a raw `ObjectRef` held over a collection is
/// stale.
pub(crate) fn vh_class_list(ctx: &mut dyn NativeContext, descs: &[String]) -> Option<Value> {
    let mut mirrors: Vec<ObjectRef> = Vec::with_capacity(descs.len());
    let mut pins: Vec<usize> = Vec::with_capacity(descs.len());
    let mut base: Option<usize> = None;
    for d in descs {
        // Re-read every earlier mirror: resolving this one can allocate.
        for (i, h) in pins.iter().enumerate() {
            mirrors[i] = ctx.read_native_pin(*h, mirrors[i]);
        }
        let m = vh_descriptor_mirror(ctx, d);
        let h = ctx.pin_native_root(m);
        if base.is_none() {
            base = Some(h);
        }
        mirrors.push(m);
        pins.push(h);
    }
    let array = ctx.new_array(ArrayElementType::Reference, descs.len());
    let array_pin = ctx.pin_native_root(array);
    if base.is_none() {
        base = Some(array_pin);
    }
    let mut array = array;
    for (i, h) in pins.iter().enumerate() {
        let m = ctx.read_native_pin(*h, mirrors[i]);
        array = ctx.read_native_pin(array_pin, array);
        ctx.set_array_element(array, i, Value::Object(Some(m)));
    }
    array = ctx.read_native_pin(array_pin, array);
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    if let Some(b) = base {
        ctx.unpin_native_roots(b);
    }
    match list {
        Some(Value::Object(Some(_))) => list,
        _ => None,
    }
}

/// Is this handle's metadata authoritative enough to describe itself?
///
/// The WP4.2 side table is "the canonical truth" (see its banner) — every
/// factory in this file writes one. Without an entry, `vh_field_desc` falls
/// back to `DESC_OBJECT`, which is a workable erasure for `accessModeType` but
/// would make `varType()` report `Object` for, say, a real-JDK `int[]` handle
/// that reached us without going through our factories. Reporting a fabricated
/// `Class` is the one outcome these accessors must not produce.
///
/// A real SegmentVarHandle and a REAL-JDK array-element VarHandle are also
/// documented sources of truth: neither has a side-table entry, but each
/// names its own value and coordinate types through its JDK form
/// (see [`real_segment_vh_descriptors`] and
/// [`real_array_var_handle_descriptors`]).
pub(crate) fn vh_meta_is_authoritative(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    // The FFM layout table is a third source of truth, and a recorded one: the
    // carrier comes from the layout the handle was minted from, not from a
    // guess about the receiver's slots.
    p67_segment_vh_shape(ctx, vh).is_some()
        || vh_meta_get(ctx, vh).is_some()
        || real_segment_vh_descriptors(ctx, vh).is_some()
        || real_array_var_handle_descriptors(ctx, vh).is_some()
}

pub(crate) fn varhandle_var_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A combinator's handle names its own type (round 14 wave 7).
    if let Some(value) = indirect_vh_var_type(ctx, this) {
        return Ok(Some(Value::Object(Some(value))));
    }
    let (kind, value_desc, _coords, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
    // An empty or non-authoritative descriptor is "we never recorded one",
    // not a type.
    if value_desc.is_empty() || !vh_meta_is_authoritative(ctx, this) {
        return Err(vh_undescribable("varType", kind));
    }
    let mirror = vh_descriptor_mirror(ctx, &value_desc);
    Ok(Some(Value::Object(Some(mirror))))
}

pub(crate) fn varhandle_coordinate_types(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(list) = indirect_vh_coordinate_types(ctx, this) {
        return Ok(Some(list));
    }
    if let Some(list) = vh_lambda_proxy_coordinate_types(ctx, this) {
        return Ok(Some(list));
    }
    let (kind, _value_desc, coords, exact) = vh_value_and_coordinate_descriptors(ctx, this);
    if !exact || !vh_meta_is_authoritative(ctx, this) {
        return Err(vh_undescribable("coordinateTypes", kind));
    }
    match vh_class_list(ctx, &coords) {
        Some(v) => Ok(Some(v)),
        // Returning null here would be a null `coordinateTypes()`, which no
        // real VarHandle ever answers.
        None => Err(vh_undescribable("coordinateTypes", kind)),
    }
}

/// `coordinateTypes()` of an instance `VarHandle` over a lambda proxy's
/// capture (interpreter round i3 wave 79, lane L4; [`lk_lambda_proxy_var_handle`]):
/// `List.of(P.class)`, the proxy's own mirror by its recorded `ClassId`. The
/// descriptor path ([`vh_class_list`]) resolves the coordinate by NAME, which
/// a proxy (outside the class store) does not have. `None` for every other
/// handle (one meta read and an id compare). The array is a `Class[]`, which
/// `List.of(Object...)` takes as is.
pub(crate) fn vh_lambda_proxy_coordinate_types(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<Value> {
    let meta = vh_meta_get(ctx, vh)?;
    if meta.kind != VH_KIND_INSTANCE || meta.class_id < 0x8000_0000 {
        return None;
    }
    let proxy = ClassId::new(meta.class_id);
    crate::lang_class::lambda_proxy_class_name(&*ctx, proxy)?;
    let mirror = ctx.get_class_mirror(proxy);
    let mirror_pin = ctx.pin_native_root(mirror);
    let array = new_class_array(ctx, 1);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.set_array_element(array, 0, Value::Object(Some(mirror)));
    let array_pin = ctx.pin_native_root(array);
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    ctx.unpin_native_roots(mirror_pin);
    match list {
        Some(Value::Object(Some(_))) => list,
        _ => None,
    }
}

/// Read `width` bytes of a `byte[]` at BYTE index `idx`, assembled per
/// endianness, and box them as the view element type. This is the correct
/// `byteArrayViewVarHandle` get — distinct from an array-element access (which
/// would return a single signed byte). CratonVM stores `byte[]` elements as
/// signed `Value::Int`.
/// Bounds-check a byte-array-view `VarHandle` access, JDK-style.
///
/// `MethodHandles.byteArrayViewVarHandle` checks the START index against
/// `array.length - (width - 1)` — the number of positions a `width`-byte
/// element can start at — and reports it that way:
/// `Index -1 out of bounds for length 15` for a `short` view over a 16-byte
/// array, not "for length 16". `RuntimeError::aioobe` formats the JDK's
/// `Preconditions.checkIndex` wording, so the message matches character for
/// character.
///
/// There was no check at all here. Both `get` and `set` took the raw `int`
/// coordinate as `*i as usize`, so a negative index wrapped to an enormous
/// `usize` and the per-byte loop below simply read zeros or dropped the writes
/// — an out-of-bounds **write** that reported success. Nothing in the corpus
/// caught it because netty only takes this path when `sun.misc.Unsafe` is
/// unavailable, and CratonVM pinned `sun.misc.unsafe.memory.access` so netty
/// never did; on stock HotSpot 25, which does not pin it, netty's heap
/// `ByteBuf` reads and writes go through exactly this VarHandle.
pub(crate) fn byte_view_check_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    index: i32,
    elem: u8,
) -> Result<usize, MethodCallFailed> {
    let width = i64::from(byte_view_width(elem) as u32);
    let length = ctx.array_length(arr) as i64;
    // The JDK passes this signed bound to Preconditions.checkIndex, including
    // a negative length when the carrier is shorter than one view element.
    let limit = length - (width - 1);
    if i64::from(index) < 0 || i64::from(index) >= limit {
        return Err(RuntimeError::aioobe(index, limit as i32).into());
    }
    Ok(index as usize)
}

pub(crate) fn byte_view_get(ctx: &dyn NativeContext, arr: ObjectRef, idx: usize, elem: u8, le: bool) -> Value {
    let w = byte_view_width(elem);
    let mut raw: u64 = 0;
    for i in 0..w {
        let b = ctx.get_array_element(arr, idx + i).as_int().unwrap_or(0) as u8 as u64;
        if le {
            raw |= b << (8 * i);
        } else {
            raw = (raw << 8) | b;
        }
    }
    match elem {
        b'J' => Value::Long(raw as i64),
        b'D' => Value::Double(f64::from_bits(raw)),
        b'I' => Value::Int(raw as u32 as i32),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'S' => Value::Int((raw as u16) as i16 as i32), // short, sign-extended
        b'C' => Value::Int((raw as u16) as i32),        // char, zero-extended
        _ => Value::Int(raw as u8 as i8 as i32),
    }
}

/// Decompose `value` into `width` bytes per endianness and write them into the
/// `byte[]` at BYTE index `idx`. The correct `byteArrayViewVarHandle` set.
pub(crate) fn byte_view_set(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
    value: &Value,
) {
    let w = byte_view_width(elem);
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    for i in 0..w {
        let shift = if le { 8 * i } else { 8 * (w - 1 - i) };
        let b = ((raw >> shift) & 0xff) as u8 as i8 as i32;
        ctx.set_array_element(arr, idx + i, Value::Int(b));
    }
}

/// If `meta` is a byte-array-view VarHandle, return `(element_desc, little_endian)`.
pub(crate) fn byte_view_kind(meta: Option<&VarHandleMeta>) -> Option<(u8, bool)> {
    let m = meta?;
    let le = match m.kind {
        VH_KIND_BYTE_VIEW_LE => true,
        VH_KIND_BYTE_VIEW_BE => false,
        _ => return None,
    };
    Some((*m.field_desc.as_bytes().first().unwrap_or(&b'J'), le))
}

/// If `meta` is a byte-buffer-view VarHandle, return `(element_desc, little_endian)`.
pub(crate) fn byte_buffer_view_kind(meta: Option<&VarHandleMeta>) -> Option<(u8, bool)> {
    let m = meta?;
    let le = match m.kind {
        VH_KIND_BYTE_BUFFER_VIEW_LE => true,
        VH_KIND_BYTE_BUFFER_VIEW_BE => false,
        _ => return None,
    };
    Some((*m.field_desc.as_bytes().first().unwrap_or(&b'J'), le))
}

pub(crate) fn byte_view_decode(elem: u8, le: bool, bytes: &[u8]) -> Value {
    let mut raw: u64 = 0;
    if le {
        for (i, &b) in bytes.iter().enumerate() {
            raw |= (b as u64) << (8 * i);
        }
    } else {
        for &b in bytes {
            raw = (raw << 8) | b as u64;
        }
    }
    match elem {
        b'J' => Value::Long(raw as i64),
        b'D' => Value::Double(f64::from_bits(raw)),
        b'I' => Value::Int(raw as u32 as i32),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'S' => Value::Int((raw as u16) as i16 as i32),
        b'C' => Value::Int((raw as u16) as i32),
        _ => Value::Int(raw as u8 as i8 as i32),
    }
}

pub(crate) fn byte_view_encode(elem: u8, le: bool, value: &Value) -> [u8; 8] {
    let w = byte_view_width(elem);
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    let mut bytes = [0u8; 8];
    for i in 0..w {
        let shift = if le { 8 * i } else { 8 * (w - 1 - i) };
        bytes[i] = ((raw >> shift) & 0xff) as u8;
    }
    bytes
}

/// Bounds-check a byte-BUFFER-view `VarHandle` access.
///
/// The sibling of [`byte_view_check_index`]. HotSpot's contract differs in the
/// exception TYPE: a buffer view raises plain `IndexOutOfBoundsException` where
/// an array view raises `ArrayIndexOutOfBoundsException`. Both use
/// `Preconditions.checkIndex`'s wording and both bound the START index by
/// `length - (width - 1)` — for a buffer that length is its **limit**.
/// Measured on JDK 25: `IndexOutOfBoundsException: Index -1 out of bounds for
/// length 15` for a `short` view over a 16-byte buffer.
///
/// There was no check. Both call sites clamped a negative coordinate to `0`
/// instead, so `get(buf, -1)` silently read element 0 and `set(buf, -1, v)`
/// silently overwrote it — a wrong answer where the JDK throws, and on the
/// `set` side a write to memory the caller never named.
pub(crate) fn byte_buffer_view_check_index(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    index: i32,
    elem: u8,
) -> Result<usize, MethodCallFailed> {
    let width = i64::from(byte_view_width(elem) as u32);
    let limit = match ctx.get_field_by_name(bb, "limit") {
        Value::Int(l) => i64::from(l),
        _ => 0,
    };
    let bound = limit - (width - 1);
    if i64::from(index) < 0 || i64::from(index) >= bound {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(cratonvm_types::error::out_of_bounds_message::check_index(
                i64::from(index),
                bound,
            )),
        }
        .into());
    }
    Ok(index as usize)
}

/// Is this `ByteBuffer` a read-only view?
///
/// `VarHandle.set` through a byte-buffer view must raise
/// `ReadOnlyBufferException`; CratonVM performed the write instead, so a
/// `asReadOnlyBuffer()` handed to code that mutates it was not read-only at
/// all. Two signals, because the field is not present on every buffer shape
/// this VM can produce: the `isReadOnly` field when there is one, and
/// otherwise the JDK's own naming — the read-only views are exactly
/// `HeapByteBufferR` and `DirectByteBufferR`.
pub(crate) fn byte_buffer_view_is_read_only(ctx: &mut dyn NativeContext, bb: ObjectRef) -> bool {
    if let Value::Int(flag) = ctx.get_field_by_name(bb, "isReadOnly") {
        if flag != 0 {
            return true;
        }
    }
    ctx.class_name_of_id(ctx.class_id_of_object(bb))
        .map_or(false, |name| name.ends_with("ByteBufferR"))
}

/// The native address of a **direct** buffer's byte `idx`, or `None`.
///
/// The `hb == null` test is the whole point. `java.nio.Buffer.address` is NOT
/// zero for a heap buffer on JDK 21+ — it holds the array base offset (16 on
/// this platform) so that `Unsafe` accesses can use one code path for both
/// kinds. Reading it as an absolute pointer therefore dereferenced address
/// `16 + idx` for every heap buffer, which is a hard SIGSEGV, and the
/// heap-array fallback below it was unreachable:
///
/// ```text
/// SIGSEGV at pc=…, addr=0x10        # 0x10 == 16
/// ```
///
/// `hb` (the backing `byte[]`) is the field that actually distinguishes the
/// two: null on a direct buffer, non-null on a heap one.
pub(crate) fn byte_buffer_view_addr(ctx: &mut dyn NativeContext, bb: ObjectRef, idx: usize) -> Option<i64> {
    if matches!(ctx.get_field_by_name(bb, "hb"), Value::Object(Some(_))) {
        return None;
    }
    let base = match ctx.get_field_by_name(bb, "address") {
        Value::Long(a) if a > 0 => a,
        _ => return None,
    };
    base.checked_add(idx as i64)
}

pub(crate) fn byte_buffer_view_heap_array(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
) -> Option<(ObjectRef, usize)> {
    let arr = match ctx.get_field_by_name(bb, "hb") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let off = match ctx.get_field_by_name(bb, "offset") {
        Value::Int(i) if i >= 0 => i as usize,
        _ => 0,
    };
    Some((arr, off + idx))
}

/// The stored session field of an understood buffer segment. This uses the
/// existing real-segment layout resolver or the native carrier's own resolver;
/// unknown shapes stay on the authoritative checked access.
pub fn byte_buffer_view_stored_session(
    ctx: &dyn NativeContext,
    segment: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    if let Some(stored) = crate::panama::jit_carrier_stored_session(ctx, segment) {
        return Some(stored);
    }
    let fields = seg_lean_class(ctx, ctx.class_id_of_object(segment))?;
    match ctx.get_field(segment, fields.scope) {
        Value::Object(Some(session)) => Some((session, fields.scope)),
        _ => None,
    }
}

/// Hold shared-close protection from the scope check through the access.
/// Bounds, read-only and alignment refusals have already run. The scope check
/// can invoke Java on an unfamiliar or failing session, so pin the buffer and
/// hand its forwarded reference back before reading any payload fields.
pub(crate) fn byte_buffer_view_access_window(
    ctx: &mut dyn NativeContext,
    bb: &mut ObjectRef,
) -> Result<crate::ffm_fast::AccessWindow, MethodCallFailed> {
    let window = crate::ffm_fast::AccessWindow::open();
    let epoch = crate::ffm_fast::epoch();
    if let Value::Object(Some(mut segment)) = ctx.get_field_by_name(*bb, "segment") {
        let pin = ctx.pin_native_root(*bb);
        let checked = crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut segment);
        *bb = ctx.read_native_pin(pin, *bb);
        ctx.unpin_native_roots(pin);
        checked?;
        if let Some((session, _)) = byte_buffer_view_stored_session(ctx, segment) {
            crate::phases_late::foreign_ffm::p67_note_checked_session(ctx, session, epoch);
        }
    }
    Ok(window)
}

pub(crate) fn byte_buffer_view_get(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
) -> Option<Value> {
    let w = byte_view_width(elem);
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        let mut bytes = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut bytes[..w]) {
            return Some(byte_view_decode(elem, le, &bytes[..w]));
        }
    }
    if let Some((arr, start)) = byte_buffer_view_heap_array(ctx, bb, idx) {
        return Some(byte_view_get(ctx, arr, start, elem, le));
    }
    None
}

pub(crate) fn byte_buffer_view_set(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
    idx: usize,
    elem: u8,
    le: bool,
    value: &Value,
) {
    let w = byte_view_width(elem);
    let bytes = byte_view_encode(elem, le, value);
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        if ctx.copy_to_native_memory(addr, &bytes[..w]) {
            return;
        }
    }
    if let Some((arr, start)) = byte_buffer_view_heap_array(ctx, bb, idx) {
        for (i, &b) in bytes[..w].iter().enumerate() {
            ctx.set_array_element(arr, start + i, Value::Int(b as i8 as i32));
        }
    }
}

// ---------------------------------------------------------------------------
// java.lang.foreign / SegmentVarHandle (JEP 454 FFM API) coordinate access
// ---------------------------------------------------------------------------
//
// `MemorySegment.get/set(ValueLayout, long)` real bytecode
// (`jdk.internal.foreign.AbstractMemorySegmentImpl`) calls straight into
// `SegmentVarHandle.get/set(segment, offset[, value])`. That class is real
// bytecode with no native body for these signature-polymorphic accessors (no
// concrete `get`/`set` method exists on it at all — see JVMS §5.4.3.3), so it
// has no entry in our synthetic `VH_KIND` side-table either. Detected purely
// by the VarHandle's own (real) class name, distinct from the array-element
// and byte-array-view shapes handled above.

pub(crate) const SEGMENT_VAR_HANDLE_CLASS: &str = "java/lang/invoke/SegmentVarHandle";

pub(crate) fn is_segment_var_handle(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        == Some(SEGMENT_VAR_HANDLE_CLASS)
}

/// A `SegmentVarHandle`'s own `enclosing` (the `ValueLayout` it was built
/// from), baked-in `offset`, and `be` (byte order requires a swap) fields —
/// resolved by name since this is a real class (real, JDK-image field
/// layout), not our synthetic 6-field VarHandle shape.
pub(crate) fn segment_vh_fields(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(ObjectRef, i64, bool)> {
    let enclosing_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "enclosing")?;
    let offset_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "offset")?;
    let be_idx = ctx.resolve_field_index(SEGMENT_VAR_HANDLE_CLASS, "be")?;
    let enclosing = match ctx.get_field(vh, enclosing_idx) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let offset = match ctx.get_field(vh, offset_idx) {
        Value::Long(n) => n,
        _ => 0,
    };
    let be = matches!(ctx.get_field(vh, be_idx), Value::Int(n) if n != 0);
    Some((enclosing, offset, be))
}

/// SBR3 lane ffm: everything a real `SegmentVarHandle` access needs from the
/// HANDLE, decoded once. The handle is immutable (its layout, offset, byte
/// order and `VarForm` are final), but reading them cost three field-index
/// resolutions by name, a `VarForm` walk with a class-name render, four reads
/// for the variable-offset test and two layout queries on EVERY access --
/// about a third of a `get`/`set`'s time once the layout `varHandle`s run the
/// JDK's own factories.
#[derive(Clone, Copy)]
pub(crate) struct SegVhPlan {
    pub(crate) shape: SegShape,
    pub(crate) enclosing_size: i64,
    pub(crate) enclosing_align: i64,
    pub(crate) vh_offset: i64,
    pub(crate) be: bool,
    pub(crate) variable: bool,
    /// JIT round 20 wave 8 (lane huntFFM): the handle's `VarForm` is an
    /// ATOMIC-capable one -- `VarHandleSegmentAs<X>Aligned`, or the bytes /
    /// booleans form, which has no unaligned twin
    /// ([`segment_vh_form_class_is_aligned`]). JDK 25 picks it from the
    /// accessed VALUE layout's alignment (`Utils.makeRawSegmentViewVarHandle`
    /// passes `layout.byteAlignment() - 1`), not from `enclosing`, which is the
    /// GROUP layout of a path handle: a struct holding a `JAVA_LONG` and a
    /// `JAVA_INT_UNALIGNED` is 8-aligned, and its `x` handle still has only
    /// `get` / `set`.
    pub(crate) aligned_form: bool,
}

thread_local! {
    /// Per-thread memo of [`seg_vh_plan`], on the terms of [`P67_SHAPE_MEMO`]
    /// (keyed by VM, identity hash, address and [`VH_META_GENERATION`]).
    pub(crate) static SEG_VH_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<SegVhPlan>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The [`SegVhPlan`] of a real `SegmentVarHandle`, or `None` when its form or
/// layout cannot be read (the caller keeps the unplanned reading).
pub(crate) fn seg_vh_plan(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<SegVhPlan> {
    let hit = vh_memo_lookup(&SEG_VH_PLAN_MEMO, &*ctx, vh, |_| None);
    if hit.is_some() {
        return hit;
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, vh)?;
    let (shape, aligned_form) = segment_vh_form_info(ctx, vh)?;
    let variable = segment_vh_form_is_variable(ctx, vh);
    let enclosing_size = match crate::panama_libffi::layout_total_size(ctx, enclosing) {
        Ok(n) => n as i64,
        Err(_) => return None,
    };
    let enclosing_align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    let plan = SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        variable,
        aligned_form,
    };
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(vh);
    let addr = vh.as_ptr() as usize;
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    SEG_VH_PLAN_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = VhMemoLine {
            vm,
            key: hash,
            addr,
            generation,
            value: Some(plan),
        };
    });
    Some(plan)
}

/// Primitive "shape" of a real `ValueLayout`, read off its implementation
/// class name (`jdk.internal.foreign.layout.ValueLayouts$OfXxxImpl`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegShape {
    Byte,
    Boolean,
    Short,
    Char,
    Int,
    Long,
    Float,
    Double,
    Address,
}

pub(crate) fn segment_layout_shape(ctx: &mut dyn NativeContext, layout: ObjectRef) -> SegShape {
    let name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    if name.contains("OfByte") {
        SegShape::Byte
    } else if name.contains("OfBoolean") {
        SegShape::Boolean
    } else if name.contains("OfShort") {
        SegShape::Short
    } else if name.contains("OfChar") {
        SegShape::Char
    } else if name.contains("OfInt") {
        SegShape::Int
    } else if name.contains("OfLong") {
        SegShape::Long
    } else if name.contains("OfFloat") {
        SegShape::Float
    } else if name.contains("OfDouble") {
        SegShape::Double
    } else {
        SegShape::Address
    }
}

pub(crate) fn seg_shape_width(shape: SegShape) -> i64 {
    match shape {
        SegShape::Byte | SegShape::Boolean => 1,
        SegShape::Short | SegShape::Char => 2,
        SegShape::Int | SegShape::Float => 4,
        SegShape::Long | SegShape::Double | SegShape::Address => 8,
    }
}

pub(crate) fn seg_swap_bytes(raw: u64, width: i64) -> u64 {
    match width {
        1 => raw,
        2 => (raw as u16).swap_bytes() as u64,
        4 => (raw as u32).swap_bytes() as u64,
        _ => raw.swap_bytes(),
    }
}

pub(crate) fn seg_decode_value(shape: SegShape, raw: u64) -> Value {
    match shape {
        SegShape::Byte => Value::Int(raw as u8 as i8 as i32),
        SegShape::Boolean => Value::Int((raw as u8 != 0) as i32),
        SegShape::Short => Value::Int(raw as u16 as i16 as i32),
        SegShape::Char => Value::Int(raw as u16 as i32),
        SegShape::Int => Value::Int(raw as u32 as i32),
        SegShape::Float => Value::Float(f32::from_bits(raw as u32)),
        SegShape::Long | SegShape::Address => Value::Long(raw as i64),
        SegShape::Double => Value::Double(f64::from_bits(raw)),
    }
}

pub(crate) fn seg_shape_desc(shape: SegShape) -> &'static str {
    match shape {
        SegShape::Byte => "B",
        SegShape::Boolean => "Z",
        SegShape::Short => "S",
        SegShape::Char => "C",
        SegShape::Int => "I",
        SegShape::Long => "J",
        SegShape::Float => "F",
        SegShape::Double => "D",
        SegShape::Address => "J",
    }
}

/// `CRATONVM_FFM_SEGMENT_VH_FORM` (default on; round 13 wave 1, lane mhffm,
/// both modes): a real `SegmentVarHandle` is read as JDK 25 builds it -- the
/// value type from its `VarForm` (`VarHandles.memorySegmentViewHandle` picks
/// `VarHandleSegmentAs<Carrier>s` from the BASE carrier: `long` for an
/// `ADDRESS` layout, whose `MemorySegment` view is a `filterValue` adapter
/// above this handle), `enclosing` as the layout the coordinate must fit and
/// be aligned for (`checkEnclosingLayout`: a path handle's `enclosing` is the
/// GROUP layout), the variable-offset form's extra `long` coordinate, and the
/// JDK's check order (read-only, bounds, alignment, then the session). `0`
/// restores round 12 wave 7's reading (value type and bounds from
/// `enclosing`, the scope checked first).
pub(crate) fn segment_vh_form_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_SEGMENT_VH_FORM")
    })
}

/// A field of a real JDK object: by name on the object, else through the
/// declaring class's field table (`get_field_by_name` answers null for a name
/// it cannot resolve, as `panama::heap_seg_field` notes).
pub(crate) fn real_object_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    declaring: &str,
    name: &str,
) -> Value {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(None) => match ctx.resolve_field_index(declaring, name) {
            Some(index) => ctx.get_field(obj, index),
            None => Value::Object(None),
        },
        other => other,
    }
}

/// The value shape of a real `SegmentVarHandle`, from its `VarForm`'s
/// `implClass` (`java/lang/invoke/VarHandleSegmentAsInts`, or its
/// `...Aligned` subclass). `None` when the form cannot be read, which keeps
/// the `enclosing`-derived reading.
pub(crate) fn segment_vh_form_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<SegShape> {
    segment_vh_form_info(ctx, vh).map(|(shape, _)| shape)
}

/// [`segment_vh_form_shape`] plus whether the form is atomic-capable
/// ([`segment_vh_form_class_is_aligned`]), read off the same `implClass`.
pub(crate) fn segment_vh_form_info(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(SegShape, bool)> {
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let name = ctx.class_name_of_id(impl_class)?;
    let shape = segment_vh_shape_of_form_class(&name)?;
    Some((shape, segment_vh_form_class_is_aligned(&name)))
}

/// JIT round 20 wave 8 (lane huntFFM): does the form class `name` (one
/// [`segment_vh_shape_of_form_class`] accepts) define the access modes past
/// `get` / `set`? JDK 25's `VarHandleSegmentAs<X>.selectForm` hands an
/// UNALIGNED value layout (`alignmentMask` short of the carrier's size) the
/// base class, which declares `get` / `set` only, and an aligned one the
/// `...Aligned` subclass, which adds the ordered and read-modify-write modes;
/// `VarHandleSegmentAsBytes` / `...Booleans` have no subclass and declare the
/// ordered modes themselves. Measured on HotSpot 25.0.3: `JAVA_INT_UNALIGNED`
/// and `JAVA_LONG.withByteAlignment(4)` support exactly `GET` / `SET`.
pub(crate) fn segment_vh_form_class_is_aligned(name: &str) -> bool {
    name.ends_with("Aligned")
        || matches!(
            name,
            "java/lang/invoke/VarHandleSegmentAsBytes"
                | "java/lang/invoke/VarHandleSegmentAsBooleans"
        )
}

/// [`segment_vh_form_shape`]'s class-name table.
pub(crate) fn segment_vh_shape_of_form_class(name: &str) -> Option<SegShape> {
    let kind = name.strip_prefix("java/lang/invoke/VarHandleSegmentAs")?;
    let kind = kind.strip_suffix("Aligned").unwrap_or(kind);
    Some(match kind {
        "Bytes" => SegShape::Byte,
        "Booleans" => SegShape::Boolean,
        "Shorts" => SegShape::Short,
        "Chars" => SegShape::Char,
        "Ints" => SegShape::Int,
        "Longs" => SegShape::Long,
        "Floats" => SegShape::Float,
        "Doubles" => SegShape::Double,
        _ => return None,
    })
}

/// Does a real `SegmentVarHandle` take the VARIABLE-offset form's extra
/// `long` coordinate (`(MemorySegment, long base, long offset)`)? Read off
/// the form's `GET` method type (`methodType_table[0]`, as
/// `SegmentVarHandle.accessModeTypeUncached` does); `false` when unreadable.
pub(crate) fn segment_vh_form_is_variable(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return false;
    };
    let Value::Object(Some(table)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "methodType_table")
    else {
        return false;
    };
    if !ctx.object_is_array(table) || ctx.array_length(table) == 0 {
        return false;
    }
    let Value::Object(Some(get_type)) = ctx.get_array_element(table, 0) else {
        return false;
    };
    match ctx.get_field_by_name(get_type, "ptypes") {
        Value::Object(Some(ptypes)) if ctx.object_is_array(ptypes) => ctx.array_length(ptypes) == 3,
        _ => false,
    }
}

/// JDK 25 `AbstractMemorySegmentImpl.checkEnclosingLayout(base, enclosing, ro)`
/// minus its read-only half (the caller asks that first): the whole
/// `enclosing` layout must fit at `base` (`IndexOutOfBoundsException`), and
/// `base` must meet its alignment in this segment (`IllegalArgumentException`;
/// a heap segment promises at most its element size, `isAlignedForElement`).
/// A synthetic `Unsafe` arena handle is not an address and is not
/// alignment-checked (as in [`segment_vh_misaligned`]).
pub(crate) fn segment_vh_check_enclosing(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: ObjectRef,
    enclosing_size: i64,
    enclosing_align: i64,
    base: i64,
) -> Result<(), MethodCallFailed> {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let real = is_real_foreign_segment(ctx, seg);
    let fits = base >= 0 && base <= size.saturating_sub(enclosing_size.max(0));
    if !fits {
        // SBR3 lane ffm: the JDK's words
        // (`AbstractMemorySegmentImpl.outOfBoundException`), naming the
        // segment (j27 wave 4, lane FFMMSG: a carrier too; it said
        // `offset=.., length=.., size=..`).
        let text = segment_refusal_text(ctx, seg);
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(crate::panama::out_of_bounds_message(&text, base, enclosing_size)),
        }
        .into());
    }
    if enclosing_align <= 1 {
        return Ok(());
    }
    if crate::panama::heap_segment_view(ctx, seg).is_none() {
        let addr = crate::panama_libffi::segment_address(ctx, seg).wrapping_add(base);
        // An arena handle is 16-aligned (`reserved_for`), as a `malloc` block
        // is, so a REAL native segment over one is alignment-checked like any
        // address (the JDK's `isAlignedForElement` over `unsafeGetOffset`);
        // a carrier's handle keeps the old exemption.
        if !crate::panama::ffm_native_align_check() || (!real && crate::unsafe_arena_contains(addr))
        {
            return Ok(());
        }
    }
    if crate::panama::pe_segment_max_byte_alignment_at(ctx, seg, base) < enclosing_align {
        // j27 wave 4 (lane FFMMSG): a carrier gets HotSpot's text too.
        return Err(enclosing_misaligned(ctx, seg, Some(vh), base, enclosing_align));
    }
    Ok(())
}

/// Is `seg` an instance of one of the JDK's own `jdk/internal/foreign/*`
/// segment classes (as opposed to this VM's `MemorySegmentImpl` carrier)?
pub(crate) fn is_real_foreign_segment(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(seg))
        .is_some_and(|n| n.starts_with("jdk/internal/foreign/"))
}

/// The segment's `toString()` for a refusal message (j27 wave 4, lane
/// FFMMSG): a real JDK segment's own (runs Java), a carrier's from
/// `panama::segment_to_string` (and that as the fallback).
fn segment_refusal_text(ctx: &mut dyn NativeContext, seg: ObjectRef) -> String {
    let real = is_real_foreign_segment(ctx, seg)
        .then(|| jdk_object_to_string(ctx, seg))
        .flatten();
    real.unwrap_or_else(|| crate::panama::segment_to_string(ctx, seg))
}

/// The `enclosing` layout of a real `SegmentVarHandle` (`None` for any other
/// handle object, whose slots this must not read positionally).
fn vh_enclosing_layout(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<ObjectRef> {
    let is_segment_vh = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(vh))
        .is_some_and(|n| &*n == SEGMENT_VAR_HANDLE_CLASS);
    if !is_segment_vh {
        return None;
    }
    segment_vh_fields(ctx, vh).map(|fields| fields.0)
}

/// `checkEnclosingLayout`'s `Target offset %d is incompatible with alignment
/// constraint %d (of %s) for segment %s` (j27 wave 4, lane FFMMSG; the real
/// segment's text was SBR3's). The layout is read off the handle before any
/// Java runs and pinned across the segment's `toString` (a GC during it
/// moves the layout).
fn enclosing_misaligned(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: Option<ObjectRef>,
    offset: i64,
    align: i64,
) -> MethodCallFailed {
    let layout = vh.and_then(|vh| vh_enclosing_layout(ctx, vh));
    let pinned = layout.map(|layout| (ctx.pin_native_root(layout), layout));
    let segment = segment_refusal_text(ctx, seg);
    let layout = pinned.map(|(pin, layout)| {
        let now = ctx.read_native_pin(pin, layout);
        ctx.unpin_native_roots(pin);
        now
    });
    RuntimeError::IllegalArgumentException {
        message: crate::panama::misaligned_message(ctx, &segment, layout, offset, align),
    }
    .into()
}

/// `obj.toString()` through the JDK's own bytecode, for an error message.
pub(crate) fn jdk_object_to_string(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// A `long` coordinate of a VarHandle call.
pub(crate) fn vh_long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    }
}

/// [`segment_vh_get`] / [`segment_vh_set`] under [`segment_vh_form_enabled`]:
/// `shape` is the form's value type; `args` is `[vh, seg, base, (offset,)?
/// (value)?]`. `enclosing`'s size and alignment are read before anything can
/// run Java.
#[allow(clippy::too_many_arguments)]
pub(crate) fn segment_vh_form_access(
    ctx: &mut dyn NativeContext,
    plan: SegVhPlan,
    vh: ObjectRef,
    seg: ObjectRef,
    args: &[Value],
    write: bool,
) -> MethodCallResult {
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = plan;
    // `[vh, seg, base]` + the variable form's `offset` + a write's value.
    let variable = args.len() == 4 + usize::from(write);
    let base = vh_long_arg(args, 2);
    let offset = if variable {
        vh_long_arg(args, 3)
    } else {
        vh_offset
    };
    let width = seg_shape_width(shape);
    if write {
        let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
            ctx,
            &[Value::Object(Some(seg))],
        )?;
        if matches!(read_only, Some(Value::Int(n)) if n != 0) {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Attempt to write a read-only segment".into(),
            }
            .into());
        }
    }
    segment_vh_check_enclosing(ctx, seg, vh, enclosing_size, enclosing_align, base)?;
    let raw_in = if write {
        let value = args
            .get(3 + usize::from(variable))
            .copied()
            .unwrap_or(Value::Int(0));
        let raw = seg_encode_value(&value, width);
        Some(if be { seg_swap_bytes(raw, width) } else { raw })
    } else {
        None
    };
    // An access window from before the session check to after the last
    // touch of the block (see `ffm_fast::AccessWindow`).
    let window = crate::ffm_fast::AccessWindow::open();
    let mut seg = seg;
    segment_vh_check_scope(ctx, &mut seg)?;
    // Re-derived after the session check, which can run Java and move a heap
    // segment's array. `enclosing` covers `[base, base + size)`, so this
    // bounds check only guards a malformed handle.
    let (heap_base, addr) = segment_raw_access(ctx, seg, offset, base, width)?;
    let heap_bounds = || {
        MethodCallFailed::from(RuntimeError::IllegalStateException {
            message: "Out of bound access on heap MemorySegment".into(),
        })
    };
    match raw_in {
        Some(raw) => {
            match heap_base {
                None => segment_native_set(addr, width, raw)?,
                Some(array) => {
                    if !segment_heap_set(ctx, array, addr, width, raw) {
                        return Err(heap_bounds());
                    }
                }
            }
            drop(window);
            Ok(None)
        }
        None => {
            let raw = match heap_base {
                None => segment_native_get(addr, width)?,
                Some(array) => segment_heap_get(ctx, array, addr, width).ok_or_else(heap_bounds)?,
            };
            drop(window);
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            // CANONICAL, as the enclosing-derived reading below: the variant
            // and the descriptor switch on the same `SegShape`.
            Ok(Some(box_value_canonical(
                ctx,
                seg_decode_value(shape, raw),
                seg_shape_desc(shape),
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// SBR3 lane ffmperf: the lean access of a REAL native segment
// ---------------------------------------------------------------------------
//
// Once the JDK's own `Arena` / `NativeMemorySegmentImpl` run, `seg.get(JAVA_INT,
// off)` is `accessHandle().get(this, off)` on a real `SegmentVarHandle`, and the
// carriers' one-native fast accessor (80 ns per get+set) became the generic
// `VarHandle` native road (3.9 us: the access-mode prologue, three field-index
// resolutions by NAME per access, class-name `Arc`s, the arena store's lock taken
// twice). This is the same access `segment_vh_form_access` performs, decided
// from the handle's memoised [`SegVhPlan`], the segment class's memoised field
// slots, the session class's memoised `state`/`owner` slots and ONE arena-store
// call -- and it only ever ANSWERS: any condition it cannot prove (a heap
// segment, a carrier, a variable-offset form, a read-only or out-of-bounds or
// misaligned access, a closed or foreign-owned session, a freed handle, a value
// of another type) declines to `None` and the existing road runs and raises
// exactly what it always did. `CRATONVM_FFM_SEGMENT_VH_LEAN=0` turns it off.

#[derive(Clone, Copy)]
pub(crate) struct SegLeanClass {
    pub(crate) scope: usize,
    pub(crate) length: usize,
    pub(crate) read_only: usize,
    pub(crate) min: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct SegLeanSession {
    pub(crate) state: usize,
    pub(crate) owner: usize,
}

pub(crate) const SEG_LEAN_MEMO_WAYS: usize = 8;

thread_local! {
    /// `(vm, class, slots)` of the segment classes the lean path serves
    /// (`None`: another class, which declines).
    pub(crate) static SEG_LEAN_CLASS_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<SegLeanClass>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `(vm, class, slots)` of the real session classes.
    pub(crate) static SEG_LEAN_SESSION_MEMO: std::cell::RefCell<Vec<(usize, ClassId, Option<SegLeanSession>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn seg_lean_memo<T: Copy>(
    memo: &'static std::thread::LocalKey<std::cell::RefCell<Vec<(usize, ClassId, Option<T>)>>>,
    vm: usize,
    class_id: ClassId,
    compute: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let hit = memo.with(|m| {
        m.borrow()
            .iter()
            .find(|e| e.0 == vm && e.1 == class_id)
            .map(|e| e.2)
    });
    if let Some(found) = hit {
        return found;
    }
    let value = compute();
    memo.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= SEG_LEAN_MEMO_WAYS {
            m.remove(0);
        }
        m.push((vm, class_id, value));
    });
    value
}

pub(crate) fn seg_lean_class(ctx: &dyn NativeContext, class_id: ClassId) -> Option<SegLeanClass> {
    seg_lean_memo(&SEG_LEAN_CLASS_MEMO, ctx.vm_identity(), class_id, || {
        let name = ctx.class_name_arc_of_id(class_id)?;
        if !matches!(
            &*name,
            "jdk/internal/foreign/NativeMemorySegmentImpl"
                | "jdk/internal/foreign/MappedMemorySegmentImpl"
        ) {
            return None;
        }
        Some(SegLeanClass {
            scope: ctx.resolve_field_index_by_class_id(class_id, "scope")?,
            length: ctx.resolve_field_index_by_class_id(class_id, "length")?,
            read_only: ctx.resolve_field_index_by_class_id(class_id, "readOnly")?,
            min: ctx.resolve_field_index_by_class_id(class_id, "min")?,
        })
    })
}

pub(crate) fn seg_lean_session(ctx: &dyn NativeContext, class_id: ClassId) -> Option<SegLeanSession> {
    seg_lean_memo(&SEG_LEAN_SESSION_MEMO, ctx.vm_identity(), class_id, || {
        // A real session (the VM's own `MemorySessionImpl` shape is not one,
        // see `p67_session_is_real`).
        if ctx.class_name_arc_of_id(class_id).as_deref()
            == Some("jdk/internal/foreign/MemorySessionImpl")
        {
            return None;
        }
        Some(SegLeanSession {
            state: ctx.resolve_field_index_by_class_id(class_id, "state")?,
            owner: ctx.resolve_field_index_by_class_id(class_id, "owner")?,
        })
    })
}

/// The final fields of one real native segment (`length`, `readOnly`, `scope`,
/// `min` never change after construction) and its session's field slots,
/// valid for one address in one [`crate::ffm_fast::epoch`] -- the epoch moves
/// on every collection (the only thing that moves a segment or its session)
/// and on every free of native memory, so an address stands for its object in
/// between, the argument `ffm_fast`'s carrier verdicts rest on. What is NOT
/// cached is anything a close can change: the session's `state` and `owner`
/// are read on every access.
#[derive(Clone, Copy)]
pub(crate) struct SegLeanFacts {
    pub(crate) vm: usize,
    pub(crate) addr: usize,
    pub(crate) epoch: u64,
    pub(crate) size: i64,
    pub(crate) read_only: bool,
    pub(crate) min: i64,
    pub(crate) scope: ObjectRef,
    pub(crate) session: SegLeanSession,
}

pub(crate) const SEG_LEAN_FACT_WAYS: usize = 4;

thread_local! {
    pub(crate) static SEG_LEAN_FACTS: std::cell::Cell<[Option<SegLeanFacts>; SEG_LEAN_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_LEAN_FACT_WAYS]) };
    pub(crate) static SEG_LEAN_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn seg_lean_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    class_hint: Option<ClassId>,
) -> Option<SegLeanFacts> {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let ways = SEG_LEAN_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm {
            return Some(*way);
        }
    }
    let class_id = match class_hint {
        Some(id) => id,
        None => ctx.class_id_of_object(seg),
    };
    let class = seg_lean_class(&*ctx, class_id)?;
    let mut fields = [Value::Int(0); 4];
    ctx.get_fields_typed(
        seg,
        &[
            (class.length, b'J'),
            (class.read_only, b'Z'),
            (class.scope, b'L'),
            (class.min, b'J'),
        ],
        &mut fields,
    );
    let (Value::Long(size), Value::Int(read_only), Value::Object(Some(scope)), Value::Long(min)) =
        (fields[0], fields[1], fields[2], fields[3])
    else {
        return None;
    };
    let session = seg_lean_session(&*ctx, ctx.class_id_of_object(scope))?;
    let facts = SegLeanFacts {
        vm,
        addr,
        epoch,
        size,
        read_only: read_only != 0,
        min,
        scope,
        session,
    };
    let slot = SEG_LEAN_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_LEAN_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(facts);
    SEG_LEAN_FACTS.with(|c| c.set(updated));
    Some(facts)
}

/// `CRATONVM_FFM_SEGMENT_VH_LEAN` (default on): see the block comment above.
pub(crate) fn segment_vh_lean_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_SEGMENT_VH_LEAN")
    })
}

/// The JVM descriptor character a call site declares for `shape`'s carrier.
pub(crate) fn seg_shape_site_kind(shape: SegShape) -> u8 {
    match shape {
        SegShape::Byte => b'B',
        SegShape::Boolean => b'Z',
        SegShape::Short => b'S',
        SegShape::Char => b'C',
        SegShape::Int => b'I',
        SegShape::Long | SegShape::Address => b'J',
        SegShape::Float => b'F',
        SegShape::Double => b'D',
    }
}

/// One planned `get` (`write == None`: the decoded value comes back) or `set`
/// (`Some(value)`: `Some(None)` comes back) of a real native segment, or `None`
/// to decline. `site_kind` is the call site's declared carrier (`0`: not
/// known, the caller has judged the value itself).
pub(crate) fn segment_vh_lean(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    write: Option<Value>,
    site_kind: u8,
    keys: Option<(i32, ClassId)>,
) -> Option<(SegShape, Option<Value>)> {
    if !segment_vh_lean_enabled() || !segment_vh_checks() || !segment_vh_form_enabled() {
        return None;
    }
    // The funnel already knows the handle's identity hash and the segment's
    // class (its per-thread receiver memo); a native road asks the context.
    let plan = match keys {
        Some((hash, _)) => seg_vh_plan_hit(ctx.vm_identity(), hash, vh.as_ptr() as usize)?,
        None => vh_memo_lookup(&SEG_VH_PLAN_MEMO, &*ctx, vh, |_| None)?,
    };
    segment_vh_lean_planned(ctx, plan, seg, base, write, site_kind, keys.map(|k| k.1))
}

/// [`segment_vh_lean`] once the handle's plan is in hand (SBR4 lane segacc: the
/// compiled `MemorySegment.get/set` door has the LAYOUT's plan, not a handle).
pub(crate) fn segment_vh_lean_planned(
    ctx: &mut dyn NativeContext,
    plan: SegVhPlan,
    seg: ObjectRef,
    base: i64,
    write: Option<Value>,
    site_kind: u8,
    class_hint: Option<ClassId>,
) -> Option<(SegShape, Option<Value>)> {
    if plan.variable {
        return None;
    }
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = plan;
    if site_kind != 0 && seg_shape_site_kind(shape) != site_kind {
        return None;
    }
    let width = seg_shape_width(shape);
    let raw_in = match write {
        Some(value) => {
            let fits = matches!(
                (shape, &value),
                (
                    SegShape::Byte
                        | SegShape::Boolean
                        | SegShape::Short
                        | SegShape::Char
                        | SegShape::Int,
                    Value::Int(_)
                ) | (SegShape::Long | SegShape::Address, Value::Long(_))
                    | (SegShape::Float, Value::Float(_))
                    | (SegShape::Double, Value::Double(_))
            );
            if !fits {
                return None;
            }
            let raw = seg_encode_value(&value, width);
            Some(if be { seg_swap_bytes(raw, width) } else { raw })
        }
        None => None,
    };
    // r23 wave 3, lane L8: a heap segment this thread already filed is
    // served before the native facts are asked for (they miss on every heap
    // access: no `min`, and a miss files nothing).
    if crate::nbflags().ffm_segment_vh_lean_heap && seg_heap_facts_filed(ctx, seg) {
        let out = segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)?;
        return Some((shape, out));
    }
    let Some(SegLeanFacts {
        size,
        read_only,
        min,
        scope,
        session,
        ..
    }) = seg_lean_facts(ctx, seg, class_hint)
    else {
        // r23 wave 2c, lane L8: not a native segment -- a real heap one
        // (`MemorySegment.ofArray`) is served through its backing array.
        if !crate::nbflags().ffm_segment_vh_lean_heap {
            return None;
        }
        let out = segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)?;
        return Some((shape, out));
    };
    if raw_in.is_some() && read_only {
        return None;
    }
    // The JDK's checks in the JDK's order, all of them passing or the
    // existing road reports the failure.
    if base < 0 || base > size.saturating_sub(enclosing_size.max(0)) {
        return None;
    }
    let total = vh_offset.checked_add(base)?;
    if total < 0 || total.checked_add(width)? > size {
        return None;
    }
    if enclosing_align > 1 && min.wrapping_add(base) & (enclosing_align - 1) != 0 {
        return None;
    }
    let addr = min.checked_add(total)?;
    // An arena handle (what the JDK's `allocateMemory` answers on this VM) goes
    // through the arena store; a plain non-null pointer (a mapped file, an
    // `ofAddress` window) is dereferenced as `segment_native_get` does. A freed
    // or foreign tagged value declines.
    let handle = crate::panama_libffi::is_arena_handle(addr);
    if !handle && (addr <= 0 || crate::unsafe_arena_addr_is_tagged(addr)) {
        return None;
    }
    // An access window from before the session check to after the last touch
    // of the block (see `ffm_fast::AccessWindow`).
    let window = crate::ffm_fast::AccessWindow::open();
    let mut state_owner = [Value::Int(0); 2];
    ctx.get_fields_typed(
        scope,
        &[(session.state, b'I'), (session.owner, b'L')],
        &mut state_owner,
    );
    match state_owner[0] {
        Value::Int(state) if state >= 0 => {}
        _ => return None,
    }
    match state_owner[1] {
        Value::Object(None) => {}
        Value::Object(Some(owner)) => {
            // zs s1/nd: the EXISTING mirror only (`current_thread_object_if_any`,
            // as `foreign_ffm.rs`'s owner check): a thread with no mirror yet
            // cannot own this session, and minting one here is an allocation
            // from a JIT helper (`try_varhandle_segment_access`), where the
            // one-attempt door aborts on a full ZGC heap.
            if ctx.current_thread_object_if_any() != Some(owner) {
                return None;
            }
        }
        _ => return None,
    }
    let out = match raw_in {
        Some(raw) => {
            if handle {
                let bytes = raw.to_le_bytes();
                if !crate::unsafe_arena_copy_in(addr, &bytes[..width as usize]) {
                    return None;
                }
            } else {
                // SAFETY: the address is the segment's own `min + offset`, inside
                // the segment's bounds (checked above), as in `segment_native_set`.
                unsafe {
                    let ptr = addr as usize as *mut u8;
                    match width {
                        1 => *ptr = raw as u8,
                        2 => (ptr as *mut u16).write_unaligned(raw as u16),
                        4 => (ptr as *mut u32).write_unaligned(raw as u32),
                        _ => (ptr as *mut u64).write_unaligned(raw),
                    }
                }
            }
            None
        }
        None => {
            let raw = if handle {
                let mut buf = [0u8; 8];
                if !crate::unsafe_arena_copy_out(addr, &mut buf[..width as usize]) {
                    return None;
                }
                u64::from_le_bytes(buf)
            } else {
                // SAFETY: see the store above; `segment_native_get`'s own read.
                unsafe {
                    let ptr = addr as usize as *const u8;
                    match width {
                        1 => *ptr as u64,
                        2 => (ptr as *const u16).read_unaligned() as u64,
                        4 => (ptr as *const u32).read_unaligned() as u64,
                        _ => (ptr as *const u64).read_unaligned(),
                    }
                }
            };
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            Some(seg_decode_value(shape, raw))
        }
    };
    drop(window);
    Some((shape, out))
}

/// JIT round 23 wave 2c, lane L8
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// the final facts of one real HEAP segment (`HeapMemorySegmentImpl$Of*`: its
/// backing array, data start, size and read-only flag never change) and its
/// session's field slots, filed per thread for one segment address in one
/// [`crate::ffm_fast::epoch`] -- the argument [`SegLeanFacts`] rests on (the
/// epoch moves on every collection, the only thing that moves the segment or
/// its array). The session's `state` and `owner` are read on every access.
#[derive(Clone, Copy)]
pub(crate) struct SegHeapFacts {
    pub(crate) vm: usize,
    pub(crate) addr: usize,
    pub(crate) epoch: u64,
    pub(crate) view: crate::panama::HeapSegmentView,
    pub(crate) scope: ObjectRef,
    pub(crate) session: SegLeanSession,
}

pub(crate) const SEG_HEAP_FACT_WAYS: usize = 4;

thread_local! {
    pub(crate) static SEG_HEAP_FACTS: std::cell::Cell<[Option<SegHeapFacts>; SEG_HEAP_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_HEAP_FACT_WAYS]) };
    pub(crate) static SEG_HEAP_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Is `seg` in this thread's [`SegHeapFacts`] lines for the current epoch?
/// Reads only (r23 wave 3, lane L8).
pub(crate) fn seg_heap_facts_filed(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    SEG_HEAP_FACTS.with(std::cell::Cell::get).iter().flatten().any(|way| {
        way.addr == addr && way.epoch == epoch && way.vm == vm
    })
}

pub(crate) fn seg_heap_facts(ctx: &mut dyn NativeContext, seg: ObjectRef) -> Option<SegHeapFacts> {
    let vm = ctx.vm_identity();
    let addr = seg.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let ways = SEG_HEAP_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm {
            return Some(*way);
        }
    }
    // A REAL heap segment only: this VM's own `ofArray` carriers keep their
    // older road.
    if !crate::panama_libffi::is_real_heap_segment(&*ctx, seg) {
        return None;
    }
    let view = crate::panama::heap_segment_view(&*ctx, seg)?;
    let Value::Object(Some(scope)) = real_object_field(
        ctx,
        seg,
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "scope",
    ) else {
        return None;
    };
    let session = seg_lean_session(&*ctx, ctx.class_id_of_object(scope))?;
    let facts = SegHeapFacts {
        vm,
        addr,
        epoch,
        view,
        scope,
        session,
    };
    let slot = SEG_HEAP_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_HEAP_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(facts);
    SEG_HEAP_FACTS.with(|c| c.set(updated));
    Some(facts)
}

/// [`segment_vh_lean_planned`]'s heap arm: one planned `get` (`raw_in ==
/// None`: `Some(Some(value))`) or `set` (`raw_in`: the value's memory image,
/// already byte-swapped for a big-endian handle; `Some(None)`) of a real heap
/// segment, or `None` to decline -- any check that does not pass declines, and
/// the full road ([`segment_vh_get`] / its `set` twin) then raises exactly
/// what it always raised. The JDK's checks, in its order: read-only, bounds,
/// alignment (a layout aligned wider than the backing element is the heap
/// segment's `maxAlignMask` refusal, so it declines too), then the session.
///
/// The bytes are the backing array's, little-endian through the element and
/// through the array (`panama::heap_bytes_read`'s measured rule); an access as
/// wide as the element and aligned to it is one element read or write.
pub(crate) fn segment_vh_lean_heap(
    ctx: &mut dyn NativeContext,
    plan: &SegVhPlan,
    seg: ObjectRef,
    base: i64,
    raw_in: Option<u64>,
) -> Option<Option<Value>> {
    let SegVhPlan {
        shape,
        enclosing_size,
        enclosing_align,
        vh_offset,
        be,
        ..
    } = *plan;
    let width = seg_shape_width(shape);
    let SegHeapFacts {
        view,
        scope,
        session,
        ..
    } = seg_heap_facts(ctx, seg)?;
    let size = view.size();
    if raw_in.is_some() && view.read_only() {
        return None;
    }
    if base < 0 || base > size.saturating_sub(enclosing_size.max(0)) {
        return None;
    }
    let total = vh_offset.checked_add(base)?;
    if total < 0 || total.checked_add(width)? > size {
        return None;
    }
    // Cast: an element is 1..=8 bytes.
    let elem_width = view.elem_width as i64;
    let byte_start = view.start.checked_add(total)?;
    if enclosing_align > elem_width
        || (enclosing_align > 1 && byte_start & (enclosing_align - 1) != 0)
    {
        return None;
    }
    let mut state_owner = [Value::Int(0); 2];
    ctx.get_fields_typed(
        scope,
        &[(session.state, b'I'), (session.owner, b'L')],
        &mut state_owner,
    );
    match state_owner[0] {
        Value::Int(state) if state >= 0 => {}
        _ => return None,
    }
    match state_owner[1] {
        Value::Object(None) => {}
        Value::Object(Some(owner)) => {
            if ctx.current_thread_object_if_any() != Some(owner) {
                return None;
            }
        }
        _ => return None,
    }
    // Cast: `byte_start >= 0` (the view's start and `total` are), and the
    // view proved the whole segment lies inside the array.
    let whole_element = width == elem_width && byte_start % elem_width == 0;
    let index = (byte_start / elem_width) as usize;
    let mask = if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * width)) - 1
    };
    match raw_in {
        Some(raw) => {
            if whole_element {
                crate::panama::heap_store_element(&*ctx, view.base, view.elem_type, index, raw);
            } else {
                crate::panama::heap_bytes_write(
                    &*ctx,
                    view.base,
                    view.elem_type,
                    view.elem_width,
                    byte_start,
                    width as usize,
                    raw,
                );
            }
            Some(None)
        }
        None => {
            let raw = if whole_element {
                crate::panama::heap_element_bits(&*ctx, view.base, index) & mask
            } else {
                crate::panama::heap_bytes_read(
                    &*ctx,
                    view.base,
                    view.elem_width,
                    byte_start,
                    width as usize,
                )
            };
            let raw = if be { seg_swap_bytes(raw, width) } else { raw };
            Some(Some(seg_decode_value(shape, raw)))
        }
    }
}

/// r23 wave 2g, lane L8
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// correctness, `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE`, default ON): a real
/// `SegmentVarHandle` `get` / `set` whose native road REFUSED (alignment,
/// bounds, read-only, a closed session, a null segment) is run again through
/// the JDK's own static form body -- `VarHandleSegmentAs<X>.get(VarHandle,
/// Object, long)` / `set(VarHandle, Object, long, x)`, what HotSpot's
/// `VarHandleGuards` links to -- so the JDK raises the exception itself with
/// its own frames (`checkSegment`, `checkEnclosingLayout`, `checkAccess`,
/// `checkBounds`, `Preconditions.*`, `ScopedMemoryAccess.*`). The refusal's
/// own exception is dropped. Only the throwing path pays; `None` (keep the
/// native's answer) for any other mode, a variable-offset form, an
/// unreadable form or an argument shape the body does not take. A body that
/// does NOT throw answers what it computed (boxed for a `get`, as the native
/// road answers).
pub(crate) fn segment_vh_raise_through_jdk(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    mode: &str,
) -> Option<MethodCallResult> {
    if !crate::nbflags().ffm_segment_vh_jdk_raise {
        return None;
    }
    // j27 wave 1 (lane FFMVH): never raise through the JDK from inside a
    // raise. A body that resolves back to a refusing segment access (a mode
    // the form class does not declare, see `segment_vh_mode_descriptor`)
    // keeps the native's own exception instead of recursing to a stack
    // overflow.
    if SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()) {
        return None;
    }
    let _in_flight = SegVhRaiseInFlight::enter();
    // JIT r23 wave 6 (lane L8, `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES`):
    // the ordered and read-modify-write modes too, through the same static
    // form bodies (declared on `VarHandleSegmentAs<X>Aligned`).
    let kind = if crate::nbflags().ffm_segment_vh_jdk_raise_all_modes {
        segment_vh_mode_kind(mode)?
    } else {
        match mode {
            "get" => SegModeKind::Get,
            "set" => SegModeKind::Set,
            _ => return None,
        }
    };
    if !matches!(mode, "get" | "set") {
        return segment_vh_raise_mode_through_jdk(ctx, vh, args, mode, kind);
    }
    let is_set = kind == SegModeKind::Set;
    if args.len() != 3 + usize::from(is_set) || !matches!(args.get(2), Some(Value::Long(_))) {
        return None;
    }
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let name = ctx.class_name_of_id(impl_class)?;
    let shape = segment_vh_shape_of_form_class(&name)?;
    if segment_vh_form_is_variable(ctx, vh) {
        return None;
    }
    // The static `get` / `set` are declared on the base form class; the
    // `...Aligned` subclass adds the ordered modes only.
    let owner = name.strip_suffix("Aligned").unwrap_or(&name).to_string();
    let carrier = seg_shape_desc(shape);
    let value_fits = !is_set
        || matches!(
            (carrier, args.get(3)),
            ("B" | "Z" | "S" | "C" | "I", Some(Value::Int(_)))
                | ("J", Some(Value::Long(_)))
                | ("F", Some(Value::Float(_)))
                | ("D", Some(Value::Double(_)))
        );
    if !value_fits {
        return None;
    }
    let desc = if is_set {
        format!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J{carrier})V")
    } else {
        format!("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J){carrier}")
    };
    let out = ctx.invoke_static(&owner, mode, &desc, args);
    Some(match out {
        Ok(Some(v)) if !is_set => Ok(Some(box_value_canonical(ctx, v, carrier))),
        Ok(_) if is_set => Ok(None),
        other => other,
    })
}

thread_local! {
    /// j27 wave 1 (lane FFMVH): a [`segment_vh_raise_through_jdk`] is running
    /// on this thread.
    static SEG_VH_RAISE_IN_FLIGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Holds [`SEG_VH_RAISE_IN_FLIGHT`] for one raise; clears it on drop (also
/// on an unwind).
struct SegVhRaiseInFlight;

impl SegVhRaiseInFlight {
    fn enter() -> Self {
        SEG_VH_RAISE_IN_FLIGHT.with(|c| c.set(true));
        SegVhRaiseInFlight
    }
}

impl Drop for SegVhRaiseInFlight {
    fn drop(&mut self) {
        SEG_VH_RAISE_IN_FLIGHT.with(|c| c.set(false));
    }
}

/// The shape of a `VarHandle` access mode's static form body: what it reads
/// and returns (JDK 25 `VarHandleSegmentAs<X>[Aligned]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegModeKind {
    /// `get`, `getVolatile`, `getAcquire`, `getOpaque`: no value, returns `x`.
    Get,
    /// `set`, `setVolatile`, `setRelease`, `setOpaque`: one value, `void`.
    Set,
    /// `compareAndSet`, `weakCompareAndSet[Plain|Acquire|Release]`: expected
    /// and new value, returns `boolean`.
    Cas,
    /// `compareAndExchange[Acquire|Release]`: expected and new, returns `x`.
    Cae,
    /// `getAndSet*`: one value, returns `x`; any carrier the form serves.
    GetAndSet,
    /// `getAndAdd*`, `getAndBitwise{Or,And,Xor}*`: one value, returns `x`;
    /// integral carriers only (the float forms have no such body).
    GetAndArith,
}

/// [`SegModeKind`] of an access-mode name, or `None` for any other name.
pub(crate) fn segment_vh_mode_kind(mode: &str) -> Option<SegModeKind> {
    Some(match mode {
        "get" | "getVolatile" | "getAcquire" | "getOpaque" => SegModeKind::Get,
        "set" | "setVolatile" | "setRelease" | "setOpaque" => SegModeKind::Set,
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => SegModeKind::Cas,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => {
            SegModeKind::Cae
        }
        "getAndSet" | "getAndSetAcquire" | "getAndSetRelease" => SegModeKind::GetAndSet,
        "getAndAdd" | "getAndAddAcquire" | "getAndAddRelease" | "getAndBitwiseOr"
        | "getAndBitwiseOrAcquire" | "getAndBitwiseOrRelease" | "getAndBitwiseAnd"
        | "getAndBitwiseAndAcquire" | "getAndBitwiseAndRelease" | "getAndBitwiseXor"
        | "getAndBitwiseXorAcquire" | "getAndBitwiseXorRelease" => SegModeKind::GetAndArith,
        _ => return None,
    })
}

/// The static form body's descriptor for `kind` over `carrier` (constant
/// offset form: `(VarHandle, Object, long, values...)`), or `None` when JDK 25
/// declares no such body (`getAndAdd` / `getAndBitwise*` of `F` / `D`).
pub(crate) fn segment_vh_mode_descriptor(kind: SegModeKind, carrier: &str) -> Option<String> {
    const HEAD: &str = "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J";
    // j27 wave 1 (lane FFMVH; page
    // `ffm-segment-varhandle-short-compare-and-set-overflows-the-stack-20261027`):
    // JDK 25 generates the CAS family (`compareAndSet`, `compareAndExchange*`,
    // `weakCompareAndSet*`, `getAndSet*`) only for the `int` / `long` /
    // `float` / `double` segment views (`GensrcVarHandles.gmk`: the byte,
    // short and char views have no `CAS` key). `VarHandleSegmentAsShortsAligned`
    // and `...CharsAligned` exist -- they add the ORDERED modes -- so a
    // descriptor minted for them named a body that is not there, and
    // `invoke_static` resolved the name to the signature-polymorphic
    // `VarHandle.compareAndSet`, whose native refused and raised through here
    // again until the stack overflowed (SIGSEGV where HotSpot throws
    // `UnsupportedOperationException: compareAndSet`).
    let cas_family = matches!(carrier, "I" | "J" | "F" | "D");
    Some(match kind {
        SegModeKind::Get => format!("{HEAD}){carrier}"),
        SegModeKind::Set => format!("{HEAD}{carrier})V"),
        SegModeKind::Cas | SegModeKind::Cae | SegModeKind::GetAndSet if !cas_family => {
            return None;
        }
        SegModeKind::Cas => format!("{HEAD}{carrier}{carrier})Z"),
        SegModeKind::Cae => format!("{HEAD}{carrier}{carrier}){carrier}"),
        SegModeKind::GetAndSet => format!("{HEAD}{carrier}){carrier}"),
        SegModeKind::GetAndArith => {
            if !matches!(carrier, "I" | "J") {
                return None;
            }
            format!("{HEAD}{carrier}){carrier}")
        }
    })
}

/// How many values a [`SegModeKind`] body takes after the offset.
pub(crate) fn segment_vh_mode_values(kind: SegModeKind) -> usize {
    match kind {
        SegModeKind::Get => 0,
        SegModeKind::Set | SegModeKind::GetAndSet | SegModeKind::GetAndArith => 1,
        SegModeKind::Cas | SegModeKind::Cae => 2,
    }
}

/// JIT r23 wave 6 (lane L8; `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES`,
/// correctness, default ON): [`segment_vh_raise_through_jdk`] for an ORDERED
/// or read-modify-write mode of a constant-offset `SegmentVarHandle`. Those
/// bodies exist only on the aligned form class
/// (`VarHandleSegmentAs<X>Aligned`, JDK 25 `javap`), so a handle whose form
/// is not aligned answers `None` and keeps the native's
/// `UnsupportedOperationException`, as does a mode the carrier has no body
/// for. Measured on n23-w5 (`C:\craton\n23p\w6L8\R23W6SegModes.java`): every
/// such failure carried no JDK frame at all, where HotSpot shows
/// `checkEnclosingLayout < SegmentVarHandle.checkSegment <
/// VarHandleSegmentAsIntsAligned.<mode>` x2 (plus the bounds chain).
pub(crate) fn segment_vh_raise_mode_through_jdk(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    mode: &str,
    kind: SegModeKind,
) -> Option<MethodCallResult> {
    let values = segment_vh_mode_values(kind);
    if args.len() != 3 + values || !matches!(args.get(2), Some(Value::Long(_))) {
        return None;
    }
    let Value::Object(Some(vform)) =
        real_object_field(ctx, vh, "java/lang/invoke/VarHandle", "vform")
    else {
        return None;
    };
    let Value::Object(Some(impl_mirror)) =
        real_object_field(ctx, vform, "java/lang/invoke/VarForm", "implClass")
    else {
        return None;
    };
    let impl_class = ctx.class_id_from_mirror(impl_mirror)?;
    let owner = ctx.class_name_of_id(impl_class)?;
    // j27 wave 1 (lane FFMVH): the bytes / booleans forms have no `Aligned`
    // twin and declare their ordered modes themselves
    // (`segment_vh_form_class_is_aligned`), so a refused `getVolatile` on a
    // byte handle raises through its body too; their CAS family has no body
    // (`segment_vh_mode_descriptor` answers `None`).
    if !segment_vh_form_class_is_aligned(&owner) {
        return None;
    }
    let shape = segment_vh_shape_of_form_class(&owner)?;
    if segment_vh_form_is_variable(ctx, vh) {
        return None;
    }
    let carrier = seg_shape_desc(shape);
    let desc = segment_vh_mode_descriptor(kind, carrier)?;
    let value_fits = args.get(3..).unwrap_or(&[]).iter().all(|v| {
        matches!(
            (carrier, v),
            ("B" | "Z" | "S" | "C" | "I", Value::Int(_))
                | ("J", Value::Long(_))
                | ("F", Value::Float(_))
                | ("D", Value::Double(_))
        )
    });
    if !value_fits {
        return None;
    }
    let out = ctx.invoke_static(&owner, mode, &desc, args);
    Some(match (out, kind) {
        (Ok(_), SegModeKind::Set) => Ok(None),
        (Ok(Some(v)), SegModeKind::Cas) => Ok(Some(box_value_canonical(ctx, v, "Z"))),
        (Ok(Some(v)), _) => Ok(Some(box_value_canonical(ctx, v, carrier))),
        (other, _) => other,
    })
}

#[cfg(test)]
mod r23w6_segment_vh_mode_tests {
    use super::*;

    #[test]
    fn mode_bodies_follow_the_jdk_25_form_classes() {
        let h = "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J";
        assert_eq!(segment_vh_mode_kind("getAcquire"), Some(SegModeKind::Get));
        assert_eq!(segment_vh_mode_kind("setRelease"), Some(SegModeKind::Set));
        assert_eq!(segment_vh_mode_kind("weakCompareAndSetPlain"), Some(SegModeKind::Cas));
        assert_eq!(segment_vh_mode_kind("compareAndExchangeAcquire"), Some(SegModeKind::Cae));
        assert_eq!(segment_vh_mode_kind("getAndSetRelease"), Some(SegModeKind::GetAndSet));
        assert_eq!(segment_vh_mode_kind("getAndBitwiseXorAcquire"), Some(SegModeKind::GetAndArith));
        assert_eq!(segment_vh_mode_kind("toMethodHandle"), None);
        assert_eq!(
            segment_vh_mode_descriptor(SegModeKind::Get, "I").as_deref(),
            Some(&*format!("{h})I"))
        );
        assert_eq!(
            segment_vh_mode_descriptor(SegModeKind::Cas, "J").as_deref(),
            Some(&*format!("{h}JJ)Z"))
        );
        assert_eq!(
            segment_vh_mode_descriptor(SegModeKind::Cae, "I").as_deref(),
            Some(&*format!("{h}II)I"))
        );
        assert_eq!(
            segment_vh_mode_descriptor(SegModeKind::GetAndSet, "D").as_deref(),
            Some(&*format!("{h}D)D"))
        );
        assert_eq!(segment_vh_mode_descriptor(SegModeKind::GetAndArith, "F"), None);
        // j27 wave 1 (lane FFMVH): the short / char aligned forms declare
        // the ordered modes only -- no CAS family body to raise through.
        for carrier in ["S", "C", "B", "Z"] {
            for kind in [SegModeKind::Cas, SegModeKind::Cae, SegModeKind::GetAndSet, SegModeKind::GetAndArith] {
                assert_eq!(segment_vh_mode_descriptor(kind, carrier), None, "{kind:?} {carrier}");
            }
            assert!(segment_vh_mode_descriptor(SegModeKind::Get, carrier).is_some());
            assert!(segment_vh_mode_descriptor(SegModeKind::Set, carrier).is_some());
        }
        assert_eq!(
            segment_vh_mode_descriptor(SegModeKind::GetAndSet, "F").as_deref(),
            Some(&*format!("{h}F)F"))
        );
        assert_eq!(segment_vh_mode_values(SegModeKind::Cae), 2);
        assert_eq!(segment_vh_mode_values(SegModeKind::Get), 0);
    }

    /// j27 wave 1 (lane FFMVH): the raise's re-entrancy flag is held for
    /// exactly one raise and cleared on drop.
    #[test]
    fn the_raise_in_flight_flag_is_scoped() {
        assert!(!SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
        {
            let _held = SegVhRaiseInFlight::enter();
            assert!(SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
        }
        assert!(!SEG_VH_RAISE_IN_FLIGHT.with(|c| c.get()));
    }
}

/// r23 wave 2d, lane L8: the HEAP twin of the compiled FFM element door
/// ([`segment_layout_facts`] serves only a native segment): one
/// `MemorySegment.getAtIndex/setAtIndex` (or the offset `get/set`, which the
/// emitter has turned into an index) of a real heap segment through `layout`,
/// at a site whose carrier is `site_kind`. `write`: the value's bits (the
/// carrier's low bits). Answers the read's bits (sign- / zero-extended as the
/// native door's `real_seg_serve` answers them; a float's or double's raw
/// bits), `Some(0)` for a served store, or `None` to decline -- the site then
/// runs its unchanged dispatch, which raises whatever it always raised. The
/// admission is [`segment_layout_facts`]'s (a plain access through a built
/// layout handle); the access is [`segment_vh_lean_heap`]'s. Allocates nothing.
pub fn segment_layout_heap_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    index: i64,
    write: Option<i64>,
) -> Option<i64> {
    segment_layout_heap_access_with(ctx, seg, layout, site_kind, write, |width| {
        index.checked_mul(width)
    })
}

/// JIT r23 wave 4 (lane L8, `CRATONVM_JIT_FFM_UNALIGNED_OFFSET_ACCESS`): the
/// byte-offset form of [`segment_layout_heap_access`], for a `get/set(layout,
/// long offset)` site whose offset is not a multiple of the width (an
/// unaligned layout, `withByteAlignment(1)` / `JAVA_INT_UNALIGNED`). The
/// checks are the same ones in the same order -- [`segment_vh_lean_heap`]
/// tests bounds, read-only and the layout's alignment against the byte
/// offset -- so a misaligned access through an ALIGNED layout still declines
/// to the JDK body and its `IllegalArgumentException`.
pub fn segment_layout_heap_access_bytes(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    byte_offset: i64,
    write: Option<i64>,
) -> Option<i64> {
    segment_layout_heap_access_with(ctx, seg, layout, site_kind, write, |_| Some(byte_offset))
}

pub(crate) fn segment_layout_heap_access_with(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    site_kind: u8,
    write: Option<i64>,
    to_base: impl FnOnce(i64) -> Option<i64>,
) -> Option<i64> {
    if !crate::nbflags().ffm_segment_vh_lean_heap
        || !segment_vh_lean_enabled()
        || !segment_vh_checks()
        || !segment_vh_form_enabled()
    {
        return None;
    }
    let plan = seg_layout_plan(ctx, layout)?;
    let width = seg_shape_width(plan.shape);
    if plan.variable
        || plan.vh_offset != 0
        || plan.enclosing_align > width
        || plan.enclosing_size != width
        || seg_shape_site_kind(plan.shape) != site_kind
    {
        return None;
    }
    let base = to_base(width)?;
    let raw_in = write.map(|bits| {
        let mask = if width >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * width)) - 1
        };
        // Cast: the carrier's bits, reinterpreted.
        let raw = (bits as u64) & mask;
        if plan.be {
            seg_swap_bytes(raw, width)
        } else {
            raw
        }
    });
    match segment_vh_lean_heap(ctx, &plan, seg, base, raw_in)? {
        None => Some(0),
        Some(Value::Int(v)) => Some(i64::from(v)),
        Some(Value::Long(v)) => Some(v),
        Some(Value::Float(f)) => Some(i64::from(f.to_bits())),
        // Cast: the raw bits.
        Some(Value::Double(d)) => Some(d.to_bits() as i64),
        Some(_) => None,
    }
}

/// The native roads' face of [`segment_vh_lean`]: the boxed answer of a `get`
/// (`Some(Some(value))`), a served `set` (`Some(None)`), or a decline (`None`).
/// `args` is `[vh, seg, base]` (`get`) or `[vh, seg, base, value]` (`set`).
/// `site_kind`: the call site's declared return (`B S C I J F D Z`), or `0`
/// when unknown or for a `set` (r23 wave 2c, lane L8).
pub(crate) fn segment_vh_lean_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    write: bool,
    site_kind: u8,
) -> Option<Option<Value>> {
    let (Some(Value::Object(Some(vh))), Some(Value::Object(Some(seg))), Some(Value::Long(base))) = (
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) else {
        return None;
    };
    if args.len() != 3 + usize::from(write) {
        return None;
    }
    let value = if write { Some(args[3]) } else { None };
    let (shape, out) = segment_vh_lean(ctx, vh, seg, base, value, site_kind, None)?;
    Some(out.map(|v| box_value_canonical(ctx, v, seg_shape_desc(shape))))
}

/// The funnel's raw-word face of [`segment_vh_lean`] for a `get` (the bits a
/// compiled caller's register takes), or `None` to decline.
pub fn segment_vh_lean_get_bits(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    site_kind: u8,
    vh_hash: i32,
    seg_class: u32,
) -> Option<i64> {
    if site_kind == 0 {
        return None;
    }
    let keys = Some((vh_hash, ClassId::new(seg_class)));
    match segment_vh_lean(ctx, vh, seg, base, None, site_kind, keys)?.1 {
        Some(Value::Int(v)) => Some(i64::from(v)),
        Some(Value::Long(v)) => Some(v),
        Some(Value::Float(f)) => Some(i64::from(f.to_bits())),
        Some(Value::Double(d)) => Some(d.to_bits() as i64),
        _ => None,
    }
}

/// The funnel's raw-word face of [`segment_vh_lean`] for a `set`; `true` when
/// the store was made.
pub fn segment_vh_lean_set_bits(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    base: i64,
    site_kind: u8,
    value_bits: i64,
    vh_hash: i32,
    seg_class: u32,
) -> bool {
    let value = match site_kind {
        b'B' | b'Z' | b'S' | b'C' | b'I' => Value::Int(value_bits as i32),
        b'J' => Value::Long(value_bits),
        b'F' => Value::Float(f32::from_bits(value_bits as u32)),
        b'D' => Value::Double(f64::from_bits(value_bits as u64)),
        _ => return false,
    };
    let keys = Some((vh_hash, ClassId::new(seg_class)));
    segment_vh_lean(ctx, vh, seg, base, Some(value), site_kind, keys).is_some()
}

/// SBR4 lane segacc: the plan of a real `ValueLayout`'s access handle, filed
/// per thread by `(vm, layout address, ffm epoch, handle-table generation)`.
/// The layout's own `handle` field (`AbstractValueLayout.accessHandle()`'s
/// memo) names the `SegmentVarHandle` whose plan [`seg_vh_plan`] decodes, so a
/// compiled `MemorySegment.get(layout, off)` reaches the same decisions as a
/// `VarHandle.get(segment, off)` without the interface call, the handle
/// fetch and the funnel. A layout whose handle is not built yet (the first
/// access) answers `None` and the JDK's own bytecode builds it.
#[derive(Clone, Copy)]
pub(crate) struct SegLayoutFacts {
    pub(crate) vm: usize,
    pub(crate) addr: usize,
    pub(crate) epoch: u64,
    pub(crate) generation: u64,
    pub(crate) plan: SegVhPlan,
}

pub(crate) const SEG_LAYOUT_FACT_WAYS: usize = 8;

thread_local! {
    pub(crate) static SEG_LAYOUT_FACTS: std::cell::Cell<[Option<SegLayoutFacts>; SEG_LAYOUT_FACT_WAYS]> =
        const { std::cell::Cell::new([None; SEG_LAYOUT_FACT_WAYS]) };
    pub(crate) static SEG_LAYOUT_FACTS_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn seg_layout_plan(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Option<SegVhPlan> {
    let vm = ctx.vm_identity();
    let addr = layout.as_ptr() as usize;
    let epoch = crate::ffm_fast::epoch();
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let ways = SEG_LAYOUT_FACTS.with(std::cell::Cell::get);
    for way in ways.iter().flatten() {
        if way.addr == addr && way.epoch == epoch && way.vm == vm && way.generation == generation {
            return Some(way.plan);
        }
    }
    let Value::Object(Some(vh)) = real_object_field(
        ctx,
        layout,
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
        "handle",
    ) else {
        return None;
    };
    let plan = seg_vh_plan(ctx, vh)?;
    // `seg_vh_plan` can move the generation (a table change); file under the
    // value the next lookup will read.
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = SEG_LAYOUT_FACTS_NEXT.with(|c| {
        let i = c.get();
        c.set((i + 1) % SEG_LAYOUT_FACT_WAYS);
        i
    });
    let mut updated = ways;
    updated[slot] = Some(SegLayoutFacts {
        vm,
        addr,
        epoch,
        generation,
        plan,
    });
    SEG_LAYOUT_FACTS.with(|c| c.set(updated));
    Some(plan)
}

/// Everything a compiled `MemorySegment.get/set(layout, offset)` door needs to
/// serve one REAL native segment against one real layout WITHOUT a native
/// context: the segment's final fields, the layout's plan, the session's `state`
/// slot (the one thing a close changes, read on every access by the caller) and
/// whether this thread may touch the session (its `owner` is final). Valid for
/// one `(segment address, layout address, ffm epoch)` -- the argument
/// [`SegLeanFacts`] and [`SegLayoutFacts`] rest on.
#[derive(Clone, Copy)]
pub struct SegRealFacts {
    /// The segment's `length`.
    pub size: i64,
    /// The segment's `min`: an arena handle when `handle`, else a plain pointer.
    pub min: i64,
    pub read_only: bool,
    /// Address of the session object (its `state` slot is `state_slot`).
    pub scope: usize,
    pub state_slot: usize,
    /// The session is shared / implicit / global (no `owner`), or owned by the
    /// thread that built these facts (the facts are filed per thread).
    pub owner_ok: bool,
    pub handle: bool,
    /// The access width in bytes.
    pub width: i64,
    /// The layout's alignment (at most `width`).
    pub align: i64,
    pub be: bool,
}

/// [`SegRealFacts`] for `seg` accessed through `layout` at a site whose
/// descriptor names `site_kind` (`B S C I J F D`), or `None` when this pair is
/// not a plain real-native-segment access (a heap segment, a carrier, a
/// layout whose handle is not built yet, a path / variable-offset handle, a
/// layout aligned wider than its size, a read of another carrier): the JDK's
/// own bytecode then serves it and raises whatever it always raised.
pub fn segment_layout_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    seg_class: u32,
    site_kind: u8,
) -> Option<SegRealFacts> {
    if !segment_vh_lean_enabled() || !segment_vh_checks() || !segment_vh_form_enabled() {
        return None;
    }
    let plan = seg_layout_plan(ctx, layout)?;
    let width = seg_shape_width(plan.shape);
    // `getAtIndex` refuses a layout aligned wider than its size, and a
    // non-trivial `vh_offset` is a path handle: neither is a plain access.
    if plan.variable
        || plan.vh_offset != 0
        || plan.enclosing_align > width
        || plan.enclosing_size != width
        || seg_shape_site_kind(plan.shape) != site_kind
    {
        return None;
    }
    let Some(SegLeanFacts {
        size,
        read_only,
        min,
        scope,
        session,
        ..
    }) = seg_lean_facts(ctx, seg, Some(ClassId::new(seg_class)))
    else {
        return segment_layout_heap_facts(ctx, seg, &plan, width);
    };
    // An arena handle goes through the arena store; a plain non-null pointer (a
    // mapped file, an `ofAddress` window) is dereferenced; anything else
    // declines (as `segment_vh_lean_planned` does).
    let handle = crate::panama_libffi::is_arena_handle(min);
    if !handle && (min <= 0 || crate::unsafe_arena_addr_is_tagged(min)) {
        return None;
    }
    let mut owner = [Value::Int(0); 1];
    ctx.get_fields_typed(scope, &[(session.owner, b'L')], &mut owner);
    let owner_ok = match owner[0] {
        Value::Object(None) => true,
        // zs s1/nd: never mint the mirror here (see `segment_vh_lean_planned`);
        // `real_seg_fill` calls this from a JIT helper.
        Value::Object(Some(o)) => ctx.current_thread_object_if_any() == Some(o),
        _ => return None,
    };
    Some(SegRealFacts {
        size,
        min,
        read_only,
        scope: scope.as_ptr() as usize,
        state_slot: session.state,
        owner_ok,
        handle,
        width,
        align: plan.enclosing_align,
        be: plan.be,
    })
}

/// r23 wave 3, lane L8 (`CRATONVM_FFM_SEGMENT_LAYOUT_HEAP_FACTS`, perf,
/// default ON, `=0` kills): [`segment_layout_facts`] for a real HEAP segment
/// (`MemorySegment.ofArray`): the same [`SegRealFacts`] a native segment
/// gets, with `min` the RAW address of the segment's first byte inside its
/// backing array (`array + HEADER_SIZE + start`), so the compiled door's
/// cached road (`vm::jit::helpers::real_seg_serve`) reads and writes it as it
/// reads a native block: one unaligned load or store. Sound for the facts'
/// lifetime -- one `(segment, layout, ffm epoch)`, the epoch bumped at the
/// entry of every collection, the only thing that moves an array (every
/// relocator runs with the mutators parked) -- and byte-exact: a primitive
/// array's payload is its elements little-endian, back to back, which is the
/// JDK's own heap-segment byte order (`panama::heap_bytes_read`'s measured
/// rule). Declines a layout aligned wider than the backing element (the heap
/// segment's `maxAlignMask` refusal, which the JDK raises) and everything
/// [`seg_heap_facts`] declines.
pub(crate) fn segment_layout_heap_facts(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    plan: &SegVhPlan,
    width: i64,
) -> Option<SegRealFacts> {
    if !crate::nbflags().ffm_segment_vh_lean_heap
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_FFM_SEGMENT_LAYOUT_HEAP_FACTS",
        )
    {
        return None;
    }
    let SegHeapFacts {
        view,
        scope,
        session,
        ..
    } = seg_heap_facts(ctx, seg)?;
    // Cast: an element is 1..=8 bytes.
    if plan.enclosing_align > view.elem_width as i64 {
        return None;
    }
    // Cast: an object address, and the data offset inside it.
    let min = i64::try_from(view.base.as_ptr() as usize)
        .ok()?
        .checked_add(cratonvm_types::HEADER_SIZE as i64)?
        .checked_add(view.start)?;
    if min <= 0 {
        return None;
    }
    let mut owner = [Value::Int(0); 1];
    ctx.get_fields_typed(scope, &[(session.owner, b'L')], &mut owner);
    let owner_ok = match owner[0] {
        Value::Object(None) => true,
        Value::Object(Some(o)) => ctx.current_thread_object_if_any() == Some(o),
        _ => return None,
    };
    Some(SegRealFacts {
        size: view.size(),
        min,
        read_only: view.read_only(),
        scope: scope.as_ptr() as usize,
        state_slot: session.state,
        owner_ok,
        handle: false,
        width,
        align: plan.enclosing_align,
        be: plan.be,
    })
}

/// [`seg_vh_plan`]'s memo, hit only, for a caller that already holds the
/// handle's identity hash (the funnel's receiver memo): no hash re-derivation
/// and no miss line.
pub(crate) fn seg_vh_plan_hit(vm: usize, hash: i32, addr: usize) -> Option<SegVhPlan> {
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    SEG_VH_PLAN_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if line.vm == vm && line.key == hash && line.addr == addr && line.generation == generation {
            line.value
        } else {
            None
        }
    })
}

pub(crate) fn seg_encode_value(value: &Value, width: i64) -> u64 {
    let raw: u64 = match value {
        Value::Long(v) => *v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => *v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    match width {
        1 => raw & 0xff,
        2 => raw & 0xffff,
        4 => raw & 0xffff_ffff,
        _ => raw,
    }
}

/// Resolve a `MemorySegment` coordinate's raw access point by reading the
/// real implementation object's own fields directly (NOT by calling back
/// into its `unsafeGetBase()`/`unsafeGetOffset()`/`byteSize()` methods — a
/// reentrant native→bytecode call from inside a native invoked by
/// JIT-compiled code crashed here; see the fix commit for the observed
/// fault). Mirrors `sun.misc.Unsafe`'s dual addressing: a null base means
/// `offset` is an absolute native address; a non-null base means `offset` is
/// a byte offset into that (possibly heap/GC-managed) array object.
///
/// Field layout (real JDK `jdk.internal.foreign` classes, JEP 454):
/// `AbstractMemorySegmentImpl.length` (byteSize for every concrete subtype);
/// `NativeMemorySegmentImpl.min` (absolute address; `MappedMemorySegmentImpl`
/// extends it and inherits the field); `HeapMemorySegmentImpl.base`/`.offset`
/// (array object + Unsafe-style byte offset into it).
pub(crate) fn segment_raw_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh_offset: i64,
    coord_offset: i64,
    width: i64,
) -> Result<(Option<ObjectRef>, i64), MethodCallFailed> {
    const ABSTRACT_SEGMENT: &str = "jdk/internal/foreign/AbstractMemorySegmentImpl";
    const NATIVE_SEGMENT: &str = "jdk/internal/foreign/NativeMemorySegmentImpl";
    const HEAP_SEGMENT: &str = "jdk/internal/foreign/HeapMemorySegmentImpl";

    let class_name = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(seg))
        .unwrap_or_else(|| Arc::from(""));
    // Round 12 wave 7 (lane ffm4): a CratonVM-minted carrier
    // (`cratonvm/internal/foreign/MemorySegmentImpl`) has none of the JDK
    // fields below -- resolving `NativeMemorySegmentImpl.min` or
    // `AbstractMemorySegmentImpl.length` against the JDK class and reading
    // that slot index off the carrier read its `readOnly` flag as the address
    // and its pointer as the size. It goes through the readers that know both
    // models, as the layout VarHandles do.
    if segment_vh_checks() && !class_name.starts_with("jdk/internal/foreign/") {
        let size = crate::panama_libffi::segment_byte_size(ctx, seg);
        let (base, base_off) = match crate::panama::heap_segment_view(ctx, seg) {
            Some(view) => (Some(view.base), view.start),
            None => (None, crate::panama_libffi::segment_address(ctx, seg)),
        };
        return segment_raw_bounds(base, base_off, size, vh_offset, coord_offset, width)
            .ok_or_else(|| segment_raw_refusal(ctx, seg, coord_offset, width));
    }
    let (base, base_off) = if class_name.contains("Heap") {
        // `HeapMemorySegmentImpl.offset` is `Unsafe`-style and must be
        // un-biased before use — see `panama_libffi::heap_segment_byte_start`,
        // the one shared reader for this rule (NOM F35-2; it used to be a
        // second, independent `const ABASE: i64 = 16` here). Skipping it
        // lands every heap-segment access 16 bytes past its intended target
        // (silently corrupting data when still in-bounds, throwing when it
        // overflows the backing array).
        let base_idx = ctx.resolve_field_index(HEAP_SEGMENT, "base");
        let off_idx = ctx.resolve_field_index(HEAP_SEGMENT, "offset");
        let base = match (base_idx, base_idx.map(|i| ctx.get_field(seg, i))) {
            (Some(_), Some(Value::Object(b))) => b,
            _ => None,
        };
        let off = match off_idx {
            Some(i) => match ctx.get_field(seg, i) {
                Value::Long(n) => crate::panama_libffi::heap_segment_byte_start(n),
                _ => 0,
            },
            None => 0,
        };
        (base, off)
    } else {
        let off = match ctx.resolve_field_index(NATIVE_SEGMENT, "min") {
            Some(i) => match ctx.get_field(seg, i) {
                Value::Long(n) => n,
                _ => 0,
            },
            None => 0,
        };
        (None, off)
    };
    let size = match ctx.resolve_field_index(ABSTRACT_SEGMENT, "length") {
        Some(i) => match ctx.get_field(seg, i) {
            Value::Long(n) => n,
            _ => 0,
        },
        None => 0,
    };
    segment_raw_bounds(base, base_off, size, vh_offset, coord_offset, width)
        .ok_or_else(|| segment_raw_refusal(ctx, seg, coord_offset, width))
}

/// `CRATONVM_FFM_SEGMENT_VH_CHECKS` (default on): a real `SegmentVarHandle`
/// access checks what the JDK's `checkSegment` / `offset` and the session
/// check do -- read-only (`IllegalArgumentException`), bounds
/// (`IndexOutOfBoundsException`, was `IllegalStateException`), the absolute
/// address against the layout's alignment on a native segment
/// (`IllegalArgumentException: Misaligned access`), and the scope, under an
/// access window -- and decodes a CratonVM carrier through the readers that
/// know its slots. Off restores round 12 wave 6's unchecked access (round 12
/// wave 7, lane ffm4).
pub(crate) fn segment_vh_checks() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_SEGMENT_VH_CHECKS")
    })
}

/// Bounds-check a `SegmentVarHandle` access of `width` bytes at `vh_offset +
/// coord_offset` in a segment of `size` bytes whose data starts at `base_off`
/// (an absolute address, or a byte offset into `base`'s data), and answer
/// `(base, address)`, or `None` when it does not fit ([`segment_raw_refusal`]).
pub(crate) fn segment_raw_bounds(
    base: Option<ObjectRef>,
    base_off: i64,
    size: i64,
    vh_offset: i64,
    coord_offset: i64,
    width: i64,
) -> Option<(Option<ObjectRef>, i64)> {
    let total_offset = vh_offset.checked_add(coord_offset)?;
    let end = total_offset.checked_add(width);
    if total_offset < 0 || end.map_or(true, |e| e > size) {
        return None;
    }
    let addr = base_off.checked_add(total_offset)?;
    Some((base, addr))
}

/// [`segment_raw_bounds`]' refusal: `outOfBoundException(coord_offset,
/// width)`'s text naming the segment (j27 wave 4, lane FFMMSG: it said
/// `offset=.., width=.., size=..`). HotSpot's length is the enclosing
/// layout's size, which is `width` for a value-layout handle (`vh_offset` 0).
/// Builds the text without running Java: the caller holds an access window.
fn segment_raw_refusal(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    coord_offset: i64,
    width: i64,
) -> MethodCallFailed {
    let segment = crate::panama::segment_to_string(ctx, seg);
    let message = crate::panama::out_of_bounds_message(&segment, coord_offset, width);
    if segment_vh_checks() {
        // The JDK's class for an access past the end (`checkBounds`).
        MethodCallFailed::from(RuntimeError::IndexOutOfBoundsException {
            message: Some(message),
        })
    } else {
        MethodCallFailed::from(RuntimeError::IllegalStateException { message })
    }
}

/// The scope and alignment half of a `SegmentVarHandle` access (round 12 wave
/// 7, lane ffm4; `r12w6-ffm3-segment-varhandle-has-no-scope-check`). The
/// caller holds an `ffm_fast::AccessWindow` from before this to after its
/// last touch of the block, so a shared close either waits for the access or
/// is what the scope check reads. `seg` is forwarded: a real session's check
/// can run Java.
pub(crate) fn segment_vh_check_scope(
    ctx: &mut dyn NativeContext,
    seg: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !segment_vh_checks() {
        return Ok(());
    }
    crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, seg)
}

/// Is a native access at absolute `addr` misaligned for `align` -- the rule
/// `isAlignedForElement` applies (a native segment's `maxAlignMask` is 0)? A
/// synthetic `Unsafe` arena handle is not an address and is not checked;
/// neither is a heap segment (its rule involves the array's element
/// alignment and is left to the bounds check). The refusal is
/// `panama::native_misaligned_refusal` (j27 wave 4, lane FFMMSG: HotSpot's
/// `checkEnclosingLayout` text; it said `Misaligned access at address: N`).
pub(crate) fn segment_vh_misaligned(align: i64, base: Option<ObjectRef>, addr: i64) -> bool {
    segment_vh_checks()
        && base.is_none()
        && !crate::unsafe_arena_contains(addr)
        && crate::panama::native_misaligned(addr as usize, align.max(1) as u64)
}

/// j27 wave 6 (lane T1LOOPFFM): the legacy `segment_vh_get` / `_set` road's
/// `SegmentVarHandle.checkSegment` -> `checkEnclosingLayout(base, enclosing)`,
/// run as JDK 25 runs it: after the read-only test, BEFORE the session check
/// and before the access window opens (its refusals can run Java). So, as on
/// HotSpot, the WHOLE enclosing layout must fit at `base` (a member handle
/// was bounded by its own field, and named its numbers), `base` must meet the
/// enclosing alignment (a member handle tested `base + offset`), a closed
/// session does not hide either refusal, and the misaligned text carries its
/// `(of ..)` clause (`enclosing` is still the reference read at entry). The
/// one checker the form road uses, [`segment_vh_check_enclosing`]. Asked
/// under `CRATONVM_FFM_BOUNDS_BEFORE_SCOPE` (`panama::ffm_bounds_before_scope`,
/// HotSpot's order, default ON); off, or with an unreadable layout size, the
/// road keeps its checks after the scope, as before.
fn segment_vh_enclosing_before_scope(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    vh: ObjectRef,
    enclosing: ObjectRef,
    align: i64,
    base: i64,
) -> Result<(), MethodCallFailed> {
    if !segment_vh_checks() || !crate::panama::ffm_bounds_before_scope() {
        return Ok(());
    }
    let Ok(size) = crate::panama_libffi::layout_total_size(ctx, enclosing) else {
        return Ok(());
    };
    // Cast: a layout's byte size is a Java `long`.
    segment_vh_check_enclosing(ctx, seg, vh, size as i64, align, base)
}

/// Heap-segment access for `SegmentVarHandle`. `addr` is the Unsafe-style
/// absolute byte offset into `base` as returned by `unsafeGetOffset()`; real
/// `byte[]` segments carry `ARRAY_BYTE_BASE_OFFSET` baked into that value so
/// it lands directly on `base`'s own element indices.
///
/// NOM F35-3 (was a real defect, not just a comment): this used to read `base`
/// one BYTE per `get_array_element` call regardless of its actual element
/// type, which is right only for `byte[]`. A `VarHandle` over an
/// `int[]`-backed `MemorySegment` (`MemorySegment.ofArray(new int[8])`) read
/// element `addr..addr+width` of a `byte[]` view that does not exist, i.e. it
/// read INT elements at BYTE granularity — wrong data, not a crash, so nothing
/// short of comparing against the oracle would show it. `heap_bytes_read` is
/// `panama.rs`'s own general reader (`heap_segment_read`'s core), shared here
/// rather than re-derived, per that file's own doc comment asking for exactly
/// this ("make those two `pub(crate)` ... or move both into one module").
pub(crate) fn segment_heap_get(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    addr: i64,
    width: i64,
) -> Option<u64> {
    let elem_type = ctx.heap_element_type_of(base);
    let elem_width = crate::panama::heap_element_width(elem_type)? as i64;
    let byte_len = (ctx.array_length(base) as i64).checked_mul(elem_width)?;
    if addr < 0 || addr.checked_add(width)? > byte_len {
        return None;
    }
    // JIT round 20 wave 8 (lane huntFFM): one access on the flat payload.
    if let Some(raw) = segment_heap_payload_read(ctx, base, addr as usize, width as usize) {
        return Some(raw);
    }
    Some(crate::panama::heap_bytes_read(
        ctx,
        base,
        elem_width as usize,
        addr,
        width as usize,
    ))
}

/// JIT round 20 wave 8 (lane huntFFM): a heap-segment access is ONE access of
/// exactly its own bytes, as JDK 25's `Unsafe` access on the base array is.
/// The element-wise fallback ([`crate::panama::heap_bytes_read`] /
/// [`crate::panama::heap_bytes_write`]) reads, and for a store
/// read-modify-writes, the whole ELEMENT once per byte: a `JAVA_BYTE` /
/// `JAVA_SHORT` store into an `int[]` / `long[]` segment rewrote its
/// neighbours' bytes (a concurrent store to another byte of the same element
/// was lost), and an aligned `JAVA_INT` / `JAVA_LONG` read or store was torn
/// into byte steps where the JDK promises single-copy atomicity for an aligned
/// plain access (probe `R20HuntFFMHeapTear`: HotSpot `lost.* 0`, `torn.* 0`).
///
/// An aligned 2/4/8-byte access is one hardware atomic on the payload
/// (`NativeContext::primitive_array_atomic_update`, `f` answering `None`: a
/// load); anything else one byte copy of exactly `width` bytes
/// (`read_primitive_array_bytes`). `None`: no flat payload (the caller's
/// element-wise reading stands). `off` is the payload byte offset the caller
/// bounds-checked.
pub(crate) fn segment_heap_payload_read(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    off: usize,
    width: usize,
) -> Option<u64> {
    if width == 0 || width > 8 {
        return None;
    }
    if matches!(width, 2 | 4 | 8) && off % width == 0 {
        if let Some((cur, _)) = ctx.primitive_array_atomic_update(base, off, width, &|_| None) {
            return Some(cur);
        }
    }
    let mut b = [0u8; 8];
    (ctx.read_primitive_array_bytes(base, off, &mut b[..width]) == width)
        .then(|| u64::from_le_bytes(b))
}

/// The store half of [`segment_heap_payload_read`]: the low `width` bytes of
/// `raw` (little-endian, the payload's order) as one access. `false`: no flat
/// payload, nothing written (the caller's element-wise store stands).
pub(crate) fn segment_heap_payload_write(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    off: usize,
    width: usize,
    raw: u64,
) -> bool {
    if width == 0 || width > 8 {
        return false;
    }
    let raw = if width == 8 {
        raw
    } else {
        raw & ((1u64 << (width * 8)) - 1)
    };
    if matches!(width, 2 | 4 | 8)
        && off % width == 0
        && ctx
            .primitive_array_atomic_update(base, off, width, &|_| Some(raw))
            .is_some()
    {
        return true;
    }
    ctx.write_primitive_array_bytes(base, off, &raw.to_le_bytes()[..width])
}

/// Write-side twin of [`segment_heap_get`] — see its doc comment (NOM F35-3).
pub(crate) fn segment_heap_set(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    addr: i64,
    width: i64,
    raw: u64,
) -> bool {
    let elem_type = ctx.heap_element_type_of(base);
    let Some(elem_width) = crate::panama::heap_element_width(elem_type) else {
        return false;
    };
    let elem_width = elem_width as i64;
    let Some(byte_len) = (ctx.array_length(base) as i64).checked_mul(elem_width) else {
        return false;
    };
    match addr.checked_add(width) {
        Some(end) if addr >= 0 && end <= byte_len => {}
        _ => return false,
    }
    // JIT round 20 wave 8 (lane huntFFM): see `segment_heap_payload_read`.
    if segment_heap_payload_write(ctx, base, addr as usize, width as usize, raw) {
        return true;
    }
    crate::panama::heap_bytes_write(
        ctx,
        base,
        elem_type,
        elem_width as usize,
        addr,
        width as usize,
        raw,
    );
    true
}

/// Read `width` bytes at a null-base (off-heap) `MemorySegment` address.
///
/// `addr` is NOT necessarily a real OS pointer: `Unsafe.allocateMemory` (which
/// real `Arena.ofConfined()/allocate()` bytecode calls into) hands out
/// synthetic tagged arena handles (see `unsafe_arena_contains` doc), not real
/// pointers — dereferencing one directly segfaults. Real `MMapDirectory`
/// segments (backed by an actual OS mapping) DO carry a real pointer, so both
/// cases must be handled, exactly mirroring `vm_exec.rs`'s
/// `copy_from_native_memory`.
pub(crate) fn segment_native_get(addr: i64, width: i64) -> Result<u64, MethodCallFailed> {
    if crate::unsafe_arena_contains(addr) {
        let mut buf = [0u8; 8];
        if !crate::unsafe_arena_copy_out(addr, &mut buf[..width as usize]) {
            return Err(RuntimeError::IllegalStateException {
                message: "Out of bound access on off-heap MemorySegment arena handle".into(),
            }
            .into());
        }
        return Ok(u64::from_le_bytes(buf));
    }
    let ptr = addr as usize as *const u8;
    if ptr.is_null() {
        return Err(RuntimeError::IllegalStateException {
            message: "MemorySegment access via null address".into(),
        }
        .into());
    }
    // SAFETY: bounds-checked against the segment's declared size and
    // overflow-checked address arithmetic in `segment_raw_access`; not a
    // synthetic arena handle (checked above), so this is a real OS pointer.
    // Unaligned reads (round 12 wave 7, lane ffm4): an `*_UNALIGNED` layout
    // may address any byte, and a plain dereference there is undefined
    // behaviour in Rust.
    Ok(unsafe {
        match width {
            1 => *ptr as u64,
            2 => (ptr as *const u16).read_unaligned() as u64,
            4 => (ptr as *const u32).read_unaligned() as u64,
            _ => (ptr as *const u64).read_unaligned(),
        }
    })
}

/// Write `width` bytes at a null-base (off-heap) `MemorySegment` address —
/// see `segment_native_get`.
pub(crate) fn segment_native_set(addr: i64, width: i64, raw: u64) -> Result<(), MethodCallFailed> {
    if crate::unsafe_arena_contains(addr) {
        let bytes = raw.to_le_bytes();
        if !crate::unsafe_arena_copy_in(addr, &bytes[..width as usize]) {
            return Err(RuntimeError::IllegalStateException {
                message: "Out of bound access on off-heap MemorySegment arena handle".into(),
            }
            .into());
        }
        return Ok(());
    }
    let ptr = addr as usize as *mut u8;
    if ptr.is_null() {
        return Err(RuntimeError::IllegalStateException {
            message: "MemorySegment access via null address".into(),
        }
        .into());
    }
    // SAFETY: see `segment_native_get`, unaligned writes included.
    unsafe {
        match width {
            1 => *ptr = raw as u8,
            2 => (ptr as *mut u16).write_unaligned(raw as u16),
            4 => (ptr as *mut u32).write_unaligned(raw as u32),
            _ => (ptr as *mut u64).write_unaligned(raw),
        }
    }
    Ok(())
}

/// `SegmentVarHandle.get(segment, offset)` — handled inline by `varhandle_get`
/// once `is_segment_var_handle` matches. Returns `None` (fall through to the
/// existing dispatch) only if the VarHandle's own fields can't be resolved;
/// otherwise always produces a value or an error.
pub(crate) fn segment_vh_get(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> Option<MethodCallResult> {
    if segment_vh_checks() && segment_vh_form_enabled() {
        if let Some(plan) = seg_vh_plan(ctx, this) {
            // `checkSegment`'s `Objects.requireNonNull`.
            let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
                return Some(Err(
                    RuntimeError::NullPointerException { message: None }.into()
                ));
            };
            return Some(segment_vh_form_access(ctx, plan, this, seg, args, false));
        }
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, this)?;
    let seg = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Some(Ok(Some(Value::Object(None)))),
    };
    let coord_offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    };
    let shape = segment_layout_shape(ctx, enclosing);
    let width = seg_shape_width(shape);
    // Read before the scope check, which can run Java and move `enclosing`.
    let align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    Some((|| -> MethodCallResult {
        // Round 12 wave 7 (lane ffm4): an access window from before the scope
        // check to after the read, dropped before the boxing allocation.
        // j27 wave 6 (lane T1LOOPFFM): `checkEnclosingLayout` first, as JDK 25.
        segment_vh_enclosing_before_scope(ctx, seg, this, enclosing, align, coord_offset)?;
        let window = crate::ffm_fast::AccessWindow::open();
        let mut seg = seg;
        segment_vh_check_scope(ctx, &mut seg)?;
        let (base, addr) = segment_raw_access(ctx, seg, vh_offset, coord_offset, width)?;
        // `isAlignedForElement(base, enclosing)`: the enclosing layout's
        // start, `addr` less the handle's own offset (j27 wave 6: a member
        // handle tested its field's address against the enclosing alignment).
        if segment_vh_misaligned(align, base, addr.wrapping_sub(vh_offset)) {
            // j27 wave 4 (lane FFMMSG): HotSpot's text, after the window
            // closes. `enclosing` may have moved in the scope check: no
            // `(of ..)` clause.
            drop(window);
            return Err(crate::panama::native_misaligned_refusal(
                ctx,
                seg,
                None,
                coord_offset,
                align,
            ));
        }
        let raw = match base {
            None => segment_native_get(addr, width)?,
            Some(base_obj) => segment_heap_get(ctx, base_obj, addr, width).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::IllegalStateException {
                    message: "Out of bound access on heap MemorySegment".into(),
                })
            })?,
        };
        drop(window);
        let raw = if be { seg_swap_bytes(raw, width) } else { raw };
        // CANONICAL — same measurement as `layout_vh_get`: an FFM
        // `MemorySegment` read handed back through a `VarHandle` is
        // `X.valueOf`-boxed on HotSpot. `seg_decode_value` and
        // `seg_shape_desc` switch on the same `SegShape`, so the variant and
        // the descriptor agree (including `Address`, which is `Value::Long`
        // and `"J"` in both).
        Ok(Some(box_value_canonical(
            ctx,
            seg_decode_value(shape, raw),
            seg_shape_desc(shape),
        )))
    })())
}

/// `SegmentVarHandle.set(segment, offset, value)` or path-bound
/// `SegmentVarHandle.set(segment, value)` - see `segment_vh_get`.
pub(crate) fn segment_vh_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> Option<MethodCallResult> {
    if segment_vh_checks() && segment_vh_form_enabled() {
        if let Some(plan) = seg_vh_plan(ctx, this) {
            // `checkSegment`'s `Objects.requireNonNull`.
            let Some(Value::Object(Some(seg))) = args.get(1).copied() else {
                return Some(Err(
                    RuntimeError::NullPointerException { message: None }.into()
                ));
            };
            return Some(segment_vh_form_access(ctx, plan, this, seg, args, true));
        }
    }
    let (enclosing, vh_offset, be) = segment_vh_fields(ctx, this)?;
    let seg = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Some(Ok(None)),
    };
    let (coord_offset, value_index) = if args.len() >= 4 {
        (
            match args.get(2) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            },
            3,
        )
    } else {
        (0, 2)
    };
    let value = args.get(value_index).cloned().unwrap_or(Value::Int(0));
    let shape = segment_layout_shape(ctx, enclosing);
    let width = seg_shape_width(shape);
    // Read before the scope check, which can run Java and move `enclosing`.
    let align = crate::panama_libffi::layout_align(ctx, enclosing) as i64;
    Some((|| -> MethodCallResult {
        // Direct field read (not `isReadOnly()` invoke_virtual) — see
        // `segment_raw_access`'s doc comment on avoiding reentrant native→
        // bytecode calls from inside a native invoked by JIT-compiled code.
        if segment_vh_checks() {
            // Round 12 wave 7 (lane ffm4): the reader that knows both segment
            // models, and the JDK's exception (`checkAccess`).
            let read_only = crate::phases_late::foreign_ffm::p67_segment_is_read_only(
                ctx,
                &[Value::Object(Some(seg))],
            )?;
            if matches!(read_only, Some(Value::Int(n)) if n != 0) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Attempt to write a read-only segment".into(),
                }
                .into());
            }
        } else {
            let is_ro = match ctx
                .resolve_field_index("jdk/internal/foreign/AbstractMemorySegmentImpl", "readOnly")
            {
                Some(i) => matches!(ctx.get_field(seg, i), Value::Int(n) if n != 0),
                None => false,
            };
            if is_ro {
                return Err(RuntimeError::IllegalStateException {
                    message: "Attempted write on read-only MemorySegment".into(),
                }
                .into());
            }
        }
        // Encoded before the scope check can run Java: an `ADDRESS` value is a
        // `MemorySegment` reference, stored as its address (it used to encode
        // as 0); a null one is `NullPointerException`, as on HotSpot.
        let mut raw = match (shape, value) {
            (SegShape::Address, Value::Object(Some(target))) if segment_vh_checks() => {
                crate::panama_libffi::segment_address(ctx, target) as u64
            }
            (SegShape::Address, Value::Object(None)) if segment_vh_checks() => {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            _ => seg_encode_value(&value, width),
        };
        if be {
            raw = seg_swap_bytes(raw, width);
        }
        // As in `segment_vh_get` (j27 wave 6, lane T1LOOPFFM): after the
        // read-only test above, before the session.
        segment_vh_enclosing_before_scope(ctx, seg, this, enclosing, align, coord_offset)?;
        // Round 12 wave 7 (lane ffm4): an access window from before the scope
        // check to after the write.
        let _window = crate::ffm_fast::AccessWindow::open();
        let mut seg = seg;
        segment_vh_check_scope(ctx, &mut seg)?;
        let (base, addr) = segment_raw_access(ctx, seg, vh_offset, coord_offset, width)?;
        // The enclosing layout's start, as in `segment_vh_get`.
        if segment_vh_misaligned(align, base, addr.wrapping_sub(vh_offset)) {
            // As in `segment_vh_get` (j27 wave 4, lane FFMMSG).
            drop(_window);
            return Err(crate::panama::native_misaligned_refusal(
                ctx,
                seg,
                None,
                coord_offset,
                align,
            ));
        }
        match base {
            None => segment_native_set(addr, width, raw)?,
            Some(base_obj) => {
                if !segment_heap_set(ctx, base_obj, addr, width, raw) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "Out of bound access on heap MemorySegment".into(),
                    }
                    .into());
                }
            }
        }
        Ok(None)
    })())
}

#[cfg(test)]
mod r12w7_ffm4_segment_vh_tests {
    //! Round 12 wave 7 (lane ffm4): a real `SegmentVarHandle` over a CratonVM
    //! carrier decodes the carrier (it read `readOnly` as the address), and
    //! checks scope, bounds, alignment and read-only like the JDK
    //! (`r12w6-ffm3-segment-varhandle-has-no-scope-check`).
    use super::*;
    use crate::phases_late::foreign_ffm::{p67_memory_session, p67_session_slots};
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    struct Fixture {
        ctx: MockNativeContext,
        vh: ObjectRef,
        seg: ObjectRef,
        session: ObjectRef,
        ptr: *mut u8,
    }

    fn fixture() -> Fixture {
        let mut ctx = mock_ctx();
        // `get` answers the CANONICAL box from the process-wide cache keyed by
        // `vm_identity()`: under the mock default `0` a full parallel lib run
        // served a box another test's dropped mock heap had filed (the
        // `r15w3_vh4_index_coordinate_tests` failure, i59 L4). Its own row.
        ctx.set_vm_identity(0x5E6A_0001);
        // Production answers an unresolvable field name with null; the
        // carrier's `readOnly` lookup falls back to slot 3 only then.
        ctx.set_absent_field_answers_null(true);
        let vh_cid = match ctx.ensure_class_initialized(SEGMENT_VAR_HANDLE_CLASS) {
            Ok(cid) => cid,
            Err(err) => panic!("no class: {err:?}"),
        };
        let field = |name: &str, desc: &str, slot: usize| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: vh_cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            vh_cid,
            vec![
                field("enclosing", "Ljava/lang/foreign/MemoryLayout;", 0),
                field("offset", "J", 1),
                field("be", "Z", 2),
            ],
        );
        let layout_name = "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl";
        let layout_cid = match ctx.ensure_class_initialized(layout_name) {
            Ok(cid) => cid,
            Err(err) => panic!("no class: {err:?}"),
        };
        let layout = ctx.alloc_object(layout_cid, 3);
        ctx.set_field(layout, 0, Value::Long(4));
        ctx.set_field(layout, 1, Value::Long(4));
        let vh = ctx.alloc_object(vh_cid, 3);
        ctx.set_field(vh, 0, Value::Object(Some(layout)));
        ctx.set_field(vh, 1, Value::Long(0));
        ctx.set_field(vh, 2, Value::Int(0));
        let session = match p67_memory_session(&mut ctx) {
            Ok(Value::Object(Some(session))) => session,
            other => panic!("no session: {other:?}"),
        };
        let (_id, ptr) = ctx
            .allocate_native_memory(16, 8)
            .unwrap_or_else(|| panic!("mock allocation failed"));
        let seg = match crate::panama::alloc_segment_carrier(&mut ctx, 6) {
            Ok(seg) => seg,
            Err(err) => panic!("no carrier: {err:?}"),
        };
        ctx.set_field(seg, 0, Value::Long(ptr as i64));
        ctx.set_field(seg, 1, Value::Long(16));
        ctx.set_field(seg, 2, Value::Object(Some(session)));
        ctx.set_field(seg, 3, Value::Int(0));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(0));
        Fixture {
            ctx,
            vh,
            seg,
            session,
            ptr,
        }
    }

    fn set(f: &mut Fixture, offset: i64, value: i32) -> MethodCallResult {
        let args = [
            Value::Object(Some(f.vh)),
            Value::Object(Some(f.seg)),
            Value::Long(offset),
            Value::Int(value),
        ];
        match segment_vh_set(&mut f.ctx, f.vh, &args) {
            Some(result) => result,
            None => panic!("the SegmentVarHandle fields did not resolve"),
        }
    }

    fn get(f: &mut Fixture, offset: i64) -> MethodCallResult {
        let args = [
            Value::Object(Some(f.vh)),
            Value::Object(Some(f.seg)),
            Value::Long(offset),
        ];
        match segment_vh_get(&mut f.ctx, f.vh, &args) {
            Some(result) => result,
            None => panic!("the SegmentVarHandle fields did not resolve"),
        }
    }

    fn word_at(f: &Fixture, offset: usize) -> u32 {
        // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
        unsafe { (f.ptr.add(offset) as *const u32).read_unaligned() }
    }

    /// Before the fix the carrier's `readOnly` slot was read as the address,
    /// so every access was `MemorySegment access via null address`.
    #[test]
    fn a_carrier_is_written_and_read_at_its_own_address() {
        let mut f = fixture();
        assert!(set(&mut f, 4, 0x1122_3344).is_ok());
        assert_eq!(word_at(&f, 4), 0x1122_3344);
        let got = match get(&mut f, 4) {
            Ok(Some(Value::Object(Some(boxed)))) => crate::lang_class::unbox_value(&f.ctx, boxed),
            Ok(Some(v)) => v,
            other => panic!("get gave {other:?}"),
        };
        assert_eq!(got, Value::Int(0x1122_3344));
    }

    /// The use-after-close the page records: a closed scope refuses both.
    #[test]
    fn a_closed_scope_is_already_closed_for_get_and_set() {
        let mut f = fixture();
        assert!(set(&mut f, 0, 7).is_ok());
        let slots = p67_session_slots(&f.ctx, f.session);
        f.ctx.set_field(f.session, slots.state, Value::Int(0));
        let err = match set(&mut f, 0, 9) {
            Ok(v) => panic!("a closed segment was written: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
        assert_eq!(word_at(&f, 0), 7, "nothing was written");
        let err = match get(&mut f, 0) {
            Ok(v) => panic!("a closed segment was read: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    }

    /// Bounds are `IndexOutOfBoundsException`, a misaligned native address is
    /// `IllegalArgumentException`, and a read-only segment refuses the write.
    #[test]
    fn bounds_alignment_and_read_only_raise_the_jdk_exceptions() {
        let mut f = fixture();
        let err = match get(&mut f, 16) {
            Ok(v) => panic!("an out-of-bounds read answered: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
        // j27 wave 4 (lane FFMMSG): HotSpot's `checkEnclosingLayout` text, not
        // the byte-array view handles' `Misaligned access`. The `(of ..)`
        // clause is the layout's `toString()`, which this mock answers with
        // nothing, so none here (j27 wave 6: see the next test for it).
        let message = match get(&mut f, 2) {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => message,
            other => panic!("a misaligned read answered: {other:?}"),
        };
        assert_eq!(
            message,
            format!(
                "Target offset 2 is incompatible with alignment constraint 4 for segment \
                 MemorySegment{{ kind: native, address: 0x{:x}, byteSize: 16 }}",
                f.ptr as usize
            )
        );
        // An out-of-bounds read names the segment too (it said `offset=..,
        // width=.., size=..`).
        let message = match get(&mut f, 16) {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IndexOutOfBoundsException { message: Some(message) },
            ))) => message,
            other => panic!("an out-of-bounds read answered: {other:?}"),
        };
        assert_eq!(
            message,
            format!(
                "Out of bound access on segment MemorySegment{{ kind: native, address: 0x{:x}, \
                 byteSize: 16 }}; new offset = 16; new length = 4",
                f.ptr as usize
            )
        );
        f.ctx.set_field(f.seg, 3, Value::Int(1));
        let err = match set(&mut f, 0, 5) {
            Ok(v) => panic!("a read-only segment was written: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("read-only"), "{err:?}");
        assert_eq!(word_at(&f, 0), 0);
    }

    /// j27 wave 6 (lane T1LOOPFFM): JDK 25 checks the alignment
    /// (`checkEnclosingLayout`) BEFORE the session, so the misaligned refusal
    /// names its layout (`(of i4)`, which the old scope-first road dropped)
    /// and a closed session does not turn it into `Already closed`. An
    /// aligned access to the closed segment is still the session's refusal.
    #[test]
    fn a_misaligned_access_refuses_before_the_session_and_names_its_layout() {
        let mut f = fixture();
        let expected = format!(
            "Target offset 2 is incompatible with alignment constraint 4 (of i4) for segment \
             MemorySegment{{ kind: native, address: 0x{:x}, byteSize: 16 }}",
            f.ptr as usize
        );
        let misaligned = |f: &mut Fixture, write: bool| {
            let i4 = f.ctx.create_string("i4");
            f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
            match if write { set(f, 2, 1) } else { get(f, 2) } {
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalArgumentException { message },
                ))) => message,
                other => panic!("a misaligned access (write {write}) answered: {other:?}"),
            }
        };
        for write in [false, true] {
            assert_eq!(misaligned(&mut f, write), expected, "open, write {write}");
        }
        let slots = p67_session_slots(&f.ctx, f.session);
        f.ctx.set_field(f.session, slots.state, Value::Int(0));
        for write in [false, true] {
            assert_eq!(misaligned(&mut f, write), expected, "closed, write {write}");
        }
        let err = match get(&mut f, 0) {
            Ok(v) => panic!("a closed segment was read: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("IllegalState"), "{err:?}");
        assert_eq!(word_at(&f, 0), 0, "nothing written");
    }

    /// The layout-shape handle never asked either: `seg.asReadOnly()` written
    /// through `JAVA_INT.varHandle()` was written.
    #[test]
    fn a_layout_varhandle_refuses_to_write_a_read_only_segment() {
        let mut f = fixture();
        f.ctx.set_field(f.seg, 3, Value::Int(1));
        let shape = SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 0,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 0,
        };
        let args = [
            Value::Object(Some(f.vh)),
            Value::Object(Some(f.seg)),
            Value::Long(0),
            Value::Int(3),
        ];
        let err = match layout_vh_set(&mut f.ctx, shape, &args) {
            Ok(v) => panic!("a read-only segment was written: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("read-only"), "{err:?}");
        assert_eq!(word_at(&f, 0), 0);
    }

    /// Round 14 wave 6 (lane ffm6): the shape
    /// `p67_memory_layout_array_element_var_handle` mints for
    /// `JAVA_INT.arrayElementVarHandle()` stores at `offset + 4 * index`, and
    /// refuses the way JDK 25's `AbstractLayout.scale` does -- before the
    /// segment's own checks (here: before the read-only refusal).
    #[test]
    fn an_array_element_layout_varhandle_scales_and_checks_its_index() {
        let mut f = fixture();
        let shape = SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 4,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: true,
            enclosing_size: 0,
        };
        let vh = Value::Object(Some(f.vh));
        let seg = Value::Object(Some(f.seg));
        let stored = layout_vh_set(
            &mut f.ctx,
            shape,
            &[vh, seg, Value::Long(4), Value::Long(2), Value::Int(42)],
        );
        assert!(matches!(stored, Ok(None)), "{stored:?}");
        assert_eq!(word_at(&f, 12), 42);
        assert_eq!(word_at(&f, 0), 0);
        let refusal = |f: &mut Fixture, offset: i64, index: i64| -> String {
            let args = [vh, seg, Value::Long(offset), Value::Long(index)];
            match layout_vh_get(&mut f.ctx, shape, &args) {
                Ok(v) => panic!("offset {offset} index {index} answered: {v:?}"),
                Err(err) => format!("{err:?}"),
            }
        };
        let err = refusal(&mut f, 0, -1);
        assert!(err.contains("The provided index is negative: -1"), "{err}");
        let err = refusal(&mut f, -4, -1);
        assert!(err.contains("The provided offset is negative: -4"), "{err}");
        let err = refusal(&mut f, 0, i64::MAX);
        assert!(err.contains("long overflow"), "{err}");
        // In range for `scale`, past the segment: the segment's own refusal.
        let err = refusal(&mut f, 0, 4);
        assert!(err.contains("IndexOutOfBounds"), "{err}");
        // The index refusal precedes the read-only one.
        f.ctx.set_field(f.seg, 3, Value::Int(1));
        let err = match layout_vh_set(
            &mut f.ctx,
            shape,
            &[vh, seg, Value::Long(0), Value::Long(-1), Value::Int(7)],
        ) {
            Ok(v) => panic!("a negative index was written: {v:?}"),
            Err(err) => format!("{err:?}"),
        };
        assert!(err.contains("The provided index is negative"), "{err}");
    }

    /// Round 14 wave 6 (lane ffm6): a `sequenceElement()` index is
    /// `Objects.checkIndex(index, elementCount)`, even where the address it
    /// names is still inside the segment.
    #[test]
    fn a_sequence_layout_varhandle_checks_its_index_against_the_element_count() {
        let mut f = fixture();
        let shape = SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 4,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: Some(2),
            scaled_index: false,
            enclosing_size: 0,
        };
        let vh = Value::Object(Some(f.vh));
        let seg = Value::Object(Some(f.seg));
        let stored = layout_vh_set(
            &mut f.ctx,
            shape,
            &[vh, seg, Value::Long(0), Value::Long(1), Value::Int(9)],
        );
        assert!(matches!(stored, Ok(None)), "{stored:?}");
        assert_eq!(word_at(&f, 4), 9);
        for index in [2_i64, -1] {
            let err = match layout_vh_set(
                &mut f.ctx,
                shape,
                &[vh, seg, Value::Long(0), Value::Long(index), Value::Int(5)],
            ) {
                Ok(v) => panic!("index {index} was written: {v:?}"),
                Err(err) => format!("{err:?}"),
            };
            assert!(
                err.contains(&format!("Index {index} out of bounds for length 2")),
                "{err}"
            );
        }
        assert_eq!(word_at(&f, 8), 0);
        // `sequenceLayout(10, JAVA_INT)` (40 bytes) does not fit the 16-byte
        // segment at base 0, so even index 0 is refused (`checkEnclosingLayout`).
        let tall = SegmentVhShape {
            index_bound: Some(10),
            enclosing_size: 40,
            ..shape
        };
        let err = match layout_vh_set(
            &mut f.ctx,
            tall,
            &[vh, seg, Value::Long(0), Value::Long(0), Value::Int(5)],
        ) {
            Ok(v) => panic!("a sequence larger than the segment was written: {v:?}"),
            Err(err) => format!("{err:?}"),
        };
        assert!(err.contains("Out of bound access"), "{err}");
        assert_eq!(word_at(&f, 0), 0);
    }

    /// Round 14 wave 7 (lane review7): the ROOT layout's alignment is checked
    /// at the handle's base, as JDK 25's `checkEnclosingLayout` does -- member
    /// `b` at offset 4 of an 8-aligned `struct { int a; int b; }` is written
    /// at base 0 (it was refused: 8 was asked of offset 4), and base 4 is
    /// refused by the struct's alignment.
    #[test]
    fn a_member_handle_checks_the_root_alignment_at_its_base() {
        let mut f = fixture();
        let shape = SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: cfg!(target_endian = "little"),
            base_offset: 4,
            stride: 0,
            alignment: 8,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 8,
        };
        let vh = Value::Object(Some(f.vh));
        let seg = Value::Object(Some(f.seg));
        let stored = layout_vh_set(
            &mut f.ctx,
            shape,
            &[vh, seg, Value::Long(0), Value::Int(77)],
        );
        assert!(matches!(stored, Ok(None)), "{stored:?}");
        assert_eq!(word_at(&f, 4), 77);
        let err = match layout_vh_get(&mut f.ctx, shape, &[vh, seg, Value::Long(4)]) {
            Ok(v) => panic!("a base the struct's alignment refuses was read: {v:?}"),
            Err(err) => format!("{err:?}"),
        };
        assert!(
            err.contains("Target offset 4 is incompatible with alignment constraint 8"),
            "{err}"
        );
    }

    /// Round 14 wave 1 (lane ffm): `ADDRESS.withTargetLayout(JAVA_INT)
    /// .varHandle().get(..)` answers a segment of the target's size (it was 0,
    /// so following the pointer was `IndexOutOfBoundsException`), and a
    /// pointer the target's alignment does not divide is refused
    /// (`r13w13-ffm7-address-read-ignores-target-layout`).
    #[test]
    fn an_address_layout_varhandle_read_is_sized_and_aligned_by_its_target() {
        let mut f = fixture();
        let shape = SegmentVhShape {
            width: 8,
            carrier: FFM_ADDRESS_CARRIER,
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 0,
            alignment: 8,
            target_size: 4,
            target_align: 4,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 0,
        };
        let args = [
            Value::Object(Some(f.vh)),
            Value::Object(Some(f.seg)),
            Value::Long(0),
        ];
        // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
        unsafe { (f.ptr as *mut u64).write_unaligned(0x1000) };
        let read = match layout_vh_get(&mut f.ctx, shape, &args) {
            Ok(Some(Value::Object(Some(seg)))) => seg,
            other => panic!("no segment: {other:?}"),
        };
        assert_eq!(f.ctx.get_field(read, 0), Value::Long(0x1000));
        assert_eq!(f.ctx.get_field(read, 1), Value::Long(4));
        // SAFETY: as above.
        unsafe { (f.ptr as *mut u64).write_unaligned(0x1002) };
        let err = match layout_vh_get(&mut f.ctx, shape, &args) {
            Ok(v) => panic!("a misaligned pointer was answered: {v:?}"),
            Err(err) => err,
        };
        assert!(
            format!("{err:?}").contains("Invalid alignment constraint for address: 0x1002"),
            "{err:?}"
        );
        // Without a target (or with the switch off) the answer stays zero-length.
        let untargeted = SegmentVhShape {
            target_size: 0,
            target_align: 1,
            ..shape
        };
        let read = match layout_vh_get(&mut f.ctx, untargeted, &args) {
            Ok(Some(Value::Object(Some(seg)))) => seg,
            other => panic!("no segment: {other:?}"),
        };
        assert_eq!(f.ctx.get_field(read, 1), Value::Long(0));
    }

    /// j27 wave 4 (lane FFMMSG): `ADDRESS.varHandle().set(seg, 0L,
    /// MemorySegment.ofArray(new int[2]))` is the `unboxSegment` filter's
    /// `IllegalArgumentException: Heap segment not allowed: <segment>` on
    /// HotSpot 25; this named the carrier class (`class: ..`). Nothing stored.
    #[test]
    fn an_address_layout_varhandle_refuses_a_heap_value_with_hotspot_text() {
        let mut f = fixture();
        let array = f.ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
        let heap =
            match crate::panama::pe_of_array_alias(&mut f.ctx, &[Value::Object(Some(array))]) {
                Ok(Some(Value::Object(Some(heap)))) => heap,
                other => panic!("ofArray answered {other:?}"),
            };
        let shape = SegmentVhShape {
            width: 8,
            carrier: FFM_ADDRESS_CARRIER,
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 0,
            alignment: 8,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 0,
        };
        let args = [
            Value::Object(Some(f.vh)),
            Value::Object(Some(f.seg)),
            Value::Long(0),
            Value::Object(Some(heap)),
        ];
        let message = match layout_vh_set(&mut f.ctx, shape, &args) {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => message,
            other => panic!("expected IllegalArgumentException, got {other:?}"),
        };
        assert_eq!(
            message,
            format!(
                "Heap segment not allowed: MemorySegment{{ kind: heap, heapBase: [I@{:x}, address: 0x0, byteSize: 8 }}",
                f.ctx.identity_hash_code(array) as u32
            )
        );
        assert_eq!((word_at(&f, 0), word_at(&f, 4)), (0, 0), "the refused store landed");
    }

    /// A `byte[16]` alias segment and its HotSpot `toString()` text.
    fn heap_bytes(f: &mut Fixture) -> (ObjectRef, String) {
        let array = f.ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let seg =
            match crate::panama::pe_of_array_alias(&mut f.ctx, &[Value::Object(Some(array))]) {
                Ok(Some(Value::Object(Some(seg)))) => seg,
                other => panic!("ofArray answered {other:?}"),
            };
        let text = format!(
            "MemorySegment{{ kind: heap, heapBase: [B@{:x}, address: 0x0, byteSize: 16 }}",
            f.ctx.identity_hash_code(array) as u32
        );
        (seg, text)
    }

    fn message_of(result: Result<impl std::fmt::Debug, MethodCallFailed>) -> String {
        match result {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IndexOutOfBoundsException { message: Some(message) },
            ))) => message,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => message,
            other => panic!("expected IOOBE or IAE, got {other:?}"),
        }
    }

    /// j27 wave 4 (lane FFMMSG): a CratonVM carrier reaching
    /// `checkEnclosingLayout`'s two refusals gets HotSpot's text, the segment
    /// from `panama::segment_to_string` and the layout from its own
    /// `toString()`. It said `Out of bound access on segment: offset=..,
    /// length=.., size=..` and `.. for segment` with nothing after.
    #[test]
    fn a_carrier_through_check_enclosing_names_segment_and_layout() {
        let mut f = fixture();
        let (seg, text) = heap_bytes(&mut f);
        assert_eq!(
            message_of(segment_vh_check_enclosing(&mut f.ctx, seg, f.vh, 4, 4, 14)),
            format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
        );
        let i4 = f.ctx.create_string("i4");
        f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
        assert_eq!(
            message_of(segment_vh_check_enclosing(&mut f.ctx, seg, f.vh, 4, 4, 0)),
            format!(
                "Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment {text}"
            )
        );
    }

    /// j27 wave 4 (lane FFMMSG): the same two refusals on the layout-handle
    /// road (`layout_vh_locate`): `Out of bound access on segment of 16
    /// bytes` and a bare `.. for segment` before.
    #[test]
    fn a_carrier_through_the_layout_handle_names_segment_and_layout() {
        let mut f = fixture();
        let (seg, text) = heap_bytes(&mut f);
        let shape = SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: cfg!(target_endian = "little"),
            base_offset: 0,
            stride: 0,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 4,
        };
        let vh = f.vh;
        let at = |base: i64| [Value::Object(Some(vh)), Value::Object(Some(seg)), Value::Long(base)];
        let args = at(14);
        assert_eq!(
            message_of(layout_vh_locate(&mut f.ctx, shape, &args, seg).map(|_| ())),
            format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
        );
        // Without a modelled enclosing layout the access's own bounds check
        // answers the same text (it said `VarHandle access of 4 bytes at
        // offset 14 is out of bounds for a segment of 16 bytes`).
        let unenclosed = SegmentVhShape {
            enclosing_size: 0,
            ..shape
        };
        assert_eq!(
            message_of(layout_vh_locate(&mut f.ctx, unenclosed, &args, seg).map(|_| ())),
            format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
        );
        let args = at(0);
        let i4 = f.ctx.create_string("i4");
        f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
        assert_eq!(
            message_of(layout_vh_locate(&mut f.ctx, shape, &args, seg).map(|_| ())),
            format!(
                "Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment {text}"
            )
        );
    }

    /// Round 13 wave 1 (lane mhffm): turn the fixture's handle into a JDK 25
    /// PATH handle -- `enclosing` a 8-byte, 4-aligned struct, the member a
    /// `short` at offset 2, the value type in the `VarForm`
    /// (`VarHandleSegmentAsShorts`).
    fn make_struct_path_handle(f: &mut Fixture) {
        let vh_cid = f.ctx.class_id_of_object(f.vh);
        let field = |name: &str, desc: &str, slot: usize, owner: ClassId| {
            cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: desc.to_string(),
                access_flags: 0,
                slot_index: slot,
                declaring_class_id: owner,
                is_static: false,
            }
        };
        f.ctx.set_declared_fields(
            vh_cid,
            vec![
                field("enclosing", "Ljava/lang/foreign/MemoryLayout;", 0, vh_cid),
                field("offset", "J", 1, vh_cid),
                field("be", "Z", 2, vh_cid),
                field("vform", "Ljava/lang/invoke/VarForm;", 3, vh_cid),
            ],
        );
        let form_cid = match f.ctx.ensure_class_initialized("java/lang/invoke/VarForm") {
            Ok(cid) => cid,
            Err(err) => panic!("no class: {err:?}"),
        };
        f.ctx.set_declared_fields(
            form_cid,
            vec![field("implClass", "Ljava/lang/Class;", 0, form_cid)],
        );
        let impl_cid = match f
            .ctx
            .ensure_class_initialized("java/lang/invoke/VarHandleSegmentAsShorts")
        {
            Ok(cid) => cid,
            Err(err) => panic!("no class: {err:?}"),
        };
        let mirror = f.ctx.alloc_object(impl_cid, 1);
        f.ctx
            .set_field(mirror, 0, Value::Int(impl_cid.as_u32() as i32));
        let form = f.ctx.alloc_object(form_cid, 1);
        f.ctx.set_field(form, 0, Value::Object(Some(mirror)));
        let struct_cid = match f
            .ctx
            .ensure_class_initialized("jdk/internal/foreign/layout/StructLayoutImpl")
        {
            Ok(cid) => cid,
            Err(err) => panic!("no class: {err:?}"),
        };
        let layout = f.ctx.alloc_object(struct_cid, 3);
        f.ctx.set_field(layout, 0, Value::Long(8));
        f.ctx.set_field(layout, 1, Value::Long(4));
        f.ctx.set_field(f.vh, 0, Value::Object(Some(layout)));
        f.ctx.set_field(f.vh, 1, Value::Long(2));
        f.ctx.set_field(f.vh, 3, Value::Object(Some(form)));
    }

    fn short_at(f: &Fixture, offset: usize) -> u16 {
        // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
        unsafe { (f.ptr.add(offset) as *const u16).read_unaligned() }
    }

    /// Wave 7 read a group `enclosing` as an 8-byte ADDRESS at `offset`:
    /// the member at base 8 was an out-of-bounds 8-byte read at 10.
    #[test]
    fn a_path_handle_reads_the_forms_type_at_the_members_offset() {
        let mut f = fixture();
        make_struct_path_handle(&mut f);
        assert!(set(&mut f, 8, 0x1234).is_ok());
        assert_eq!(short_at(&f, 10), 0x1234);
        let got = match get(&mut f, 8) {
            Ok(Some(Value::Object(Some(boxed)))) => crate::lang_class::unbox_value(&f.ctx, boxed),
            Ok(Some(v)) => v,
            other => panic!("get gave {other:?}"),
        };
        assert_eq!(got, Value::Int(0x1234));
    }

    /// `checkEnclosingLayout`: the whole struct must fit at the coordinate
    /// (base 10 leaves 6 of its 8 bytes), and the coordinate must meet the
    /// struct's alignment (base 2), even where the member alone would fit.
    #[test]
    fn a_path_handle_checks_the_enclosing_layout_not_the_member() {
        let mut f = fixture();
        make_struct_path_handle(&mut f);
        let err = match get(&mut f, 10) {
            Ok(v) => panic!("a struct past the end answered: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
        let err = match get(&mut f, 2) {
            Ok(v) => panic!("a misaligned struct answered: {v:?}"),
            Err(err) => err,
        };
        assert!(
            format!("{err:?}").contains("alignment constraint 4"),
            "{err:?}"
        );
    }

    /// HotSpot checks bounds before the session: a CLOSED segment accessed
    /// out of bounds is `IndexOutOfBoundsException`, not `Already closed`.
    #[test]
    fn bounds_come_before_the_session_check() {
        let mut f = fixture();
        make_struct_path_handle(&mut f);
        let slots = p67_session_slots(&f.ctx, f.session);
        f.ctx.set_field(f.session, slots.state, Value::Int(0));
        let err = match get(&mut f, 10) {
            Ok(v) => panic!("a closed out-of-bounds read answered: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
        let err = match get(&mut f, 8) {
            Ok(v) => panic!("a closed segment was read: {v:?}"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    }

    #[test]
    fn the_form_class_names_the_value_type() {
        let t = segment_vh_shape_of_form_class;
        assert!(t("java/lang/invoke/VarHandleSegmentAsInts") == Some(SegShape::Int));
        assert!(t("java/lang/invoke/VarHandleSegmentAsIntsAligned") == Some(SegShape::Int));
        assert!(t("java/lang/invoke/VarHandleSegmentAsLongs") == Some(SegShape::Long));
        assert!(t("java/lang/invoke/VarHandleSegmentAsBooleans") == Some(SegShape::Boolean));
        assert!(t("java/lang/invoke/VarHandleSegmentAsDoublesAligned") == Some(SegShape::Double));
        assert!(t("java/lang/invoke/VarHandleByteArrayAsInts").is_none());
    }

    /// JIT round 20 wave 8 (lane huntFFM): JDK 25's unaligned segment forms
    /// declare `get` / `set` only; the `...Aligned` subclasses and the byte /
    /// boolean forms (no subclass) add the ordered modes.
    #[test]
    fn only_aligned_and_byte_forms_define_the_ordered_modes() {
        let t = segment_vh_form_class_is_aligned;
        assert!(t("java/lang/invoke/VarHandleSegmentAsIntsAligned"));
        assert!(t("java/lang/invoke/VarHandleSegmentAsDoublesAligned"));
        assert!(t("java/lang/invoke/VarHandleSegmentAsBytes"));
        assert!(t("java/lang/invoke/VarHandleSegmentAsBooleans"));
        assert!(!t("java/lang/invoke/VarHandleSegmentAsInts"));
        assert!(!t("java/lang/invoke/VarHandleSegmentAsShorts"));
        assert!(!t("java/lang/invoke/VarHandleSegmentAsLongs"));
    }
}

/// The descriptor of the variable a `VarHandle` access reads or writes, for the
/// call shape in `args`.
///
/// It answers the same question the access natives themselves answer when they
/// pick a branch, in the SAME order, so the descriptor cannot disagree with the
/// value the native produced:
///
/// 1. `vh_array_call` matches → the array's element type. Every RMW native
///    probes this first (see `varhandle_get_and_set` / `_add` / `_bitwise` /
///    `compare_and_exchange`), including for byte-array-view handles, which
///    those natives do not special-case.
/// 2. otherwise the handle's own `field_desc`, via the meta side table
///    (`vh_type_desc`), which is what the instance/static branches read.
///
/// Falls back to `DESC_REF` when nothing resolves. That is deliberate and
/// lossless: `box_value`'s catch-all arm returns the value untouched for a
/// reference descriptor, so an unresolvable handle keeps exactly today's
/// behaviour instead of guessing a wrapper class.
pub(crate) fn vh_access_value_desc(ctx: &mut dyn NativeContext, args: &[Value]) -> Cow<'static, str> {
    if let Some((arr, _)) = vh_array_call(ctx, args) {
        return Cow::Borrowed(array_element_desc(ctx, arr));
    }
    match args.first() {
        Some(Value::Object(Some(this))) => vh_type_desc(ctx, *this),
        _ => Cow::Borrowed(DESC_REF),
    }
}

/// THE boxing funnel for every `VarHandle` access mode that HANDS BACK the
/// accessed variable.
///
/// # The defect this closes
///
/// Every such mode is registered under the erased
/// `([Ljava/lang/Object;)Ljava/lang/Object;` descriptor, so the native's own
/// contract is "return a reference". `varhandle_get` honoured it (it calls
/// `box_value`); `getAndSet`, `getAndAdd`, `getAndBitwise*` and
/// `compareAndExchange` returned the BARE primitive `Value`. Nothing
/// downstream repairs that — `vm_exec.rs`'s `unbox_poly_return` passes
/// `b'L' | b'['` through untouched — so a call site with no cast, whose
/// descriptor therefore ends in `)Ljava/lang/Object;`, received a raw `Int`
/// into a reference return slot. Measured: `System.out.println(
/// va.getAndSet(arr, 1, 20))` printed `null` where HotSpot 25.0.3 prints `2`.
/// The cast form `(int) va.getAndSet(...)` was correct only because its
/// descriptor is then `([III)I` and the coercion path accepts a raw `Int`.
///
/// # Why the boxing has to be HERE and not at the poly-return boundary
///
/// `Value::Int` carries `boolean`, `byte`, `char`, `short` AND `int` (see
/// `cratonvm_types::Value`), so the value's tag does not name its wrapper.
/// Measured on the JDK 25 oracle, HotSpot boxes by the VARIABLE's type:
/// `boolean[]` → `java.lang.Boolean` (prints `true`), `int[]` →
/// `java.lang.Integer`. `unbox_poly_return` sees only the value and the
/// call-site descriptor, and an erased call site says nothing but "Object" —
/// boxing there would have to guess `Integer` and would print `1` for a
/// `boolean[]`. The native is the only layer that knows the element/field
/// descriptor, so this is the only layer that can be right.
///
/// # Idempotent by construction
///
/// A value that is already `Value::Object(_)` passes straight through. That
/// covers `null`, a reference-typed variable, and `varhandle_get`'s own inline
/// `box_value` calls, so routing a mode through this funnel can never
/// double-box. It is also why `varhandle_get` keeps its inline boxing: the two
/// cannot fight.
///
/// # Relationship to `varhandle_reference_return_mismatch` (vm_exec.rs)
///
/// That check fires when a BOXED primitive reaches a call site whose declared
/// reference return type the wrapper is not assignable to — i.e. it can only
/// ever see values this funnel produces, and it accepts `java/lang/Object` and
/// every wrapper supertype. So the two agree by construction: this funnel makes
/// `Object o = vh.getAndSet(...)` correct (the check declines — `Object` is a
/// supertype), and makes `String s = (String) vh.getAndSet(...)` reach the
/// check that HotSpot answers with `WrongMethodTypeException`. Before this fix
/// the raw `Int` was not an object at all, so the check silently could not fire
/// on the RMW modes; there is no shape where both act.
pub(crate) fn vh_box_access_result(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(value) = result? else {
        return Ok(None);
    };
    if matches!(value, Value::Object(_)) {
        return Ok(Some(value));
    }
    let desc = vh_access_value_desc(ctx, args);
    // Widen BEFORE boxing, exactly as `varhandle_get`'s three field arms and
    // `Field.get` do. This `let` SHADOWS the binding twelve lines up, after
    // the `matches!(value, Value::Object(_))` early return has already
    // excluded references — so the coercion only ever sees a primitive, which
    // is its contract.
    let value = crate::lang_class::coerce_reflective_field_value(value, &desc);
    // CANONICAL — measured `vh.getAndSetInt` = true and
    // `vh.compareAndExchangeInt` = true: the value a read-modify-write mode
    // HANDS BACK is `X.valueOf`-boxed on HotSpot, exactly like a plain `get`.
    //
    // The variant guard inside `box_value_canonical` matters HERE more than
    // anywhere else in this file: `desc` comes from `vh_access_value_desc`,
    // which resolves the VARIABLE's declared descriptor, while `value` came
    // from whatever the access mode computed. A `("J", Value::Int)` pair is
    // therefore reachable, and the helper answers it by falling back to
    // `box_value` — i.e. today's behaviour verbatim, never a cached
    // `Long.valueOf(0)`. `vh_access_value_desc`'s own `DESC_REF` fallback is
    // likewise untouched: a reference descriptor is not one of the six arms.
    //
    // F19-1 N3 scoped the missing `coerce_reflective_field_value` to
    // `varhandle_get`'s three field arms. MEASURED (`VhLong.java`),
    // `vh.getAndSetLong` hands back the old value **5** as a canonical
    // `Long` — so this funnel is a FOURTH member of that set, not a bystander:
    // the same `("J", Value::Int(5))` slot reaches it through `getAndSet` and
    // came out as a `Long` wrapper carrying compact-Int bits. The coercion
    // above closes it, and it is the member a fix that reads only
    // `varhandle_get` leaves behind — three of four looks complete.
    Ok(Some(box_value_canonical(ctx, value, &desc)))
}

/// VarHandle.get(receiver) → value
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver] for instance fields.
/// `CRATONVM_VH_NULL_COORDINATE_NPE=0` — restore the pre-2026-09-02 behaviour,
/// in which a `VarHandle` access with a NULL coordinate answered instead of
/// throwing.
///
/// Default on. The switch exists because this turns silence into an exception
/// on a path any workload can reach, so a suite that starts failing has to be
/// bisectable to this and not to a rebuild.
pub(crate) fn vh_null_coordinate_npe_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_VH_NULL_COORDINATE_NPE").as_deref(),
        Ok("0")
    )
}

/// The null check every access mode owes its leading COORDINATE.
///
/// # What was wrong
///
/// HotSpot raises `NullPointerException` when a `VarHandle` access is given a
/// null coordinate — the receiver of an instance-field handle, or the array of
/// an array-element handle. CratonVM answered instead. Measured on JDK 25 with
/// `RJdkVarHandleNullCoord`, which walks every access mode against an `int`
/// instance field, an `Object` instance field and an `int[]` element handle:
/// **75 of its 76 rows disagreed**, and the one that agreed is the control (a
/// STATIC-field handle, which has no coordinate to be null).
///
/// ```text
///   35 rows  returned 0        primitive reads, compareAndExchange, getAndAdd, bitwise
///   15 rows  returned false    every CAS mode
///   13 rows  returned normally every write mode  <- a lost store, silently
///   12 rows  returned null     reference reads
///    1 row   correct           the static-handle control
/// ```
///
/// The write rows are the worst of them: a store through a null receiver was
/// simply dropped, so the next read returned a stale value that looks
/// legitimate. The CAS rows are next: answering `false` tells the caller
/// "somebody else won the race", which is a retry loop rather than a failure.
///
/// # Why it is one check and not thirty
///
/// The registry is FIRST-WINS and `--dump-native-registry` names this file for
/// all 37 `java/lang/invoke/VarHandle` registrations — `phases_late/
/// reflect_invoke.rs` registers many of the same names and never owns a slot.
/// Those 37 names reach seven registered entry points, and this is called from
/// each of them.
///
/// # Why it is free
///
/// `VarHandle.get` is 21 368 822 calls on one netty phase, so a check that
/// costs anything per access is a regression. Only a leading coordinate that
/// is ACTUALLY NULL (or absent) reaches the slow path below; everything else —
/// a live object, or a primitive, which is what `args[1]` is for a
/// static-field handle's `set` — returns after one `match`.
///
/// # What it deliberately does not cover
///
/// `SegmentVarHandle` and the `MemorySegment` layout kind are skipped: their
/// null behaviour was not measured against the oracle, and a guard that
/// guesses is worse than none. `RJdkVarHandleNullCoord` covers what is
/// asserted here and nothing else.
pub(crate) fn vh_check_leading_coordinate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &str,
) -> Result<(), MethodCallFailed> {
    vh_check_leading_coordinate_row(ctx, args, &None, mode)
}

/// [`vh_check_leading_coordinate`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]). A STATIC row has no coordinate: its `get`
/// has no `args[1]` at all, so every static read took the slow path below
/// (a flag-table lookup, a class-name fetch and a row read) only to return.
pub(crate) fn vh_check_leading_coordinate_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    // The access mode (`get`, `setVolatile`, ...): an array handle's message
    // depends on it ([`vh_null_array_message`]).
    mode: &str,
) -> Result<(), MethodCallFailed> {
    // The hot path. A present, non-null leading argument is every ordinary
    // access; a primitive there belongs to a static-field handle, which has no
    // coordinate at all.
    match args.get(1) {
        Some(Value::Object(None)) | None => {}
        _ => return Ok(()),
    }
    if matches!(row, Some(Some(m)) if m.kind == VH_KIND_STATIC) {
        return Ok(());
    }
    if !vh_null_coordinate_npe_enabled() {
        return Ok(());
    }
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        // No handle to ask. Leave it to the access mode's own decode, which is
        // what produced today's answer for this shape.
        return Ok(());
    };
    if is_segment_var_handle(ctx, this) {
        return Ok(());
    }
    // The kind, resolved the same way `vh_value_and_coordinate_descriptors`
    // resolves it, and for the same reason: a REAL-JDK array handle has no
    // side-table entry and its slot 0 is the real `vform` REFERENCE, so
    // reading it as a kind tag would silently call it an instance handle.
    let meta_kind = vh_meta_get(ctx, this).as_deref().map(|m| m.kind);
    let kind = match meta_kind {
        Some(k) => k,
        None => {
            if real_array_var_handle_descriptors(ctx, this).is_some() {
                VH_KIND_ARRAY
            } else {
                ctx.get_field(this, VH_KIND)
                    .as_int()
                    .unwrap_or(VH_KIND_INSTANCE)
            }
        }
    };
    let coordinate = match kind {
        // No coordinate: `args[1]` is the VALUE, and a null one is a legal
        // store of null into a reference static.
        VH_KIND_STATIC => return Ok(()),
        // HotSpot's own texts (interpreter round i1 wave 52, lane L4; probe
        // `L4W52VarHandleNullMessages`, measured on 25.0.3 over every mode):
        // an instance handle's `Objects.requireNonNull` has none, and an
        // array handle's is the helpful NPE of its `VarHandle<T>s$Array` body
        // ([`vh_null_array_message`]).
        VH_KIND_INSTANCE => {
            // j27 wave 9 (lane W9EXC): the JDK frames HotSpot lists, with
            // `Objects.requireNonNull` on top.
            let (_, value_desc, _, _) = vh_value_and_coordinate_descriptors(ctx, this);
            let variable = value_desc.as_bytes().first().copied().unwrap_or(0);
            if let Some(chain) = vh_null_holder_chain(mode, variable) {
                let _ = ctx.note_served_raise_chain(chain);
            }
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        VH_KIND_ARRAY => {
            let (_, value_desc, _, _) = vh_value_and_coordinate_descriptors(ctx, this);
            return Err(RuntimeError::NullPointerException {
                message: Some(vh_null_array_message(mode, &value_desc)),
            }
            .into());
        }
        // The views (interpreter round i2 wave 53, lane L4; probe
        // `L4W53VarHandleRefusalTexts`, measured on 25.0.3): a byte-array
        // view's `get` / `set` (its only modes; the others are refused
        // first) is the helpful NPE of `VarHandleByteArrayAs<T>s.offset`,
        // which reads `ba.length`; a `ByteBuffer` view's every mode is
        // `Objects.requireNonNull`, with no message.
        VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => {
            Some("Cannot read the array length because \"ba\" is null")
        }
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => None,
        // An unmeasured kind (the `MemorySegment` layout handle) keeps today's
        // behaviour rather than inheriting a rule nothing checked.
        _ => return Ok(()),
    };
    Err(RuntimeError::NullPointerException {
        message: coordinate.map(str::to_string),
    }
    .into())
}

/// The helpful `NullPointerException` text of an array-element `VarHandle`
/// access mode `mode` on a null array whose element descriptor is
/// `value_desc`, as HotSpot 25 raises it from the `VarHandle<T>s$Array` body:
/// plain `get` / `set` index the array (`Cannot load from int array` /
/// `Cannot store to object array`), every other mode checks the index
/// against `array.length` first (`Cannot read the array length`). The local
/// is named `array` in each body.
pub(crate) fn vh_null_array_message(mode: &str, value_desc: &str) -> String {
    let elem = match value_desc.as_bytes().first() {
        Some(b'I') => "int",
        Some(b'J') => "long",
        Some(b'S') => "short",
        Some(b'C') => "char",
        Some(b'F') => "float",
        Some(b'D') => "double",
        Some(b'B' | b'Z') => "byte/boolean",
        _ => "object",
    };
    match mode {
        "get" => format!("Cannot load from {elem} array because \"array\" is null"),
        "set" => format!("Cannot store to {elem} array because \"array\" is null"),
        _ => "Cannot read the array length because \"array\" is null".to_string(),
    }
}

/// Does the view `meta` (a byte-array or `ByteBuffer` view) refuse access mode
/// `mode` with `UnsupportedOperationException`? The mode half of
/// [`vh_view_mode_refusal`], split out (round 15 wave 3, lane vh4) so the
/// index-coordinate conversion ([`vh_has_index_coordinate`]) can leave that
/// refusal first, as HotSpot orders it.
pub(crate) fn vh_view_mode_unsupported(meta: &VarHandleMeta, mode: &str) -> bool {
    if mode == "get" || mode == "set" {
        return false;
    }
    if !matches!(
        meta.kind,
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE
    ) {
        return true;
    }
    let elem = *meta.field_desc.as_bytes().first().unwrap_or(&b'J');
    let ordered = matches!(
        mode,
        "getVolatile" | "getOpaque" | "getAcquire" | "setVolatile" | "setOpaque" | "setRelease"
    );
    let numeric_update = mode.starts_with("getAndAdd") || mode.starts_with("getAndBitwise");
    let supported = match elem {
        b'I' | b'J' => true,
        b'F' | b'D' => !numeric_update,
        _ => ordered,
    };
    !supported
}

/// The access modes a byte-array or `ByteBuffer` view admits, refused the way
/// JDK 25's `VarHandleByteArrayAs*` / `VarHandleByteBufferAs*` refuse them.
///
/// * A `byte[]` view (`MethodHandles.byteArrayViewVarHandle`) has `get` and
///   `set` only; every other mode is `UnsupportedOperationException: <mode>`.
///   Before this, the ordered modes answered and the read-modify-write modes
///   ran as element operations on ONE byte (`getAndAdd` on an `int` view wrote
///   a byte and returned a `Byte`).
/// * A `ByteBuffer` view (`byteBufferViewVarHandle`) supports every mode for
///   `int`/`long`, every mode but `getAndAdd*`/`getAndBitwise*` for
///   `float`/`double`, and only the ordered reads and writes for
///   `short`/`char`. Past `get`/`set`, HotSpot then checks, in this order: a
///   heap buffer (`IllegalStateException: Atomic access not supported for heap
///   buffer`), a read-only buffer on a write (`ReadOnlyBufferException`), the
///   index, and the alignment (`IllegalStateException: Misaligned access at
///   index: N`).
///
/// Interpreter round i1 wave 31
/// (`interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`, items 1
/// and 2). Field and array handles never reach this.
pub(crate) fn vh_view_mode_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    meta: &VarHandleMeta,
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    if mode == "get" || mode == "set" {
        return Ok(());
    }
    if vh_view_mode_unsupported(meta, mode) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: mode.to_string(),
        }
        .into());
    }
    let elem = *meta.field_desc.as_bytes().first().unwrap_or(&b'J');
    // A null buffer is the leading-coordinate check's to report.
    let Some(Value::Object(Some(bb))) = args.get(1).copied() else {
        return Ok(());
    };
    if matches!(ctx.get_field_by_name(bb, "hb"), Value::Object(Some(_))) {
        return Err(RuntimeError::IllegalStateException {
            message: "Atomic access not supported for heap buffer".to_string(),
        }
        .into());
    }
    let writes = !mode.starts_with("get") || mode.starts_with("getAnd");
    if writes && byte_buffer_view_is_read_only(ctx, bb) {
        return Err(RuntimeError::ReadOnlyBufferException.into());
    }
    let Some(Value::Int(index)) = args.get(2).copied() else {
        return Ok(());
    };
    let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
        Ok(i) => i,
        Err(fallback) => {
            // Raised through the JDK body of `mode` (fuzz14b), like `get`/`set`.
            let this = obj_arg(args, 0)?;
            let view = Some((elem, meta.kind == VH_KIND_BYTE_BUFFER_VIEW_LE, true));
            let at = Value::Object(Some(bb));
            return Err(vh_index_raise_through_jdk(
                ctx, this, view, mode, at, index, fallback,
            ));
        }
    };
    let width = byte_view_width(elem) as i64;
    if let Some(addr) = byte_buffer_view_addr(ctx, bb, idx) {
        if addr & (width - 1) != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: format!("Misaligned access at index: {index}"),
            }
            .into());
        }
    }
    Ok(())
}

/// Whether [`vh_check_arity`]'s kind is a `ByteBuffer` view's.
#[inline]
pub(crate) fn is_byte_buffer_view_kind(kind: Option<i32>) -> bool {
    matches!(
        kind,
        Some(VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE)
    )
}

/// A read-modify-write access mode on a `ByteBuffer` view.
#[derive(Clone, Copy)]
pub(crate) enum VhViewRmw {
    CompareAndSet,
    CompareAndExchange,
    GetAndSet,
    GetAndAdd,
    Bitwise(VhBitOp),
}

/// A read-modify-write access through a `ByteBuffer` view, as one hardware
/// atomic on the direct buffer's memory. [`vh_view_mode_refusal`] has already
/// refused every mode, buffer and index HotSpot refuses, so the buffer here is
/// direct and the element aligned.
///
/// Before this, the natives' kind switches did not know the buffer kinds:
/// `compareAndSet` answered `false`, `compareAndExchange`/`getAndSet` `null`,
/// `getAndAdd`/`getAndBitwise*` `0`, and none of them wrote -- so a CAS retry
/// loop over a direct buffer never ended.
///
/// The word is compared and swapped in the buffer's own byte order: the
/// expected and new values are encoded with [`byte_view_encode`] (which
/// honours the view's endianness), so the atomic compares the bytes memory
/// holds. A `float`/`double` compares its raw bits, as HotSpot's does. A
/// buffer whose memory is not an Unsafe-arena block this VM can address
/// directly is `UnsupportedOperationException` rather than a silent no-op.
pub(crate) fn byte_buffer_view_rmw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhViewRmw,
) -> MethodCallResult {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    let this = obj_arg(args, 0)?;
    let meta = vh_meta_get(ctx, this);
    let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) else {
        return Ok(Some(Value::Object(None)));
    };
    let mut bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    let index = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let idx = byte_buffer_view_check_index(ctx, bb, index, elem)?;
    let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
    let width = byte_view_width(elem);
    let refuse = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: "atomic access to this buffer's memory is not supported".to_string(),
        }
        .into()
    };
    let addr = byte_buffer_view_addr(ctx, bb, idx).ok_or_else(refuse)?;
    let ptr =
        crate::unsafe_natives_ext::unsafe_arena_access_ptr(addr, width).ok_or_else(refuse)?;
    if (ptr as usize) % width != 0 || !(width == 4 || width == 8) {
        return Err(refuse());
    }
    // `word(v)`: the bytes `v` occupies in the buffer, read as a native word.
    let word = |v: &Value| -> u64 {
        let bytes = byte_view_encode(elem, le, v);
        if width == 4 {
            u64::from(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        } else {
            u64::from_ne_bytes(bytes)
        }
    };
    let value = |w: u64| -> Value {
        if width == 4 {
            byte_view_decode(elem, le, &(w as u32).to_ne_bytes())
        } else {
            byte_view_decode(elem, le, &w.to_ne_bytes())
        }
    };
    // SAFETY: `unsafe_arena_real_ptr_bounded` answered for at least `width`
    // live bytes, and the pointer is `width`-aligned (checked above).
    let cas = |current: u64, new: u64| -> Result<u64, u64> {
        if width == 4 {
            let cell = unsafe { AtomicU32::from_ptr(ptr as *mut u32) };
            cell.compare_exchange(
                current as u32,
                new as u32,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map(u64::from)
            .map_err(u64::from)
        } else {
            let cell = unsafe { AtomicU64::from_ptr(ptr as *mut u64) };
            cell.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
        }
    };
    let load = || -> u64 {
        if width == 4 {
            u64::from(unsafe { AtomicU32::from_ptr(ptr as *mut u32) }.load(Ordering::SeqCst))
        } else {
            unsafe { AtomicU64::from_ptr(ptr as *mut u64) }.load(Ordering::SeqCst)
        }
    };
    let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Int(0));
    match op {
        VhViewRmw::CompareAndSet => {
            let ok = cas(word(&arg(3)), word(&arg(4))).is_ok();
            Ok(Some(Value::Int(ok as i32)))
        }
        VhViewRmw::CompareAndExchange => {
            let witness = match cas(word(&arg(3)), word(&arg(4))) {
                Ok(w) | Err(w) => w,
            };
            Ok(Some(value(witness)))
        }
        VhViewRmw::GetAndSet | VhViewRmw::GetAndAdd | VhViewRmw::Bitwise(_) => {
            let operand = arg(3);
            let mut current = load();
            loop {
                let old = value(current);
                let new = match (op, &old, &operand) {
                    (VhViewRmw::GetAndSet, _, v) => *v,
                    (VhViewRmw::GetAndAdd, Value::Int(a), Value::Int(b)) => {
                        Value::Int(a.wrapping_add(*b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Long(b)) => {
                        Value::Long(a.wrapping_add(*b))
                    }
                    (VhViewRmw::GetAndAdd, Value::Long(a), Value::Int(b)) => {
                        Value::Long(a.wrapping_add(i64::from(*b)))
                    }
                    (VhViewRmw::Bitwise(bit), Value::Int(a), Value::Int(b)) => {
                        Value::Int(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    (VhViewRmw::Bitwise(bit), Value::Long(a), Value::Long(b)) => {
                        Value::Long(match bit {
                            VhBitOp::Or => a | b,
                            VhBitOp::And => a & b,
                            VhBitOp::Xor => a ^ b,
                        })
                    }
                    _ => return Err(refuse()),
                };
                match cas(current, word(&new)) {
                    Ok(_) => return Ok(Some(old)),
                    Err(witness) => current = witness,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `IndirectVarHandle` (round 14 wave 7, lane ffm7; proposal FFM6-1)
// ---------------------------------------------------------------------------
//
// Every VarHandle combinator (`MethodHandles.filterValue`, `filterCoordinates`,
// `insertCoordinates`, `collectCoordinates`, `dropCoordinates`,
// `permuteCoordinates`, and the FFM layout paths built on them) runs JDK
// bytecode that returns a `java.lang.invoke.IndirectVarHandle`. None of the
// access natives below knew the class: no side-table row, slot 0 is the real
// `vform`, so each fell to its positional `VH_KIND` read and answered `null` /
// stored nothing -- silently -- and an `(array, int, ...)` call shape was
// hijacked by `vh_array_call` as a raw array access
// (`r14w6-ffm6-indirect-var-handle-accesses-dropped-FIXED-20260929.md`).
//
// Served the way HotSpot serves it (`IndirectVarHandle.getMethodHandleUncached`
// and the `VarHandleGuards` indirect arm): the mode handle is
// `handleFactory.apply(mode, <target's mode handle>)`, typed
// `(VarHandle, coordinates..., values...)R`, invoked with `directTarget`
// leading. A non-indirect target's mode handle is the mode's own invoker --
// the handle `varhandle_to_method_handle` builds before its `bindTo` -- and an
// indirect target recurses. The composed handle is cached where the JDK caches
// it, in the handle's own `VarHandle.methodHandleTable[mode.ordinal()]`.
// Anything that cannot be served THROWS `UnsupportedOperationException`; it
// never answers `null`.

pub(crate) const INDIRECT_VAR_HANDLE_CLASS: &str = "java/lang/invoke/IndirectVarHandle";
pub(crate) const VAR_HANDLE_CLASS: &str = "java/lang/invoke/VarHandle";

/// `VarHandle.AccessMode` in ordinal order, `(enum name, method name)`: the
/// index is the ordinal and the `methodHandleTable` slot. The same 31 modes in
/// the same order on every JDK 9+ image.
pub(crate) const VH_ACCESS_MODES: [(&str, &str); 31] = [
    ("GET", "get"),
    ("SET", "set"),
    ("GET_VOLATILE", "getVolatile"),
    ("SET_VOLATILE", "setVolatile"),
    ("GET_ACQUIRE", "getAcquire"),
    ("SET_RELEASE", "setRelease"),
    ("GET_OPAQUE", "getOpaque"),
    ("SET_OPAQUE", "setOpaque"),
    ("COMPARE_AND_SET", "compareAndSet"),
    ("COMPARE_AND_EXCHANGE", "compareAndExchange"),
    ("COMPARE_AND_EXCHANGE_ACQUIRE", "compareAndExchangeAcquire"),
    ("COMPARE_AND_EXCHANGE_RELEASE", "compareAndExchangeRelease"),
    ("WEAK_COMPARE_AND_SET_PLAIN", "weakCompareAndSetPlain"),
    ("WEAK_COMPARE_AND_SET", "weakCompareAndSet"),
    ("WEAK_COMPARE_AND_SET_ACQUIRE", "weakCompareAndSetAcquire"),
    ("WEAK_COMPARE_AND_SET_RELEASE", "weakCompareAndSetRelease"),
    ("GET_AND_SET", "getAndSet"),
    ("GET_AND_SET_ACQUIRE", "getAndSetAcquire"),
    ("GET_AND_SET_RELEASE", "getAndSetRelease"),
    ("GET_AND_ADD", "getAndAdd"),
    ("GET_AND_ADD_ACQUIRE", "getAndAddAcquire"),
    ("GET_AND_ADD_RELEASE", "getAndAddRelease"),
    ("GET_AND_BITWISE_OR", "getAndBitwiseOr"),
    ("GET_AND_BITWISE_OR_RELEASE", "getAndBitwiseOrRelease"),
    ("GET_AND_BITWISE_OR_ACQUIRE", "getAndBitwiseOrAcquire"),
    ("GET_AND_BITWISE_AND", "getAndBitwiseAnd"),
    ("GET_AND_BITWISE_AND_RELEASE", "getAndBitwiseAndRelease"),
    ("GET_AND_BITWISE_AND_ACQUIRE", "getAndBitwiseAndAcquire"),
    ("GET_AND_BITWISE_XOR", "getAndBitwiseXor"),
    ("GET_AND_BITWISE_XOR_RELEASE", "getAndBitwiseXorRelease"),
    ("GET_AND_BITWISE_XOR_ACQUIRE", "getAndBitwiseXorAcquire"),
];

/// The `AccessType.SET` modes (`set`, `setVolatile`, `setRelease`,
/// `setOpaque`): the access natives answer `void` for them.
pub(crate) fn vh_access_mode_is_set(ordinal: usize) -> bool {
    matches!(ordinal, 1 | 3 | 5 | 7)
}

/// Deepest combinator-over-combinator chain followed; a real one is a
/// handful deep, so this only bounds a malformed graph.
pub(crate) const VH_INDIRECT_MAX_DEPTH: u32 = 64;

/// `CRATONVM_VH_INDIRECT_SERVE=0`: an `IndirectVarHandle` access is REFUSED
/// (`UnsupportedOperationException`) instead of served through its
/// `handleFactory`, and `varType()`/`coordinateTypes()` refuse as before.
/// Default on, both modes (round 14 wave 7, lane ffm7).
pub(crate) fn vh_indirect_serve_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_SERVE"))
}

/// `CRATONVM_VH_INDIRECT_REFUSE=0`, together with
/// `CRATONVM_VH_INDIRECT_SERVE=0`: restore the pre-wave-7 answer (the access
/// falls through to the generic arms: `null` / nothing stored). Default on,
/// both modes; it has no effect while serving is on.
pub(crate) fn vh_indirect_refuse_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_REFUSE")
    })
}

pub(crate) fn is_indirect_var_handle(ctx: &mut dyn NativeContext, vh: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(vh))
        .as_deref()
        == Some(INDIRECT_VAR_HANDLE_CLASS)
}

pub(crate) fn vh_indirect_refusal(method: &str, why: &str) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!("IndirectVarHandle.{method}: {why}"),
    }
    .into()
}

/// A reference field of an `IndirectVarHandle` (a real JDK class, so its own
/// layout, resolved by name).
pub(crate) fn indirect_vh_field(ctx: &mut dyn NativeContext, vh: ObjectRef, field: &str) -> Option<ObjectRef> {
    let idx = ctx.resolve_field_index(INDIRECT_VAR_HANDLE_CLASS, field)?;
    match ctx.get_field(vh, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The access natives' first step: `Some(answer)` when `args[0]` is an
/// `IndirectVarHandle`, `None` for every other handle. `method` is the access
/// mode's method name (`get`, `compareAndSet`, ...), as each native passes it.
///
/// Placed before the arity, read-only, null-coordinate and value-adaptation
/// checks: those judge the handle by this file's shapes, and an indirect
/// handle's own coordinates are the adapter's to check.
///
/// Cost for every other handle: the side-table memo the native reads anyway,
/// and a class-name compare only for a handle with no row.
///
/// With no parameter-site word (nothing armed). Since round 15 wave 3 (lane
/// vh3; VH1-3) every access-mode native, read modes included, takes the word
/// and calls [`vh_indirect_serve_site`]; this form is the tests' spelling.
#[cfg(test)]
pub(crate) fn vh_indirect_serve(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> Option<MethodCallResult> {
    vh_indirect_serve_site(ctx, args, method, 0, &None)
}

/// [`vh_indirect_serve`] with the parameter-site word the native took
/// (`poly_call_site::take_vh_param_site`, `0` = none armed), which decides the
/// `asType` refusals of the trailing value parameters (round 15 wave 1, lane
/// vh) and, since round 15 wave 3 (lane vh3; VH1-3), of the coordinates it
/// covers ([`vh_indirect_value_site_refusal`]); and over the row the prologue
/// read (interpreter wave 47, [`VARHANDLE_ONE_ROW_READ`]; `&None` = not read,
/// ask the side table).
pub(crate) fn vh_indirect_serve_site(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    site: u64,
    row: &VhPrologueRow,
) -> Option<MethodCallResult> {
    let vh = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let has_row = match row {
        Some(meta) => meta.is_some(),
        None => vh_meta_get(ctx, vh).is_some(),
    };
    if has_row || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    if !vh_indirect_serve_enabled() && !vh_indirect_refuse_enabled() {
        return None;
    }
    Some(vh_indirect_serve_on(ctx, vh, args, method, site))
}

pub(crate) fn vh_indirect_serve_on(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    method: &str,
    site: u64,
) -> MethodCallResult {
    let Some(ordinal) = VH_ACCESS_MODES.iter().position(|(_, m)| *m == method) else {
        return Err(vh_indirect_refusal(method, "unknown access mode"));
    };
    // Wave 57 (lane L4): `asType`'s return direction against the combinator's
    // own variable type (its `value`), before the access, as the direct
    // handles' natives judge theirs (`vh_return_site_check`).
    if matches!(vh_access_type_of_method(method), Some(0 | 3 | 4)) {
        let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
        if ret != 0 {
            vh_return_site_check_cold(ctx, args, ret, &Some(None))?;
        }
    }
    if !vh_indirect_serve_enabled() {
        return Err(vh_indirect_refusal(
            method,
            "an access through a VarHandle combinator is not served \
             (CRATONVM_VH_INDIRECT_SERVE=0)",
        ));
    }
    // SBR5 lane avh: a repeat access of a JDK `arrayElementVarHandle` (nothing allocated or
    // run in Java: the verdict and screens of its first access are filed).
    if let Some(answer) = vh_array_element_early(ctx, vh, args, ordinal, site) {
        return answer;
    }
    // Everything below allocates or runs Java: pin the handle and every
    // reference argument, and re-read each before use.
    let base = ctx.pin_native_root(vh);
    let mut arg_pins: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for v in &args[1..] {
        arg_pins.push(match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        });
    }
    let result = vh_indirect_dispatch(ctx, base, vh, args, &arg_pins, ordinal, site);
    ctx.unpin_native_roots(base);
    result
}

/// A primitive argument boxed for `invokeWithArguments`: as the declared
/// parameter's primitive when the call site's value converts to it (an `int`
/// slot also carries `boolean`/`byte`/`short`/`char`; `asType` widens `int`
/// to `long`/`float`/`double`), else as its own kind. `None` for a reference.
pub(crate) fn vh_indirect_box_input(value: Value, declared: Option<&str>) -> Option<(Value, &'static str)> {
    Some(match (value, declared.unwrap_or("")) {
        (Value::Int(_), "Z") => (value, "Z"),
        (Value::Int(_), "B") => (value, "B"),
        (Value::Int(_), "S") => (value, "S"),
        (Value::Int(_), "C") => (value, "C"),
        (Value::Int(i), "J") => (Value::Long(i64::from(i)), "J"),
        (Value::Int(i), "F") => (Value::Float(i as f32), "F"),
        (Value::Int(i), "D") => (Value::Double(f64::from(i)), "D"),
        (Value::Long(l), "F") => (Value::Float(l as f32), "F"),
        (Value::Long(l), "D") => (Value::Double(l as f64), "D"),
        (Value::Float(f), "D") => (Value::Double(f64::from(f)), "D"),
        (Value::Int(_), _) => (value, "I"),
        (Value::Long(_), _) => (value, "J"),
        (Value::Float(_), _) => (value, "F"),
        (Value::Double(_), _) => (value, "D"),
        _ => return None,
    })
}

/// `CRATONVM_VH_INDIRECT_PARAMS_MEMO` (default on, both modes; round 15 wave
/// 2, lane vh2; VH1-1 in `jit-r15-vh-proposals-RETIRED-20261003.md`): a composed mode handle's
/// declared parameter list is rendered once per `MethodType` and thread
/// ([`vh_indirect_declared_params`]) instead of on every access. `0` renders
/// per access again.
pub(crate) fn vh_indirect_params_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_PARAMS_MEMO")
    })
}

thread_local! {
    /// VH1-1: [`vh_indirect_declared_params`]'s answers, keyed like
    /// [`vh_memo_lookup`]'s lines (VM, identity hash, address,
    /// [`VH_META_GENERATION`]) but by the mode handle's `MethodType`, whose
    /// `ptypes` are final: the parameter list is a pure function of it.
    pub(crate) static VH_INDIRECT_PARAMS_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<[String]>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// The declared parameter list of the mode handle `mh`, read from its `type`
/// field only (what [`mh_declared_descriptor`] renders), or `None` when it has
/// none or it does not render (an unknown signature, which the callers
/// accept). Memoised per thread (VH1-1): the render is a mirror-to-name
/// resolution and a `String` per parameter, paid on every `IndirectVarHandle`
/// access before this.
pub(crate) fn vh_indirect_declared_params(
    ctx: &mut dyn NativeContext,
    mh: ObjectRef,
) -> Option<Arc<[String]>> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    if !vh_indirect_params_memo_enabled() {
        return vh_method_type_params(ctx, mt);
    }
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(mt);
    let addr = mt.as_ptr() as usize;
    // Read BEFORE the render, as `vh_memo_lookup` does: a render that moves
    // `mt` files its line under a generation no later lookup matches.
    let generation = VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let slot = (hash as usize) & (VH_PLAN_MEMO_SLOTS - 1);
    let hit = VH_INDIRECT_PARAMS_MEMO.with(|memo| {
        let memo = memo.borrow();
        memo.get(slot).and_then(|line| {
            let same = line.vm == vm
                && line.key == hash
                && line.addr == addr
                && line.generation == generation;
            if same {
                line.value.clone()
            } else {
                None
            }
        })
    });
    if hit.is_some() {
        return hit;
    }
    let params = vh_method_type_params(ctx, mt)?;
    VH_INDIRECT_PARAMS_MEMO.with(|memo| {
        if let Some(line) = memo.borrow_mut().get_mut(slot) {
            *line = VhMemoLine {
                vm,
                key: hash,
                addr,
                generation,
                value: Some(Arc::clone(&params)),
            };
        }
    });
    Some(params)
}

/// The parameter descriptors of the `MethodType` `mt`, uncached.
pub(crate) fn vh_method_type_params(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<Arc<[String]>> {
    let desc = methodtype_to_descriptor(ctx, mt)?;
    Some(Arc::from(
        crate::lang_class::parse_descriptor_param_and_return(&desc).0,
    ))
}

pub(crate) fn vh_indirect_dispatch(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    args: &[Value],
    arg_pins: &[Option<usize>],
    ordinal: usize,
    site: u64,
) -> MethodCallResult {
    let method = VH_ACCESS_MODES[ordinal].1;
    let mh = vh_indirect_mode_handle(ctx, vh, ordinal, 0)?;
    let mh_pin = ctx.pin_native_root(mh);
    let vh = ctx.read_native_pin(vh_pin, vh);
    let Some(direct) = indirect_vh_field(ctx, vh, "directTarget") else {
        return Err(vh_indirect_refusal(
            method,
            "the handle has no directTarget",
        ));
    };
    let direct_pin = ctx.pin_native_root(direct);
    let mh = ctx.read_native_pin(mh_pin, mh);
    let params: Option<Arc<[String]>> = vh_indirect_declared_params(ctx, mh);
    // The mode handle's parameters are `(VarHandle, coordinates..., values...)`
    // and `args` is `(handle, coordinates..., values...)`: one for one.
    if let Some(params) = &params {
        if params.len() != args.len() {
            let msg = format!(
                "cannot convert IndirectVarHandle.{method}: the call site passes {} argument(s) \
                 after the VarHandle, the access mode takes {}",
                args.len() - 1,
                params.len().saturating_sub(1)
            );
            // The door rewrites the text with both method types (wave 53, L4).
            let vh = ctx.read_native_pin(vh_pin, vh);
            return Err(vh_astype_refused(ctx, vh, |ctx| {
                crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &msg,
                )
            }));
        }
        // SBR5 lane avh: a REAL composed mode handle is always screened for the typed
        // lane (below); the carrier's screen stays opt-in.
        let real_mode_handle = mh_is_real_handle(&*ctx, ctx.read_native_pin(mh_pin, mh));
        if site != 0 || vh_indirect_typed_enabled() || real_mode_handle {
            // `args`' references as they are NOW (composing the mode handle
            // ran Java); reading a pin does not allocate. Judged, never stored.
            let mut live: Vec<Value> = Vec::with_capacity(args.len());
            live.push(Value::Object(Some(ctx.read_native_pin(vh_pin, vh))));
            for (i, v) in args.iter().enumerate().skip(1) {
                live.push(match (v, arg_pins.get(i - 1).copied().flatten()) {
                    (Value::Object(Some(o)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, *o)))
                    }
                    (other, _) => *other,
                });
            }
            // The value parameters' static call-site types (round 15 wave 1,
            // lane vh): `asType` refuses `int -> char`, which an `int` slot
            // cannot show on its own.
            if let Some(refusal) = vh_indirect_value_site_refusal(ctx, &live, params, ordinal, site)
            {
                return Err(refusal);
            }
            // F7-1: the mode handle invoked at its own type, with no
            // `Object[]` and no argument boxes, when every argument already is
            // that type.
            if vh_indirect_typed_enabled() || real_mode_handle {
                if let Some(typed) = vh_indirect_typed_args(ctx, &live, params) {
                    let mh = ctx.read_native_pin(mh_pin, mh);
                    if vh_indirect_typed_admits_handle(ctx, mh) {
                        let mut call: Vec<Value> = Vec::with_capacity(args.len());
                        call.push(Value::Object(Some(ctx.read_native_pin(direct_pin, direct))));
                        for (i, v) in typed.into_iter().enumerate() {
                            call.push(match (v, arg_pins.get(i).copied().flatten()) {
                                (Value::Object(Some(o)), Some(pin)) => {
                                    Value::Object(Some(ctx.read_native_pin(pin, o)))
                                }
                                (other, _) => other,
                            });
                        }
                        let mh = ctx.read_native_pin(mh_pin, mh);
                        // No call site: the arguments are the declared types,
                        // and the door boxes a primitive result against the
                        // declared return, the shape `invokeWithArguments`
                        // answers.
                        let result = mh_invoke_exact_door(ctx, mh, &call, None, false)?;
                        if vh_access_mode_is_set(ordinal) {
                            return Ok(None);
                        }
                        return Ok(Some(result.unwrap_or(Value::Object(None))));
                    }
                    // SBR5 lane avh: a REAL composed mode handle (the JDK's own
                    // `IndirectVarHandle` over `collectCoordinates` etc.) invoked at
                    // its own type: its spun entry runs with the arguments as they
                    // are, with no `Object[]`, no `asType` and no JDK
                    // `invokeWithArguments` (the 44 us per get+set pair of
                    // `arrayElementVarHandle`'s bytecode road).
                    if real_mode_handle {
                        let mut call: Vec<Value> = Vec::with_capacity(args.len());
                        call.push(Value::Object(Some(ctx.read_native_pin(direct_pin, direct))));
                        for (i, v) in typed.into_iter().enumerate() {
                            call.push(match (v, arg_pins.get(i).copied().flatten()) {
                                (Value::Object(Some(o)), Some(pin)) => {
                                    Value::Object(Some(ctx.read_native_pin(pin, o)))
                                }
                                (other, _) => other,
                            });
                        }
                        // The JDK's `MemoryLayout.arrayElementVarHandle` (a
                        // `collectCoordinates` of the layout's scale over its segment
                        // handle): the scale is `offset + index * byteSize`, so the
                        // access is the segment handle's own at that offset.
                        if let Some(answer) =
                            vh_array_element_forward(ctx, vh_pin, vh, ordinal, &call[1..], params)
                        {
                            return answer;
                        }
                        let mh = ctx.read_native_pin(mh_pin, mh);
                        if let Some(desc) = mh_real_declared_descriptor(ctx, mh) {
                            let lane =
                                mh_real_direct_fast(ctx, mh, &call, Some(&desc), false, false)
                                    .or_else(|| {
                                        mh_real_lambda_form_dispatch(
                                            ctx,
                                            mh,
                                            &call,
                                            Some(&desc),
                                            false,
                                        )
                                    });
                            if let Some(result) = lane {
                                let result = auto_box_return(ctx, result, &desc)?;
                                if vh_access_mode_is_set(ordinal) {
                                    return Ok(None);
                                }
                                return Ok(Some(result.unwrap_or(Value::Object(None))));
                            }
                        }
                    }
                }
            }
        }
    }
    // `invokeWithArguments` is `asType(genericMethodType(n))` and an exact
    // invocation: it unboxes and casts each argument to the handle's declared
    // type, so the call-site conversions happen there, not here.
    let mut elems: Vec<(Value, Option<usize>)> = Vec::with_capacity(args.len());
    for (i, v) in args.iter().enumerate().skip(1) {
        let pin = arg_pins.get(i - 1).copied().flatten();
        if let Value::Object(Some(o)) = v {
            elems.push((Value::Object(Some(*o)), pin));
            continue;
        }
        let declared_param = params.as_ref().and_then(|p| p.get(i)).map(String::as_str);
        match vh_indirect_box_input(*v, declared_param) {
            Some((value, desc)) => match box_value_canonical(ctx, value, desc) {
                Value::Object(Some(b)) => {
                    let pin = ctx.pin_native_root(b);
                    elems.push((Value::Object(Some(b)), Some(pin)));
                }
                other => elems.push((other, None)),
            },
            None => elems.push((*v, None)),
        }
    }
    let arr = ctx.new_array(ArrayElementType::Reference, args.len());
    let direct = ctx.read_native_pin(direct_pin, direct);
    ctx.set_array_element(arr, 0, Value::Object(Some(direct)));
    for (i, (v, pin)) in elems.iter().enumerate() {
        let v = match (v, pin) {
            (Value::Object(Some(o)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(*pin, *o)))
            }
            _ => *v,
        };
        ctx.set_array_element(arr, i + 1, v);
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    let result = ctx.invoke_virtual(
        mh,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(arr))],
    )?;
    if vh_access_mode_is_set(ordinal) {
        return Ok(None);
    }
    // A primitive result comes back boxed, the shape every access native here
    // answers with; the call site's descriptor unboxes it.
    Ok(Some(result.unwrap_or(Value::Object(None))))
}

/// The mode handle for `vh` and the access mode `ordinal`: an indirect handle's
/// composed (and cached) adapter, or a direct handle's invoker.
pub(crate) fn vh_indirect_mode_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    ordinal: usize,
    depth: u32,
) -> Result<ObjectRef, MethodCallFailed> {
    let method = VH_ACCESS_MODES[ordinal].1;
    if depth > VH_INDIRECT_MAX_DEPTH {
        return Err(vh_indirect_refusal(method, "combinator chain too deep"));
    }
    let indirect = is_indirect_var_handle(ctx, vh);
    if indirect {
        if let Some(mh) = vh_indirect_cached_mode_handle(ctx, vh, ordinal) {
            return Ok(mh);
        }
    }
    let base = ctx.pin_native_root(vh);
    let result = if indirect {
        vh_indirect_compose_mode_handle(ctx, base, vh, ordinal, depth)
    } else if vh_real_adoption_enabled() {
        // The family is real (SBR3 mhperf): the JDK's own mode handle, a real `MethodHandle`
        // the real combinators of the `handleFactory` can edit. The carrier invoker below is
        // not one (`NoSuchMethodError: MethodHandle.editor()` in `MethodHandles.collectArguments`).
        vh_real_mode_handle(ctx, base, vh, ordinal)
    } else {
        vh_direct_mode_invoker(ctx, base, vh, ordinal)
    };
    ctx.unpin_native_roots(base);
    result
}

/// `target.getMethodHandle(ordinal)`: HotSpot's own mode handle of a direct `VarHandle`, typed
/// `(VarHandle, accessModeType(mode)...)`, unbound (the JDK's `IndirectVarHandle.
/// getMethodHandleUncached` asks the same of its target). The caller pinned `vh`.
pub(crate) fn vh_real_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let vh = ctx.read_native_pin(vh_pin, vh);
    match ctx.invoke_virtual_bytecode_only(
        vh,
        "getMethodHandle",
        "(I)Ljava/lang/invoke/MethodHandle;",
        &[Value::Int(ordinal as i32)],
    )? {
        Some(Value::Object(Some(mh))) => Ok(mh),
        _ => Err(vh_indirect_refusal(method, "getMethodHandle answered null")),
    }
}

pub(crate) fn vh_indirect_cached_mode_handle(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    ordinal: usize,
) -> Option<ObjectRef> {
    let idx = ctx.resolve_field_index(VAR_HANDLE_CLASS, "methodHandleTable")?;
    let Value::Object(Some(table)) = ctx.get_field(vh, idx) else {
        return None;
    };
    if !matches!(ctx.heap_kind_of(table), ObjectKind::Array) || ctx.array_length(table) <= ordinal {
        return None;
    }
    match ctx.get_array_element(table, ordinal) {
        Value::Object(Some(mh)) => Some(mh),
        _ => None,
    }
}

/// `handleFactory.apply(mode, <target's mode handle>)`, cached. The caller
/// pinned `vh` at `vh_pin` and unpins everything pinned here.
pub(crate) fn vh_indirect_compose_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    depth: u32,
) -> Result<ObjectRef, MethodCallFailed> {
    let (mode_name, method) = VH_ACCESS_MODES[ordinal];
    let Some(target) = indirect_vh_field(ctx, vh, "target") else {
        return Err(vh_indirect_refusal(method, "the handle has no target"));
    };
    let target_mh = vh_indirect_mode_handle(ctx, target, ordinal, depth + 1)?;
    let target_mh_pin = ctx.pin_native_root(target_mh);
    let mode = vh_access_mode_constant(ctx, mode_name, method)?;
    let mode_pin = ctx.pin_native_root(mode);
    let vh = ctx.read_native_pin(vh_pin, vh);
    let Some(factory) = indirect_vh_field(ctx, vh, "handleFactory") else {
        return Err(vh_indirect_refusal(
            method,
            "the handle has no handleFactory",
        ));
    };
    let target_mh = ctx.read_native_pin(target_mh_pin, target_mh);
    let mode = ctx.read_native_pin(mode_pin, mode);
    let mh = match ctx.invoke_virtual(
        factory,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(mode)), Value::Object(Some(target_mh))],
    )? {
        Some(Value::Object(Some(mh))) => mh,
        _ => {
            return Err(vh_indirect_refusal(
                method,
                "the handleFactory answered null",
            ))
        }
    };
    let mh_pin = ctx.pin_native_root(mh);
    vh_indirect_cache_mode_handle(ctx, vh_pin, vh, ordinal, mh_pin, mh);
    Ok(ctx.read_native_pin(mh_pin, mh))
}

/// Store a composed mode handle in `vh`'s `methodHandleTable`, allocating the
/// table (a `MethodHandle[AccessMode.COUNT]`, as `VarHandle.getMethodHandle`
/// does) when absent. Only ever called for an `IndirectVarHandle`, whose
/// fields are the real class's: a CratonVM-minted handle imposes its own
/// meaning on the slots and must not be written here.
pub(crate) fn vh_indirect_cache_mode_handle(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    mh_pin: usize,
    mh: ObjectRef,
) {
    let Some(idx) = ctx.resolve_field_index(VAR_HANDLE_CLASS, "methodHandleTable") else {
        return;
    };
    let vh = ctx.read_native_pin(vh_pin, vh);
    let table = match ctx.get_field(vh, idx) {
        Value::Object(Some(t)) => t,
        // No table yet (an unwritten reference slot may also read back as
        // `Int(0)`, the zeroed slot's niche).
        _ => {
            let t = match ctx.class_id_by_name("java/lang/invoke/MethodHandle") {
                Some(cid) => ctx.new_ref_array(cid, VH_ACCESS_MODES.len()),
                None => ctx.new_array(ArrayElementType::Reference, VH_ACCESS_MODES.len()),
            };
            let vh = ctx.read_native_pin(vh_pin, vh);
            ctx.set_field(vh, idx, Value::Object(Some(t)));
            t
        }
    };
    if !matches!(ctx.heap_kind_of(table), ObjectKind::Array) || ctx.array_length(table) <= ordinal {
        return;
    }
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.set_array_element(table, ordinal, Value::Object(Some(mh)));
}

/// A direct handle's mode invoker: the access mode's method on
/// `java/lang/invoke/VarHandle`, typed `(VarHandle, accessModeType(mode)...)`,
/// unbound -- `varhandle_to_method_handle`'s handle before its `bindTo`, and
/// HotSpot's `VarHandle.getMethodHandleUncached`. The caller pinned `vh`.
pub(crate) fn vh_direct_mode_invoker(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let (mode_name, method) = VH_ACCESS_MODES[ordinal];
    let mode = vh_access_mode_constant(ctx, mode_name, method)?;
    let vh = ctx.read_native_pin(vh_pin, vh);
    let mode_type = match ctx.invoke_virtual(
        vh,
        "accessModeType",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(mode))],
    )? {
        Some(Value::Object(Some(mt))) => mt,
        _ => {
            return Err(vh_indirect_refusal(
                method,
                "the target has no access-mode type",
            ))
        }
    };
    let desc = match ctx.invoke_virtual(
        mode_type,
        "toMethodDescriptorString",
        "()Ljava/lang/String;",
        &[],
    )? {
        Some(Value::Object(Some(d))) => ctx.read_string(d).unwrap_or_default(),
        _ => String::new(),
    };
    if desc.is_empty() {
        return Err(vh_indirect_refusal(
            method,
            "the target's access-mode type has no descriptor",
        ));
    }
    alloc_method_handle(ctx, VAR_HANDLE_CLASS, method, &desc, MH_KIND_VIRTUAL)
}

/// The `VarHandle.AccessMode` constant named `mode_name` (`GET`, ...).
pub(crate) fn vh_access_mode_constant(
    ctx: &mut dyn NativeContext,
    mode_name: &str,
    method: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let name = ctx.create_string_uninterned(mode_name);
    match ctx.invoke(
        "java/lang/invoke/VarHandle$AccessMode",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/invoke/VarHandle$AccessMode;",
        &[Value::Object(Some(name))],
    )? {
        Some(Value::Object(Some(mode))) => Ok(mode),
        _ => Err(vh_indirect_refusal(
            method,
            "no VarHandle.AccessMode constant",
        )),
    }
}

/// `varType()` of an `IndirectVarHandle`: its own `value` field, which is
/// what HotSpot's `accessModeTypeUncached` returns for `GET`. Without it a
/// combinator over a combinator (`insertCoordinates(filterValue(..), ..)`)
/// failed at construction: every combinator asks its target `varType()`.
pub(crate) fn indirect_vh_var_type(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<ObjectRef> {
    if !vh_indirect_serve_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    indirect_vh_field(ctx, vh, "value")
}

/// `coordinateTypes()` of an `IndirectVarHandle`: `List.of(coordinates)` (the
/// list copies the array), for the reason [`indirect_vh_var_type`] gives.
pub(crate) fn indirect_vh_coordinate_types(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<Value> {
    if !vh_indirect_serve_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    let coords = indirect_vh_field(ctx, vh, "coordinates")?;
    match ctx.invoke(
        "java/util/List",
        "of",
        "([Ljava/lang/Object;)Ljava/util/List;",
        &[Value::Object(Some(coords))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `IndirectVarHandle`: round 15 wave 1 (lane vh)
// ---------------------------------------------------------------------------
//
// `r14w7-ffm7-indirect-var-handle-residuals-FIXED-20260930.md`:
//
// * item 3: `MethodHandles.filterValue` composes its GET / GET_AND_UPDATE /
//   COMPARE_AND_EXCHANGE mode handles with the package-private
//   `MethodHandles.collectReturnValue`, whose JDK body is
//   `target.rebind().editor()...`: a CratonVM handle has no `LambdaForm`
//   editor (`NoSuchMethodError: MethodHandle.editor()`). Served here for
//   every caller, as the composition of two combinators this file already
//   serves ([`mhs_collect_return_value`]).
// * item 1: an exact `IndirectVarHandle` (its real `withInvokeExactBehavior`
//   bytecode) never opened the doors' exact-handle gate, and
//   `varhandle_exact_call_site_refusal` could not describe it
//   ([`indirect_vh_note_exact`], [`indirect_vh_exact_access_descriptor`]).
// * item 2: an `int` slot narrowed to a `char`/`short`/`byte`/`boolean` value
//   parameter, judged from the value-site cell
//   ([`vh_indirect_value_site_refusal`]).
// * proposal F7-1 (`jit-r14-ffm7-proposals-RETIRED-20260930.md`): the mode handle invoked at its
//   own type through the `invokeExact` door, with no `Object[]` and no
//   argument boxes ([`vh_indirect_typed_args`]), opt-in.

/// `CRATONVM_MH_COLLECT_RETURN_VALUE` (default on, both modes; round 15 wave
/// 1, lane vh): `MethodHandles.collectReturnValue` is served by
/// [`mhs_collect_return_value`]. `0` runs the JDK body again (the
/// `NoSuchMethodError: MethodHandle.editor()` of every `filterValue` read).
pub(crate) fn mh_collect_return_value_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_COLLECT_RETURN_VALUE")
    })
}

/// `CRATONVM_VH_INDIRECT_EXACT` (default on; round 15 wave 1, lane vh): an
/// exact `IndirectVarHandle` is judged at the call site as HotSpot judges it
/// (`WrongMethodTypeException` for a type that is not exactly the access
/// mode's), in the modes [`vh_exact_judged_in_this_mode`] admits. `0`
/// restores the unjudged access.
pub(crate) fn vh_indirect_exact_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_EXACT"))
}

/// `CRATONVM_VH_EXACT_COMPATIBLE` (default on; round 15 wave 1 hand-back, lane
/// vh): `--compatible` answers `withInvokeExactBehavior()` /
/// `withInvokeBehavior()` and judges exact handles as `--jdk-only` does
/// ([`varhandle_with_exactness`], [`indirect_vh_note_exact`]). `0` restores
/// the pre-round-15 `--compatible` answer (`this`, never judged). No effect
/// under `--jdk-only`.
pub(crate) fn vh_exact_compatible_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_EXACT_COMPATIBLE")
    })
}

/// Whether this VM's policy mints exact `VarHandle`s and opens the doors'
/// exact-handle gate: `--jdk-only` always, `--compatible` unless
/// [`vh_exact_compatible_enabled`] is off.
pub(crate) fn vh_exact_judged_in_this_mode(ctx: &dyn NativeContext) -> bool {
    // `JdkInvoke` (nio21 wave 12, L3): `--compatible` with the row takes
    // `--jdk-only`'s arm (`aligned` is always true under `--jdk-only`).
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke) || vh_exact_compatible_enabled()
}

/// `CRATONVM_VH_INDIRECT_SITE_COORD_WMTE` (default on, both modes; round 15
/// wave 1 hand-back, lane vh): the COORDINATE parameters the value-site cell
/// covers (a store mode's last coordinate) are judged like the values
/// ([`vh_indirect_value_site_refusal`]). `0` converts them permissively again.
pub(crate) fn vh_indirect_site_coord_wmte_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_SITE_COORD_WMTE")
    })
}

/// `CRATONVM_VH_INDIRECT_SITE_ALL_COORDS` (default on, both modes; round 15
/// wave 3, lane vh3; VH1-3): [`vh_indirect_value_site_refusal`] judges every
/// call-site parameter the parameter-site word covers (up to
/// `poly_call_site::VH_PARAM_SITE_SLOTS`), so a read mode's coordinates and a
/// store mode's earlier coordinates are judged as its last one is. `0` judges
/// the two low positions only (the round-15-wave-1 reach).
pub(crate) fn vh_indirect_site_all_coords_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_SITE_ALL_COORDS")
    })
}

/// `CRATONVM_JIT_VH_MIC_EXACT_JUDGE` (default on, both modes; round 15 wave 3,
/// lane vh3; `r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`):
/// the compiled-code virtual-call door (`vm::jit::helpers::
/// jit_invoke_virtual_mic`) judges an invoke-exact `VarHandle` against the
/// site's real descriptor once the native site cache has declined it, as the
/// generic dispatch door already does. `0` lets that door's tail run the
/// access unjudged again (the round-15-wave-2 answer). Cached here because the
/// door asks on every such access once an exact handle exists, and `vm/src`
/// may not grow a `static`.
pub fn vh_exact_mic_judge_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_MIC_EXACT_JUDGE")
    })
}

/// `CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE` (default on; round 15 wave 2, lane
/// vh2), cached (round 15 wave 3, lane vh3): `vm::jit::helpers::
/// varhandle_thin_binds_admitted` asks it on every compile once the VM has an
/// exact handle, and `vm/src` may not grow a `static`. Same name, same
/// default, same meaning as before.
pub fn vh_thin_bind_exact_gate_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE")
    })
}

/// `CRATONVM_JIT_OSR_VH_BIND_ASKS_ADMISSION` (default on; round 15 wave 4,
/// lane vh5 patch, applied by the orchestrator): under `--jdk-only` the OSR
/// door binds the thin `VarHandle` helpers when the registry admits the
/// `Bridge` bind, as the MethodEntry door does. `0` refuses them under
/// `--jdk-only` again.
pub fn vh_osr_bind_asks_admission_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_OSR_VH_BIND_ASKS_ADMISSION")
    })
}

/// `CRATONVM_JIT_SPLICE_VH_REFUSAL_ASKS_BIND` (default on, both modes; round
/// 15 wave 4, lane vh5; VH3-5,
/// `r15w3-vh3-splice-refusal-ignores-unwired-varhandle-binds-FIXED-20260930.md`):
/// the VM's splice planner refuses a callee for a `VarHandle` site only when
/// the callee's own compile will bind that site to a thin helper
/// (`vm::jit::helpers::varhandle_splice_keeps_thin_bind`), `compareAndSet`
/// included. `0` refuses every recognised read/write site again, bound or
/// not, and never a CAS one (the round-15-wave-3 answer). Cached here because
/// `vm/src` may not grow a `static`.
pub fn vh_splice_refusal_asks_bind_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SPLICE_VH_REFUSAL_ASKS_BIND")
    })
}

/// `CRATONVM_VH_INDIRECT_SITE_WMTE` (default on, both modes; round 15 wave 1,
/// lane vh): a value parameter of an `IndirectVarHandle` access whose static
/// call-site type `asType` cannot convert to the access mode's is refused
/// with `WrongMethodTypeException`. `0` restores the permissive boxing.
pub(crate) fn vh_indirect_site_wmte_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_INDIRECT_SITE_WMTE")
    })
}

/// `CRATONVM_VH_INDIRECT_TYPED=1` (opt-in; round 15 wave 1, lane vh; F7-1):
/// an access whose arguments already are the mode handle's declared types is
/// served through the `invokeExact` door instead of `invokeWithArguments`.
pub(crate) fn vh_indirect_typed_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_VH_INDIRECT_TYPED"))
}

/// The rows of this block, each with its kind stated here. Called from
/// `register_phase54_method_handle` (both boot paths).
///
/// One `Bridge` standing in front of real bytecode:
/// `MethodHandles.collectReturnValue`, because its body needs a `LambdaForm`
/// editor no CratonVM handle has
/// (`docs/feature-designs/native-dispatch-architecture.md` says what retires it).
///
/// `IndirectVarHandle.withInvokeExactBehavior` is no longer registered in either
/// mode: SBR5 FIX3 (2026-10-07) retired it under `--jdk-only` (the doors resolve
/// the `exact` slot on their first ask, `var_handle_exact_gate`), and the
/// `--compatible` row went the same way once the gate was shown mode-independent.
pub(crate) fn register_r15_vh_indirect_natives(r: &mut NativeMethodRegistry) {
    r.register_with_kind(
        "java/lang/invoke/MethodHandles",
        "collectReturnValue",
        MHS_COLLECT_RETURN_VALUE_DESC,
        mhs_collect_return_value,
        cratonvm_native_api::NativeKind::Bridge,
    );
}

pub(crate) const MHS_COLLECT_RETURN_VALUE_DESC: &str =
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;";

/// How [`mhs_collect_return_value`] builds `collectReturnValue(target,
/// filter)` for `T target(A...)` and `V filter(B..., T)`: the adapter is
/// `V adapter(A... a, B... b) { return filter(b..., target(a...)); }`.
///
/// `collectArguments(filter, n, target)` is `(B..., A...)V` with exactly that
/// body (`n` = the count of `B`); for `n > 0` a `permuteArguments` to
/// `(A..., B...)V` gives the JDK's parameter order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CollectReturnValuePlan {
    /// `n`: the filter's leading parameters, the collect position.
    pub(crate) leading: i32,
    /// For `n > 0`: the adapter's descriptor and the reorder from the
    /// collected handle's `(B..., A...)` to it.
    pub(crate) permute: Option<(String, Vec<i32>)>,
}

/// The plan for a target typed `target_desc` and a filter typed
/// `filter_desc`, or `None` for a shape the JDK body alone decides (a filter
/// whose last parameter is not the target's return, a value filter over a
/// `void` target).
pub(crate) fn collect_return_value_plan(
    target_desc: &str,
    filter_desc: &str,
) -> Option<CollectReturnValuePlan> {
    let (tparams, tret) = split_descriptor_params(target_desc)?;
    let (fparams, fret) = split_descriptor_params(filter_desc)?;
    let Some((value, leading)) = fparams.split_last() else {
        // `V filter()` takes nothing from a `void` target.
        return (tret == DESC_VOID).then_some(CollectReturnValuePlan {
            leading: 0,
            permute: None,
        });
    };
    if tret == DESC_VOID || *value != tret {
        return None;
    }
    if leading.is_empty() {
        return Some(CollectReturnValuePlan {
            leading: 0,
            permute: None,
        });
    }
    let m = tparams.len();
    let n = leading.len();
    let desc = format!("({}{}){fret}", tparams.concat(), leading.concat());
    let mut reorder = Vec::with_capacity(n + m);
    for i in 0..n {
        reorder.push(i32::try_from(m + i).ok()?);
    }
    for j in 0..m {
        reorder.push(i32::try_from(j).ok()?);
    }
    Some(CollectReturnValuePlan {
        leading: i32::try_from(n).ok()?,
        permute: Some((desc, reorder)),
    })
}

/// `MethodHandles.collectReturnValue(target, filter)` (package-private; its
/// caller is `VarHandles.filterValue`'s handle factory): see
/// [`CollectReturnValuePlan`]. Anything it does not plan -- the switch off, a
/// null argument (the JDK body's `target.type()` NPE), an unreadable type --
/// runs the JDK body, today's answer.
pub(crate) fn mhs_collect_return_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let planned = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(t))), Some(Value::Object(Some(f))))
            if mh_collect_return_value_enabled() =>
        {
            let (target, filter) = (*t, *f);
            match (
                mh_type_descriptor(ctx, target),
                mh_type_descriptor(ctx, filter),
            ) {
                (Some(td), Some(fd)) => {
                    collect_return_value_plan(&td, &fd).map(|plan| (target, filter, plan))
                }
                _ => None,
            }
        }
        _ => None,
    };
    let Some((target, filter, plan)) = planned else {
        return ctx.invoke_special_bytecode_only(
            "java/lang/invoke/MethodHandles",
            "collectReturnValue",
            MHS_COLLECT_RETURN_VALUE_DESC,
            args,
        );
    };
    // The two combinators' own bodies, called directly: a native-to-Java
    // `invoke_static` of a Bridge standing over bytecode takes the
    // class-shared door, where `--jdk-only` runs the JDK body instead.
    let collected = make_collect_args_adapter(
        ctx,
        Some(Value::Object(Some(filter))),
        plan.leading,
        Some(Value::Object(Some(target))),
    )?;
    let Some((desc, reorder)) = plan.permute else {
        return Ok(collected);
    };
    let collected = mh_handle_answer(collected, "collectArguments")?;
    let permuted = mh_permute_to_desc(ctx, collected, &desc, &reorder)?;
    Ok(Some(Value::Object(Some(permuted))))
}

/// Open the doors' exact-handle gate (`note_exact_var_handle_minted`, what
/// every exact handle this file mints does) for an exact `IndirectVarHandle`
/// the JDK built, in the modes [`vh_exact_judged_in_this_mode`] admits (see
/// [`varhandle_with_exactness`] for why `--compatible` is safe).
pub(crate) fn indirect_vh_note_exact(ctx: &mut dyn NativeContext, built: &Option<Value>) {
    if !vh_exact_judged_in_this_mode(ctx) || !vh_indirect_exact_enabled() {
        return;
    }
    let Some(Value::Object(Some(handle))) = built else {
        return;
    };
    let handle = *handle;
    if !is_indirect_var_handle(ctx, handle) {
        return;
    }
    let class_id = ctx.class_id_of_object(handle);
    let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "exact") else {
        return;
    };
    if matches!(ctx.get_field(handle, slot), Value::Int(v) if v != 0) {
        ctx.note_exact_var_handle_minted(slot);
    }
}

/// The descriptor a `Class` mirror names, or `None` when it cannot be read --
/// never `mirror_to_descriptor`'s `Object` fallback, which would invent a
/// mismatch at an exact call site.
///
/// Interpreter round i6 wave 118, lane L4 (follow-on of
/// `i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object`, under its
/// `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME`): a lambda proxy's mirror, which
/// `resolve_class_name_robust` cannot name, answers its OWN descriptor
/// ([`unnameable_mirror_descriptor`], `L<host>$$Lambda/0x<id>;`, what HotSpot's
/// `toMethodDescriptorString` spells for a hidden class) instead of `None`, so
/// an exact VarHandle over a proxy-typed coordinate or value is judged as
/// HotSpot judges it rather than skipped. Any other unnameable mirror is still
/// `None`.
pub(crate) fn strict_mirror_descriptor(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<String> {
    let Some(name) = resolve_class_name_robust(ctx, mirror) else {
        return unnameable_mirror_descriptor(ctx, mirror);
    };
    if name.is_empty() {
        return None;
    }
    Some(class_name_to_descriptor(&name.replace('.', "/")).into_owned())
}

/// The access mode type (handle excluded) of access type `access_type` for an
/// EXACT `IndirectVarHandle`, from its own `value` and `coordinates`: what
/// HotSpot's `accessModeTypeUncached` answers. `None` for any other handle, a
/// non-exact one, the switch off, or a mirror that does not name its class.
pub(crate) fn indirect_vh_exact_access_descriptor(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    access_type: i32,
) -> Option<String> {
    if !vh_indirect_exact_enabled() || !is_indirect_var_handle(ctx, vh) {
        return None;
    }
    if !matches!(ctx.get_field_by_name(vh, "exact"), Value::Int(v) if v != 0) {
        return None;
    }
    let (coord_descs, value_desc) = indirect_vh_shape(ctx, vh)?;
    Some(vh_access_mode_descriptor(
        access_type,
        &coord_descs,
        &value_desc,
    ))
}

/// An `IndirectVarHandle`'s own coordinate descriptors and variable
/// descriptor, from its `coordinates` / `value` fields (what
/// `accessModeTypeUncached` reads on HotSpot); `None` when either is not a
/// readable class mirror. Reads only.
pub(crate) fn indirect_vh_shape(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<(Vec<String>, String)> {
    let value = indirect_vh_field(ctx, vh, "value")?;
    let value_desc = strict_mirror_descriptor(&*ctx, value)?;
    let coords = indirect_vh_field(ctx, vh, "coordinates")?;
    if !matches!(ctx.heap_kind_of(coords), ObjectKind::Array) {
        return None;
    }
    let n = ctx.array_length(coords);
    let mut coord_descs: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let Value::Object(Some(mirror)) = ctx.get_array_element(coords, i) else {
            return None;
        };
        coord_descs.push(strict_mirror_descriptor(&*ctx, mirror)?);
    }
    Some((coord_descs, value_desc))
}

/// The number of trailing VALUE parameters access mode `ordinal` takes: `get*`
/// none, `set*` / `getAndUpdate` one, `compareAnd*` two.
pub(crate) fn vh_access_mode_value_count(ordinal: usize) -> usize {
    match VH_ACCESS_MODES
        .get(ordinal)
        .and_then(|(_, method)| vh_access_type_of_method(method))
    {
        Some(1) | Some(4) => 1,
        Some(2) | Some(3) => 2,
        _ => 0,
    }
}

/// `MethodType.canConvert(site, declared)` for one VALUE parameter, `class`
/// being its call-site [`vh_site`](cratonvm_native_api::poly_call_site::vh_site)
/// class. `None` (not judged) when nothing was armed, and for a reference site
/// into a reference parameter (the cast `invokeWithArguments` performs).
pub(crate) fn vh_indirect_site_admits(declared: &str, class: u8) -> Option<bool> {
    use cratonvm_native_api::poly_call_site::vh_site;
    if class == vh_site::UNKNOWN {
        return None;
    }
    match declared.as_bytes() {
        [p] if vh_site::is_primitive(*p) => vh_site_admits_primitive(class, *p),
        _ if vh_site::is_primitive(class) => {
            let prim = char::from(class).to_string();
            Some(
                wrapper_desc_for_primitive(&prim)
                    .is_some_and(|wrapper| reference_accepts_wrapper(declared, wrapper)),
            )
        }
        _ => None,
    }
}

/// The `WrongMethodTypeException` `asType` raises for a parameter of an
/// `IndirectVarHandle` access whose static call-site type does not convert
/// ([`vh_indirect_site_admits`]), or `None`. `site` is the parameter-site
/// word (`poly_call_site::vh_param_site_classes`): one class byte per
/// call-site parameter, the LAST parameter in the low byte. The values of an
/// access mode trail its coordinates, so the low byte(s) are the values and
/// the bytes above them the coordinates, last coordinate first (round 15
/// wave 1 hand-back judged the last coordinate of a one-value mode,
/// `r15w1-vh-indirect-coordinate-narrowing-FIXED-20260930.md`; round 15 wave 3,
/// lane vh3, VH1-3, every coordinate the word covers, read modes included).
/// A door that armed only the old 16-bit cell leaves the upper bytes
/// `UNKNOWN`, which are not judged. Coordinates are judged under their own
/// switch, and `CRATONVM_VH_INDIRECT_SITE_ALL_COORDS=0` judges only the two
/// low positions again (the wave-1 reach). `params` is the mode handle's
/// declared `(VarHandle, coordinates..., values...)`, one for one with
/// `args`; the handle itself (position 0) is never a call-site parameter.
pub(crate) fn vh_indirect_value_site_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    params: &[String],
    ordinal: usize,
    site: u64,
) -> Option<MethodCallFailed> {
    use cratonvm_native_api::poly_call_site::{
        vh_param_site_class_at, vh_site, VH_PARAM_SITE_SLOTS,
    };
    if site == 0 || params.len() != args.len() {
        return None;
    }
    let values = vh_access_mode_value_count(ordinal);
    let reach = if vh_indirect_site_all_coords_enabled() {
        VH_PARAM_SITE_SLOTS
    } else {
        2
    };
    // `k` counts back from the last parameter; the earliest one first, the
    // order `asType` walks them.
    for k in (0..reach.min(params.len())).rev() {
        let Some(pos) = params.len().checked_sub(1 + k) else {
            continue;
        };
        if pos == 0 {
            continue;
        }
        let judged = if k < values {
            vh_indirect_site_wmte_enabled()
        } else {
            vh_indirect_site_coord_wmte_enabled()
        };
        if !judged {
            continue;
        }
        let class = vh_param_site_class_at(site, k);
        let declared = params[pos].as_str();
        // A reference class judges a reference only (the other value
        // natives' rule, `vh_adapt_value`): a stand-in site whose value
        // arrived primitive is not this parameter's static type.
        if !vh_site::is_primitive(class) && !matches!(args[pos], Value::Object(_)) {
            continue;
        }
        if vh_indirect_site_admits(declared, class) != Some(false) {
            continue;
        }
        // The handle's shape goes with the refusal, for the door's
        // two-type message (wave 53, L4; `vh_astype_refused`).
        let value = args[pos];
        let raise = |ctx: &mut dyn NativeContext| {
            if vh_site::is_primitive(class) {
                let to = descriptor_to_class_name(declared).class_text(&*ctx);
                crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/invoke/WrongMethodTypeException",
                    &format!("cannot convert {} to {to}", vh_prim_java_name(class)),
                )
            } else {
                // A reference site refused only into a primitive parameter.
                let p = declared.as_bytes().first().copied().unwrap_or(b'I');
                vh_value_wmte(ctx, value, p)
            }
        };
        return Some(match args[0] {
            Value::Object(Some(vh)) => vh_astype_refused(ctx, vh, raise),
            _ => raise(ctx),
        });
    }
    None
}

/// F7-1: the arguments after the handle, each already of the mode handle's
/// declared parameter type (a primitive widened as `asType` widens it), or
/// `None` when any of them needs a conversion the `invokeWithArguments` door
/// performs (a box, an unbox, a reference cast that fails) or is an array
/// (the varargs repair's territory). References are returned as passed; the
/// caller re-reads them from their pins.
pub(crate) fn vh_indirect_typed_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    params: &[String],
) -> Option<Vec<Value>> {
    if args.is_empty() || args.len() != params.len() {
        return None;
    }
    let mut out = Vec::with_capacity(args.len() - 1);
    for (value, declared) in args.iter().zip(params.iter()).skip(1) {
        let declared = declared.as_str();
        let converted = match declared.as_bytes() {
            [b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D'] => {
                let (converted, desc) = vh_indirect_box_input(*value, Some(declared))?;
                if desc != declared {
                    return None;
                }
                converted
            }
            [b'L', ..] => match value {
                Value::Object(_)
                    if reference_arg_admitted(ctx, *value, declared) != Some(false) =>
                {
                    *value
                }
                _ => return None,
            },
            _ => return None,
        };
        out.push(converted);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// SBR5 lane avh: `MemoryLayout.arrayElementVarHandle` over the JDK's own bodies
// ---------------------------------------------------------------------------
//
// `AbstractLayout.arrayElementVarHandle(elements)` is
// `MethodHandles.collectCoordinates(varHandle(elements), 1, scaleHandle())`: an
// `IndirectVarHandle` whose index coordinate runs `MemoryLayout.scale(offset, index)`
// (`offset + index * byteSize`, a `NegativeArraySize`-free check of both operands and
// exact arithmetic) before the segment handle's own access. Through the composed
// `MethodHandle` that is a dozen `LambdaForm` entries per access (3-5 us each here), 95 us
// per get; the shortcut below makes the access the segment handle's own at the scaled
// offset (about 2 us), after the checks the composed road shares.
//
// Soundness is by CONSTRUCTION, not by shape guessing: the handle's captured filter is
// compared with the filter the JDK itself builds for the same layout
// (`collectCoordinates(target, 1, layout.scaleHandle())`, called once per handle and
// collection epoch): the same `BoundMethodHandle` species over the same `LambdaForm` and the
// same leaf arguments (the checked-exception wrapper `adaptForCheckedExceptions` puts
// around any handle it cannot read the exceptions of). Any difference (a user's own
// coordinate filter, an exact handle, a negative or overflowing scale) declines to the
// composed road, which raises what HotSpot raises.

pub(crate) const ARRAY_ELEM_MEMO_SLOTS: usize = 64;

#[derive(Clone)]
pub(crate) struct ArrayElemLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) addr: usize,
    pub(crate) filled: bool,
    pub(crate) plan: Option<(ObjectRef, i64)>,
    /// The mode handle's declared parameters of the plain `get` / `set` modes, filed by the
    /// first access that went through the composed road's screens ([`vh_array_element_note`]).
    pub(crate) get_params: Option<Arc<[String]>>,
    pub(crate) set_params: Option<Arc<[String]>>,
    /// The class of a segment that passed the full `MemorySegment` admission on such an access.
    pub(crate) seg_class: Option<ClassId>,
}

impl ArrayElemLine {
    pub(crate) fn empty() -> Self {
        ArrayElemLine {
            vm: 0,
            epoch: 0,
            addr: 0,
            filled: false,
            plan: None,
            get_params: None,
            set_params: None,
            seg_class: None,
        }
    }
}

thread_local! {
    /// Per-thread verdicts of [`vh_array_element_verify`], keyed by the indirect handle's
    /// address, valid for one collection epoch under one VM (the argument of
    /// [`REAL_HANDLE_MEMO`]: nothing moves or is freed within an epoch, and the handle's
    /// `target` / `handleFactory` are final fields).
    pub(crate) static ARRAY_ELEM_MEMO: std::cell::RefCell<Vec<ArrayElemLine>> =
        std::cell::RefCell::new(
            (0..ARRAY_ELEM_MEMO_SLOTS).map(|_| ArrayElemLine::empty()).collect(),
        );
}

pub(crate) const BMH_SPECIES_PREFIX: &str = "java/lang/invoke/BoundMethodHandle$Species_";

/// The first object under `h` (a `BoundMethodHandle` species tree) that is a
/// `jdk.internal.foreign.layout` layout, searching reference arguments.
pub(crate) fn bmh_find_layout(ctx: &mut dyn NativeContext, h: ObjectRef, depth: u32) -> Option<ObjectRef> {
    if depth > 6 {
        return None;
    }
    let cid = ctx.class_id_of_object(h);
    let name = ctx.class_name_arc_of_id(cid)?;
    if name.starts_with("jdk/internal/foreign/layout/") {
        return Some(h);
    }
    let letters = name.strip_prefix(BMH_SPECIES_PREFIX)?;
    for (i, ch) in letters.chars().enumerate() {
        if ch != 'L' {
            continue;
        }
        if let Value::Object(Some(o)) = ctx.get_field_by_name(h, &format!("argL{i}")) {
            if let Some(found) = bmh_find_layout(ctx, o, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Two `BoundMethodHandle` trees that are the same handle built twice: the same species,
/// `form` and `type`, and every argument identical (a nested handle: the same again).
pub(crate) fn bmh_tree_equal(ctx: &mut dyn NativeContext, a: ObjectRef, b: ObjectRef, depth: u32) -> bool {
    if a == b {
        return true;
    }
    if depth > 6 {
        return false;
    }
    let ca = ctx.class_id_of_object(a);
    if ca != ctx.class_id_of_object(b) {
        return false;
    }
    let Some(name) = ctx.class_name_arc_of_id(ca) else {
        return false;
    };
    let Some(letters) = name.strip_prefix(BMH_SPECIES_PREFIX) else {
        return false;
    };
    for field in ["form", "type"] {
        if ctx.get_field_by_name(a, field) != ctx.get_field_by_name(b, field) {
            return false;
        }
    }
    for (i, ch) in letters.chars().enumerate() {
        let field = format!("arg{ch}{i}");
        let (va, vb) = (
            ctx.get_field_by_name(a, &field),
            ctx.get_field_by_name(b, &field),
        );
        match (va, vb) {
            (Value::Object(Some(x)), Value::Object(Some(y))) => {
                if x != y && !bmh_tree_equal(ctx, x, y, depth + 1) {
                    return false;
                }
            }
            (x, y) => {
                if x != y {
                    return false;
                }
            }
        }
    }
    true
}

/// The captured variable `name` (`arg$N`) of a lambda: a lambda proxy this VM mints holds its
/// captures in the heap slots `0..n` in factory-argument order (no declared fields), a
/// spun hidden class has the fields.
pub(crate) fn lambda_capture(ctx: &mut dyn NativeContext, lambda: ObjectRef, name: &str) -> Value {
    match crate::lang_class::lambda_capture_slot(&*ctx, lambda, name) {
        Some(slot) => ctx.get_field(lambda, slot),
        None => ctx.get_field_by_name(lambda, name),
    }
}

/// Is `vh` the JDK's `arrayElementVarHandle` of a layout over a real `SegmentVarHandle`?
/// `(segment handle, byteSize)`, or `None`. Runs Java (a GC point): `vh` is re-read from
/// `vh_pin`, and the answer's handle is read last.
pub(crate) fn vh_array_element_verify(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Option<(ObjectRef, i64)> {
    vh_array_element_verify_why(ctx, vh_pin, vh).ok()
}

pub(crate) fn vh_array_element_verify_why(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Result<(ObjectRef, i64), &'static str> {
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    let mut scope = NativeHandleScope::new(ctx);
    let vh_h = scope.root(vh_now);
    // An exact handle judges its call site's types itself.
    if !matches!(scope.get_field_by_name(vh_now, "exact"), Value::Int(0)) {
        return Err("exact");
    }
    let factory = indirect_vh_field(&mut *scope, vh_now, "handleFactory").ok_or("no factory")?;
    // A lambda proxy this VM mints has no loaded-class name; the captured arguments and the
    // structural comparison below are what decide.
    let factory_cid = scope.class_id_of_object(factory);
    if let Some(fname) = scope.class_name_arc_of_id(factory_cid) {
        if !fname.starts_with("java/lang/invoke/VarHandles$$Lambda") {
            return Err("factory is not a VarHandles lambda");
        }
    }
    if lambda_capture(&mut *scope, factory, "arg$1") != Value::Int(1) {
        return Err("arg$1 is not 1");
    }
    let Value::Object(Some(filter)) = lambda_capture(&mut *scope, factory, "arg$2") else {
        return Err("no arg$2");
    };
    let target = indirect_vh_field(&mut *scope, vh_now, "target").ok_or("no target")?;
    if !is_segment_var_handle(&mut *scope, target) {
        return Err("target is not a SegmentVarHandle");
    }
    let filter_h = scope.root(filter);
    let target_h = scope.root(target);
    let layout = bmh_find_layout(&mut *scope, filter, 0).ok_or("no layout in the filter")?;
    let layout_h = scope.root(layout);
    let stride = match scope.get_field_by_name(layout, "byteSize") {
        Value::Long(v) if v >= 0 => v,
        _ => return Err("byteSize"),
    };
    // The JDK's own filter for this layout, built the way `arrayElementVarHandle` builds it.
    let layout_now = scope.get(&layout_h);
    let Ok(Some(Value::Object(Some(scale)))) = scope.invoke_virtual(
        layout_now,
        "scaleHandle",
        "()Ljava/lang/invoke/MethodHandle;",
        &[],
    ) else {
        return Err("scaleHandle");
    };
    let scale_h = scope.root(scale);
    let (target_now, scale_now) = (scope.get(&target_h), scope.get(&scale_h));
    let Ok(Some(Value::Object(Some(reference)))) = scope.invoke_static(
        "java/lang/invoke/MethodHandles",
        "collectCoordinates",
        "(Ljava/lang/invoke/VarHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/VarHandle;",
        &[Value::Object(Some(target_now)), Value::Int(1), Value::Object(Some(scale_now))],
    ) else {
        return Err("collectCoordinates");
    };
    let reference_factory =
        indirect_vh_field(&mut *scope, reference, "handleFactory").ok_or("no reference factory")?;
    let Value::Object(Some(reference_filter)) =
        lambda_capture(&mut *scope, reference_factory, "arg$2")
    else {
        return Err("no reference arg$2");
    };
    let _ = vh_h;
    let filter = scope.get(&filter_h);
    if !bmh_tree_equal(&mut *scope, filter, reference_filter, 0) {
        return Err("filter differs from the JDK's");
    }
    Ok((scope.get(&target_h), stride))
}

/// The memoised verdict of [`vh_array_element_verify`] for `vh` in this collection epoch:
/// `Some(verdict)` on a hit; `None` on a miss, after FILING the verdict (the verify ran
/// Java, so the caller's arguments may have moved and it declines this one access).
pub(crate) fn vh_array_element_plan_hit(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
) -> Option<Option<(ObjectRef, i64)>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    let epoch = ctx.gc_address_epoch();
    let addr = vh_now.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    let hit = ARRAY_ELEM_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        (line.filled && line.vm == vm && line.epoch == epoch && line.addr == addr)
            .then_some(line.plan)
    });
    if hit.is_some() {
        return hit;
    }
    let plan = vh_array_element_verify(ctx, vh_pin, vh);
    // Keyed by where the handle is NOW, in the epoch it is in NOW.
    let vh_after = ctx.read_native_pin(vh_pin, vh);
    let epoch = ctx.gc_address_epoch();
    let addr = vh_after.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    ARRAY_ELEM_MEMO.with(|memo| {
        memo.borrow_mut()[slot] = ArrayElemLine {
            vm,
            epoch,
            addr,
            filled: true,
            plan,
            ..ArrayElemLine::empty()
        };
    });
    None
}

/// Files what a screened access learned about the (already verified) array-element handle
/// at `vh`: the mode handle's declared parameters for `ordinal`'s kind (plain `get` or
/// `set`) and the class of the segment that passed the `MemorySegment` admission, so the
/// next access of the same shape is served by [`vh_array_element_early`] without them.
pub(crate) fn vh_array_element_note(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    is_get: bool,
    params: &Arc<[String]>,
    seg_class: ClassId,
) {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = vh.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    ARRAY_ELEM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let line = &mut memo[slot];
        if line.filled
            && line.vm == vm
            && line.epoch == epoch
            && line.addr == addr
            && line.plan.is_some()
        {
            if is_get {
                line.get_params = Some(Arc::clone(params));
            } else {
                line.set_params = Some(Arc::clone(params));
            }
            line.seg_class = Some(seg_class);
        }
    });
}

/// The first thing a plain `get` / `set` of an array-element handle does, before the mode
/// handle is composed or its parameters rendered: when this handle's verdict and the
/// screens of an earlier access of the same shape are filed (this epoch), re-judge only
/// what varies (the call site's value types, the coordinates' and the value's variants, the
/// segment's class) and make the access. `None` declines to the composed road, which
/// screens everything itself.
pub(crate) fn vh_array_element_early(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    ordinal: usize,
    site: u64,
) -> Option<MethodCallResult> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let is_get = matches!(method, "get" | "getVolatile" | "getOpaque" | "getAcquire");
    let is_set = matches!(method, "set" | "setVolatile" | "setOpaque" | "setRelease");
    if !((is_get && args.len() == 4) || (is_set && args.len() == 5)) {
        return None;
    }
    let (Value::Object(Some(seg)), Value::Long(base), Value::Long(index)) =
        (args[1], args[2], args[3])
    else {
        return None;
    };
    if base < 0 || index < 0 {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = vh.as_ptr() as usize;
    let slot = (addr >> 3) & (ARRAY_ELEM_MEMO_SLOTS - 1);
    let (target, stride, params, seg_class) = ARRAY_ELEM_MEMO.with(|memo| {
        let memo = memo.borrow();
        let line = &memo[slot];
        if !(line.filled && line.vm == vm && line.epoch == epoch && line.addr == addr) {
            return None;
        }
        let (target, stride) = line.plan?;
        let params = if is_get {
            line.get_params.clone()
        } else {
            line.set_params.clone()
        }?;
        Some((target, stride, params, line.seg_class?))
    })?;
    if ctx.class_id_of_object(seg) != seg_class || params.len() != args.len() {
        return None;
    }
    // The call site's static types, as the composed road judges them.
    if site != 0 {
        if let Some(refusal) = vh_indirect_value_site_refusal(ctx, args, &params, ordinal, site) {
            return Some(Err(refusal));
        }
    }
    // The value is the declared type exactly (the segment, admitted above, stands as `null`
    // for the typed screen so that it does not repeat the class walk).
    if is_set {
        let mut screened: [Value; 5] = [Value::Object(None); 5];
        screened[2] = args[2];
        screened[3] = args[3];
        screened[4] = args[4];
        vh_indirect_typed_args(ctx, &screened, &params)?;
    }
    let offset = base.checked_add(index.checked_mul(stride)?)?;
    Some(if is_get {
        varhandle_get_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
            ],
            method,
        )
    } else {
        varhandle_set_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
                args[4],
            ],
            method,
        )
    })
}

/// The access `rest` = `(segment, base, index[, value])` of the `arrayElementVarHandle`
/// whose plain access mode is `ordinal`, made on the segment handle at `base + index *
/// byteSize`; `None` declines to the composed road (another access mode, another handle
/// shape, a negative or overflowing scale, a verdict not filed yet).
pub(crate) fn vh_array_element_forward(
    ctx: &mut dyn NativeContext,
    vh_pin: usize,
    vh: ObjectRef,
    ordinal: usize,
    rest: &[Value],
    params: &Arc<[String]>,
) -> Option<MethodCallResult> {
    let method = VH_ACCESS_MODES[ordinal].1;
    let is_get = matches!(method, "get" | "getVolatile" | "getOpaque" | "getAcquire");
    let is_set = matches!(method, "set" | "setVolatile" | "setOpaque" | "setRelease");
    if !((is_get && rest.len() == 3) || (is_set && rest.len() == 4)) {
        return None;
    }
    let (Value::Object(Some(seg)), Value::Long(base), Value::Long(index)) =
        (rest[0], rest[1], rest[2])
    else {
        return None;
    };
    if base < 0 || index < 0 {
        return None;
    }
    let (target, stride) = vh_array_element_plan_hit(ctx, vh_pin, vh)??;
    let offset = base.checked_add(index.checked_mul(stride)?)?;
    // The screens of the composed road passed for this access: the next access of this
    // shape skips them ([`vh_array_element_early`]).
    let seg_class = ctx.class_id_of_object(seg);
    let vh_now = ctx.read_native_pin(vh_pin, vh);
    vh_array_element_note(ctx, vh_now, is_get, params, seg_class);
    Some(if is_get {
        varhandle_get_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
            ],
            method,
        )
    } else {
        varhandle_set_mode(
            ctx,
            &[
                Value::Object(Some(target)),
                Value::Object(Some(seg)),
                Value::Long(offset),
                rest[3],
            ],
            method,
        )
    })
}

/// A REAL handle's own `type()` as a descriptor, or `None`.
pub(crate) fn mh_real_declared_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(mh, "type") else {
        return None;
    };
    crate::link_to_method_descriptor(&*ctx, mt)
}

/// F7-1: the composed mode handle is one of this file's carriers (the
/// `invokeExact` door's model) and not a varargs collector.
pub(crate) fn vh_indirect_typed_admits_handle(ctx: &mut dyn NativeContext, mh: ObjectRef) -> bool {
    !mh_is_real_handle(&*ctx, mh) && mh_marking_bits(&*ctx, mh) & MH_MARK_VARARGS == 0
}

#[cfg(test)]
mod r15w1_vh_indirect_tests {
    //! Round 15 wave 1 (lane vh): `collectReturnValue`'s composition, the
    //! value-site judgement and the typed (F7-1) argument screen.
    use super::*;
    use cratonvm_native_api::poly_call_site::vh_site;

    #[test]
    fn a_unary_filter_is_a_collect_at_zero_with_no_permute() {
        let plan =
            collect_return_value_plan("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)I", "(I)J");
        assert_eq!(
            plan,
            Some(CollectReturnValuePlan {
                leading: 0,
                permute: None
            })
        );
    }

    #[test]
    fn leading_filter_parameters_trail_the_adapter() {
        // `V filter(B0, B1, T)` over `T target(A0, A1, A2)`:
        // collected `(B0, B1, A0, A1, A2)V`, adapter `(A0, A1, A2, B0, B1)V`.
        let plan = collect_return_value_plan(
            "(Ljava/lang/invoke/VarHandle;JS)I",
            "(Ljava/lang/String;DI)J",
        );
        let Some(CollectReturnValuePlan {
            leading,
            permute: Some((desc, reorder)),
        }) = plan
        else {
            panic!("no permute planned for a two-parameter prefix");
        };
        assert_eq!(leading, 2);
        assert_eq!(desc, "(Ljava/lang/invoke/VarHandle;JSLjava/lang/String;D)J");
        // Collected parameter i lands at adapter position reorder[i].
        assert_eq!(reorder, vec![3, 4, 0, 1, 2]);
    }

    #[test]
    fn shapes_the_jdk_body_decides_are_not_planned() {
        // The filter's value parameter is not the target's return.
        assert_eq!(collect_return_value_plan("()I", "(J)J"), None);
        // A value filter over a `void` target.
        assert_eq!(collect_return_value_plan("()V", "(I)I"), None);
        // `V filter()` over a `void` target is `filterReturnValue`'s case.
        assert_eq!(
            collect_return_value_plan("(I)V", "()I"),
            Some(CollectReturnValuePlan {
                leading: 0,
                permute: None
            })
        );
        // A `V filter()` cannot swallow a value.
        assert_eq!(collect_return_value_plan("()I", "()I"), None);
    }

    #[test]
    fn value_counts_follow_the_access_types() {
        for (ordinal, (name, _)) in VH_ACCESS_MODES.iter().enumerate() {
            let want = if name.starts_with("GET_AND_") {
                1
            } else if name.starts_with("GET") {
                0
            } else if name.starts_with("SET") {
                1
            } else {
                2
            };
            assert_eq!(vh_access_mode_value_count(ordinal), want, "{name}");
        }
    }

    #[test]
    fn an_int_site_does_not_narrow_into_a_char_value() {
        assert_eq!(vh_indirect_site_admits("C", b'I'), Some(false));
        assert_eq!(vh_indirect_site_admits("S", b'B'), Some(true));
        assert_eq!(vh_indirect_site_admits("J", b'I'), Some(true));
        assert_eq!(vh_indirect_site_admits("I", b'I'), Some(true));
        assert_eq!(vh_indirect_site_admits("Z", b'I'), Some(false));
        // A primitive site boxes: `Integer` is a `Number`, not a `Long`.
        assert_eq!(
            vh_indirect_site_admits("Ljava/lang/Number;", b'I'),
            Some(true)
        );
        assert_eq!(
            vh_indirect_site_admits("Ljava/lang/Long;", b'I'),
            Some(false)
        );
        // Not judged: nothing armed, or a reference into a reference.
        assert_eq!(vh_indirect_site_admits("C", vh_site::UNKNOWN), None);
        assert_eq!(
            vh_indirect_site_admits("Ljava/lang/String;", vh_site::OBJECT),
            None
        );
        // A reference site into a primitive: `canConvert`'s wrapper rule.
        assert_eq!(vh_indirect_site_admits("I", vh_site::W_LONG), Some(false));
        assert_eq!(vh_indirect_site_admits("J", vh_site::W_INTEGER), Some(true));
    }

    /// Interpreter round i2 wave 55, lane L7: the compare-and-exchange witness
    /// is `expected` on an exchange (which stores), the value found on a
    /// mismatch (which does not), and a context whose CAS equality is
    /// narrower than `vh_values_match` (the mock compares floats with `==`,
    /// so a NaN never swaps) still answers, after the bounded retries.
    #[test]
    fn the_compare_and_exchange_witness_is_the_value_the_slot_held() {
        use cratonvm_native_api::NativeHeapAccess;
        let mut ctx = crate::test_utils::mock_ctx();
        let obj = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(obj, 0, Value::Int(7));
        let w = vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, Value::Int(7), Value::Int(9));
        assert_eq!(w, Value::Int(7));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(9), "exchanged");
        let w =
            vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, Value::Int(7), Value::Int(11));
        assert_eq!(w, Value::Int(9), "the witness of a mismatch");
        assert_eq!(ctx.get_field(obj, 0), Value::Int(9), "nothing stored");
        let nan = Value::Float(f32::NAN);
        ctx.set_field(obj, 0, nan.clone());
        let w = vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, nan, Value::Float(1.0));
        assert!(
            matches!(w, Value::Float(f) if f.is_nan()),
            "bounded, then the re-read: {w:?}"
        );
    }

    #[test]
    fn typed_arguments_are_the_declared_types_or_nothing() {
        let mut ctx = crate::test_utils::mock_ctx();
        let params: Vec<String> = [
            "Ljava/lang/invoke/VarHandle;",
            "J",
            "I",
            "Ljava/lang/Object;",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // An `int` into a `long` coordinate is widened, as `asType` does.
        let args = [
            Value::Object(None),
            Value::Int(3),
            Value::Int(4),
            Value::Object(None),
        ];
        assert_eq!(
            vh_indirect_typed_args(&mut ctx, &args, &params),
            Some(vec![Value::Long(3), Value::Int(4), Value::Object(None)])
        );
        // A `long` cannot narrow into the `int`; a primitive into the
        // `Object` needs a box: both left to `invokeWithArguments`.
        let narrowing = [
            Value::Object(None),
            Value::Long(3),
            Value::Long(4),
            Value::Object(None),
        ];
        assert_eq!(vh_indirect_typed_args(&mut ctx, &narrowing, &params), None);
        let boxing = [
            Value::Object(None),
            Value::Long(3),
            Value::Int(4),
            Value::Int(5),
        ];
        assert_eq!(vh_indirect_typed_args(&mut ctx, &boxing, &params), None);
        // An arity the mode handle does not take is not typed.
        assert_eq!(vh_indirect_typed_args(&mut ctx, &args[..3], &params), None);
    }

    #[test]
    fn a_store_modes_last_coordinate_is_judged_from_the_cell() {
        // `filterCoordinates(arrayElementVarHandle(int[].class), 1, (char)int)`:
        // `set` is `(VarHandle, int[], char, int)void`.
        let mut ctx = crate::test_utils::mock_ctx();
        let set = 1; // `VH_ACCESS_MODES[1]` is SET
        assert_eq!(VH_ACCESS_MODES[set].1, "set");
        let params: Vec<String> = ["Ljava/lang/invoke/VarHandle;", "[I", "C", "I"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = [
            Value::Object(None),
            Value::Object(None),
            Value::Int(1),
            Value::Int(7),
        ];
        let pack = |coord: u8, value: u8| (u64::from(coord) << 8) | u64::from(value);
        // `f.set(arr, (char) 1, 7)`: the declared types exactly.
        assert!(
            vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, pack(b'C', b'I'))
                .is_none()
        );
        // `f.set(arr, 1, 7)`: an `int` narrowed into the `char` coordinate.
        assert!(
            vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, pack(b'I', b'I'))
                .is_some()
        );
        // Nothing armed: not judged.
        assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, 0).is_none());
    }

    /// Round 15 wave 3 (lane vh3; VH1-3,
    /// `r15w1-vh-indirect-coordinate-narrowing-FIXED-20260930.md`): a READ mode's
    /// coordinates, and a store mode's EARLIER coordinates, are judged from
    /// the parameter-site word, and a door that armed only the 16-bit cell
    /// leaves them unjudged.
    #[test]
    fn every_covered_coordinate_is_judged_from_the_param_site_word() {
        use cratonvm_native_api::poly_call_site::vh_param_site_classes;
        if !vh_indirect_site_all_coords_enabled() || !vh_indirect_site_coord_wmte_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        fn strings(xs: &[&str]) -> Vec<String> {
            xs.iter().map(|s| s.to_string()).collect()
        }
        let mut ctx = crate::test_utils::mock_ctx();
        // `get` of `(VarHandle, int[], char)int`: `f.get(arr, i)` is `([II)I`.
        let get = 0;
        assert_eq!(VH_ACCESS_MODES[get].1, "get");
        let params = strings(&["Ljava/lang/invoke/VarHandle;", "[I", "C"]);
        let args = [Value::Object(None), Value::Object(None), Value::Int(1)];
        let wrong = vh_param_site_classes("([II)I");
        let right = vh_param_site_classes("([IC)I");
        assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, get, wrong).is_some());
        assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, get, right).is_none());
        // A byte view of the coordinate that widens (`char` into `int`) is
        // admitted: `(VarHandle, int[], int)` from a `([IC)I` site.
        let int_coord = strings(&["Ljava/lang/invoke/VarHandle;", "[I", "I"]);
        assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &int_coord, get, right).is_none());
        // `set` of `(VarHandle, char, int[], int)void` (a permuted handle):
        // the FIRST coordinate sits three bytes up.
        let set = 1;
        let permuted = strings(&["Ljava/lang/invoke/VarHandle;", "C", "[I", "I"]);
        let set_args = [
            Value::Object(None),
            Value::Int(1),
            Value::Object(None),
            Value::Int(7),
        ];
        let narrowed = vh_param_site_classes("(I[II)V");
        let exact = vh_param_site_classes("(C[II)V");
        assert!(
            vh_indirect_value_site_refusal(&mut ctx, &set_args, &permuted, set, narrowed).is_some()
        );
        assert!(
            vh_indirect_value_site_refusal(&mut ctx, &set_args, &permuted, set, exact).is_none()
        );
        // Only the old 16-bit cell armed: the first coordinate reads UNKNOWN.
        assert!(vh_indirect_value_site_refusal(
            &mut ctx,
            &set_args,
            &permuted,
            set,
            narrowed & 0xffff
        )
        .is_none());
    }
}

#[cfg(test)]
mod r14w7_ffm7_indirect_vh_tests {
    //! Round 14 wave 7 (lane ffm7): an access through an `IndirectVarHandle`
    //! is served through its mode handle, or refused -- never `null`.
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    struct Fixture {
        ctx: MockNativeContext,
        vh: ObjectRef,
        direct: ObjectRef,
        table: ObjectRef,
        other_cid: ClassId,
    }

    fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
        match ctx.ensure_class_initialized(name) {
            Ok(cid) => cid,
            Err(err) => panic!("no class {name}: {err:?}"),
        }
    }

    fn field(
        cid: ClassId,
        name: &str,
        desc: &str,
        slot: usize,
    ) -> cratonvm_native_api::FieldMetadata {
        cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        }
    }

    /// An `IndirectVarHandle` on the real class's field names (the inherited
    /// `methodHandleTable` at slot 0), with a `directTarget` and an empty
    /// mode-handle table; `target` and `handleFactory` unset.
    fn fixture() -> Fixture {
        let mut ctx = mock_ctx();
        ctx.set_absent_field_answers_null(true);
        let table_desc = "[Ljava/lang/invoke/MethodHandle;";
        let base = class(&mut ctx, VAR_HANDLE_CLASS);
        ctx.set_declared_fields(base, vec![field(base, "methodHandleTable", table_desc, 0)]);
        let cid = class(&mut ctx, INDIRECT_VAR_HANDLE_CLASS);
        ctx.set_declared_fields(
            cid,
            vec![
                field(cid, "methodHandleTable", table_desc, 0),
                field(cid, "target", "Ljava/lang/invoke/VarHandle;", 1),
                field(cid, "directTarget", "Ljava/lang/invoke/VarHandle;", 2),
                field(cid, "handleFactory", "Ljava/util/function/BiFunction;", 3),
                field(cid, "value", "Ljava/lang/Class;", 4),
                field(cid, "coordinates", "[Ljava/lang/Class;", 5),
            ],
        );
        let other_cid = class(&mut ctx, "p/R14w7Other");
        let vh = ctx.alloc_object(cid, 6);
        let direct = ctx.alloc_object(other_cid, 1);
        ctx.set_field(vh, 2, Value::Object(Some(direct)));
        let table = ctx.new_array(ArrayElementType::Reference, VH_ACCESS_MODES.len());
        ctx.set_field(vh, 0, Value::Object(Some(table)));
        Fixture {
            ctx,
            vh,
            direct,
            table,
            other_cid,
        }
    }

    /// Answers `invokeWithArguments` with element `SLOT` of its array.
    fn echo<const SLOT: usize>(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        name: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if name != "invokeWithArguments" || desc != "([Ljava/lang/Object;)Ljava/lang/Object;" {
            return None;
        }
        let Some(Value::Object(Some(arr))) = args.first() else {
            return None;
        };
        Some(Ok(Some(ctx.get_array_element(*arr, SLOT))))
    }

    #[test]
    fn access_mode_table_matches_the_enum_spelling() {
        assert_eq!(VH_ACCESS_MODES.len(), 31);
        for (ordinal, (name, method)) in VH_ACCESS_MODES.iter().enumerate() {
            assert_eq!(access_mode_method_name(name), *method, "{name}");
            assert_eq!(
                vh_access_mode_is_set(ordinal),
                name.starts_with("SET"),
                "{name}"
            );
        }
    }

    #[test]
    fn primitive_arguments_box_as_the_declared_type() {
        assert_eq!(
            vh_indirect_box_input(Value::Int(1), Some("Z")),
            Some((Value::Int(1), "Z"))
        );
        assert_eq!(
            vh_indirect_box_input(Value::Int(3), Some("J")),
            Some((Value::Long(3), "J"))
        );
        assert_eq!(
            vh_indirect_box_input(Value::Int(3), Some("Ljava/lang/Object;")),
            Some((Value::Int(3), "I"))
        );
        assert_eq!(
            vh_indirect_box_input(Value::Long(3), None),
            Some((Value::Long(3), "J"))
        );
        assert_eq!(vh_indirect_box_input(Value::Object(None), Some("I")), None);
    }

    #[test]
    fn a_handle_that_is_not_indirect_is_left_to_the_native() {
        let mut ctx = mock_ctx();
        let cid = class(&mut ctx, "p/R14w7Plain");
        let vh = ctx.alloc_object(cid, 6);
        assert!(
            vh_indirect_serve(&mut ctx, &[Value::Object(Some(vh)), Value::Int(1)], "get").is_none()
        );
    }

    #[test]
    fn get_invokes_the_cached_mode_handle_with_the_direct_target_leading() {
        let mut f = fixture();
        let mh = f.ctx.alloc_object(f.other_cid, 1);
        f.ctx.set_array_element(f.table, 0, Value::Object(Some(mh)));
        f.ctx.set_invoke_virtual_hook(echo::<0>);
        let got = varhandle_get(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
        assert_eq!(got.ok(), Some(Some(Value::Object(Some(f.direct)))));
    }

    #[test]
    fn a_primitive_coordinate_reaches_the_mode_handle_boxed() {
        let mut f = fixture();
        let mh = f.ctx.alloc_object(f.other_cid, 1);
        f.ctx.set_array_element(f.table, 0, Value::Object(Some(mh)));
        f.ctx.set_invoke_virtual_hook(echo::<1>);
        let got = varhandle_get(
            &mut f.ctx,
            &[Value::Object(Some(f.vh)), Value::Int(100_000)],
        );
        let Ok(Some(Value::Object(Some(boxed)))) = &got else {
            panic!("the coordinate did not arrive boxed: {got:?}");
        };
        assert_eq!(f.ctx.get_field(*boxed, 0), Value::Int(100_000));
    }

    #[test]
    fn set_answers_void_after_the_mode_handle_runs() {
        let mut f = fixture();
        let mh = f.ctx.alloc_object(f.other_cid, 1);
        f.ctx.set_array_element(f.table, 1, Value::Object(Some(mh)));
        f.ctx.set_invoke_virtual_hook(echo::<0>);
        let got = varhandle_set(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
        assert_eq!(got.ok(), Some(None));
    }

    #[test]
    fn an_access_that_cannot_be_served_throws_instead_of_answering_null() {
        // No cached handle and no `target`: before wave 7 this answered
        // `Ok(Some(null))` / `Ok(None)`.
        let mut f = fixture();
        let get = varhandle_get(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
        let err = match get {
            Err(err) => format!("{err:?}"),
            Ok(v) => panic!("an unserved get answered {v:?}"),
        };
        assert!(err.contains("IndirectVarHandle.get"), "{err}");
        let cas = varhandle_compare_and_set(
            &mut f.ctx,
            &[Value::Object(Some(f.vh)), Value::Int(1), Value::Int(2)],
        );
        assert!(cas.is_err(), "an unserved compareAndSet answered {cas:?}");
    }

    #[test]
    fn var_type_of_an_indirect_handle_is_its_value_field() {
        let mut f = fixture();
        let mirror = f.ctx.alloc_object(f.other_cid, 1);
        f.ctx.set_field(f.vh, 4, Value::Object(Some(mirror)));
        let got = varhandle_var_type(&mut f.ctx, &[Value::Object(Some(f.vh))]);
        assert_eq!(got.ok(), Some(Some(Value::Object(Some(mirror)))));
    }
}

/// `CRATONVM_VH_ARITY_WMTE=0` — restore the pre-wave-19 behaviour, in which a
/// `VarHandle` access whose argument COUNT does not match the access mode's
/// type ran anyway (a surplus trailing argument was dropped, a missing one
/// read as `0`/`null`). Default on. Read only on the refusal path, so it costs
/// an exact-arity call nothing.
pub(crate) fn vh_arity_wmte_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_VH_ARITY_WMTE").as_deref(),
        Ok("0")
    )
}

/// Interpreter round i1 wave 47, lane L4
/// (`interpreter-L4-proposal-one-varhandle-row-read-per-access-FIXED-20261011`): the `get*`, `set*`
/// and `compareAndSet` / `weakCompareAndSet*` natives read their handle's
/// side-table row ONCE, at the top ([`vh_prologue_row`]), and hand it to the
/// prologue's checks and the body, which each read it again before; and an
/// INSTANCE or STATIC row skips the two FFM predicates
/// (`is_segment_var_handle`, a class-name fetch, and `p67_segment_vh_shape`,
/// a second memo probe), which can only answer "no" for it: such a row is
/// written only on a fresh synthetic `VarHandle` by `alloc_instance_var_handle`
/// / `alloc_static_var_handle` (copied by `varhandle_with_exactness`), none of
/// which is a `SegmentVarHandle` or registers a layout shape, and both tables
/// drop a dead handle's lock key together (`forget_var_handle_meta_keys`).
/// The row holds no object reference, so it stays valid across the GC point
/// of the value adaptation. `false` restores the per-step reads. A
/// compile-time switch: one constant branch, no per-VM state.
pub(crate) const VARHANDLE_ONE_ROW_READ: bool = true;

/// The row an access native's prologue read: `Some(row)` (itself `None` for
/// a handle with no row) when [`VARHANDLE_ONE_ROW_READ`] is on, `None` when
/// every step reads its own.
pub(crate) type VhPrologueRow = Option<Option<Arc<VarHandleMeta>>>;

/// Read the handle's row once for an access native (`None` with the switch
/// off, or with no handle to ask).
#[inline]
pub(crate) fn vh_prologue_row(ctx: &mut dyn NativeContext, args: &[Value]) -> VhPrologueRow {
    if !VARHANDLE_ONE_ROW_READ {
        return None;
    }
    match args.first() {
        Some(Value::Object(Some(vh))) => Some(vh_meta_get(ctx, *vh)),
        _ => None,
    }
}

/// `vh`'s row: the prologue's when it read one, else a fresh read.
#[inline]
pub(crate) fn vh_row_or_read(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    row: &VhPrologueRow,
) -> Option<Arc<VarHandleMeta>> {
    match row {
        Some(meta) => meta.clone(),
        None => vh_meta_get(ctx, vh),
    }
}

/// The prologue read an INSTANCE or STATIC row, for which the FFM predicates
/// can only answer "no" (see [`VARHANDLE_ONE_ROW_READ`]).
#[inline]
pub(crate) fn vh_row_is_field(row: &VhPrologueRow) -> bool {
    matches!(row, Some(Some(m)) if m.kind == VH_KIND_INSTANCE || m.kind == VH_KIND_STATIC)
}

/// `vh`'s FFM layout shape, not asked for an INSTANCE or STATIC row, which
/// cannot have one (see [`VARHANDLE_ONE_ROW_READ`]).
#[inline]
pub(crate) fn vh_layout_shape_unless_field(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    row: &VhPrologueRow,
) -> Option<SegmentVhShape> {
    if vh_row_is_field(row) {
        return None;
    }
    p67_segment_vh_shape(ctx, vh)
}

/// The `asType` ARITY check every access mode owes its call site, before any
/// value is looked at (round 11 wave 19, lane vh;
/// r11w18-orch-varhandle-set-accepts-a-surplus-reference-argument).
///
/// # What was wrong
///
/// javac does not check the arity of a signature-polymorphic call, so
/// `A.set(h, arr, "s")` on an `int[]` field handle compiles to
/// `VarHandle.set:(LH;[ILjava/lang/String;)V`. HotSpot's `asType` from the
/// access mode's type `(H,int[])void` to that one fails on the parameter
/// COUNT: `WrongMethodTypeException`, field untouched. Every native here read
/// the values it NEEDED by position and never looked at the rest, so the
/// surplus `String` was dropped and `arr` stored — in every tier, `--nojit`
/// included (`R11W18VhArrayValueArity`: `arity.set.String none=20000`). A
/// surplus PRIMITIVE was refused only by accident (the reference-variable
/// boxing pass judged the trailing `int` as the value: WMTE for the wrong
/// reason), and a MISSING value read as `Int(0)` and stored it.
///
/// # The rule
///
/// `args` is `[handle, coordinates..., values...]`, one `Value` per Java
/// parameter. The access mode fixes the value count (`access_type` is the
/// `VarHandle.AccessType` ordinal `vh_access_mode_descriptor` switches on:
/// `GET` 0, `SET` 1, `COMPARE_AND_SET` 2, `COMPARE_AND_EXCHANGE` 3,
/// `GET_AND_UPDATE` 4), the handle's kind fixes the coordinate count, and any
/// other total is WMTE. It runs after the read-only and unsupported-mode UOE
/// checks (HotSpot's `getMethodHandle(mode)` raises those before `asType`)
/// and before the null-coordinate NPE and the value adaptation (`asType`
/// precedes the access).
///
/// # Only what is known
///
/// The coordinate count is taken from an AUTHORITATIVE source only: the
/// side-table kind, or a real-JDK array-element handle's own class. A handle
/// with no side-table entry that is not a real array handle, and every FFM
/// layout/segment handle (whose coordinate lists are not modelled here), is
/// not judged — today's behaviour, not a guess.
///
/// # Why it is free
///
/// One `vh_meta_get` (the thread-local memo hit every native here already
/// pays two or three of) and an integer compare. The FFM screens, the
/// message and the kill switch are read on the refusal path only.
pub(crate) fn vh_check_arity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access_type: i32,
    mode: &'static str,
) -> Result<Option<i32>, MethodCallFailed> {
    vh_check_arity_row(ctx, args, access_type, mode, &None)
}

/// [`vh_check_arity`] over the row the access native's prologue already read
/// ([`vh_prologue_row`]; `None` reads it here, as before wave 47).
pub(crate) fn vh_check_arity_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access_type: i32,
    mode: &'static str,
    row: &VhPrologueRow,
) -> Result<Option<i32>, MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // No handle: `obj_arg` reports it, as it does today.
        _ => return Ok(None),
    };
    let values = match access_type {
        0 => 0,
        2 | 3 => 2,
        _ => 1,
    };
    let given = args.len() - 1;
    let meta = vh_row_or_read(ctx, this, row);
    let kind = meta.as_deref().map(|m| m.kind);
    let coords = match kind {
        Some(VH_KIND_STATIC) => 0,
        Some(VH_KIND_INSTANCE) => 1,
        Some(VH_KIND_ARRAY) => 2,
        Some(
            VH_KIND_BYTE_VIEW_LE
            | VH_KIND_BYTE_VIEW_BE
            | VH_KIND_BYTE_BUFFER_VIEW_LE
            | VH_KIND_BYTE_BUFFER_VIEW_BE,
        ) => {
            if let Some(m) = meta.as_deref() {
                vh_view_mode_refusal(ctx, args, m, mode)?;
            }
            2
        }
        // A kind this does not model (the FFM layout kind): not judged.
        Some(_) => return Ok(kind),
        None => {
            // Exact for an array handle: no class-name read on the hot path.
            if given == 2 + values {
                return Ok(None);
            }
            if real_array_var_handle_descriptors(ctx, this).is_none() {
                return Ok(None);
            }
            2
        }
    };
    if given == coords + values {
        return Ok(kind);
    }
    // The refusal path. An FFM handle is left alone even if a side-table row
    // answered for its identity hash: its natives decide it before the meta.
    if p67_segment_vh_shape(ctx, this).is_some() || is_segment_var_handle(ctx, this) {
        return Ok(kind);
    }
    if !vh_arity_wmte_enabled() {
        return Ok(kind);
    }
    let (_kind, value_desc, coord_descs, _exact) = vh_value_and_coordinate_descriptors(ctx, this);
    let desc = vh_access_mode_descriptor(access_type, &coord_descs, &value_desc);
    // `MethodType.toString` of the handle's invoker type, as HotSpot's
    // `asType` message spells it: the `VarHandle` leads the parameters.
    let shown = match method_type_display_in(&*ctx, &desc) {
        Some(s) if s.starts_with("()") => format!("(VarHandle{}", &s[1..]),
        Some(s) if s.starts_with('(') => format!("(VarHandle,{}", &s[1..]),
        Some(s) => s,
        None => desc,
    };
    // The door that knows the call site's descriptor rewrites this text as
    // HotSpot's `cannot convert MethodHandle(<own>) to <site>` (wave 53, L4).
    let text = format!(
        "cannot convert MethodHandle{shown}: the call site passes {given} argument(s) after the VarHandle, the access mode takes {}",
        coords + values
    );
    Err(vh_astype_refused(ctx, this, |ctx| {
        crate::phases_early::throw_jca_exc(ctx, "java/lang/invoke/WrongMethodTypeException", &text)
    }))
}

pub(crate) fn varhandle_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_mode(ctx, args, "get")
}

/// [`varhandle_get`] for the access mode `mode` (`get`, `getVolatile`,
/// `getOpaque`, `getAcquire`): the byte views admit fewer modes than a field.
pub(crate) fn varhandle_get_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    // Round 15 wave 3 (lane vh3; VH1-3): the parameter-site word, TAKEN first
    // like the store modes' cell, so a door's word never outlives this access.
    // Its coordinates are what an `IndirectVarHandle`'s `asType` judges.
    let site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    let call_site = cratonvm_native_api::poly_call_site::take();
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    // SBR3 lane ffmperf: a planned real `SegmentVarHandle` (it has no row) over
    // a real native segment, answered without the generic prologue; anything
    // the lean path cannot prove declines to the road below.
    if mode == "get" && matches!(row, Some(None)) {
        // r23 wave 2c, lane L8: a call site whose return is a primitive takes
        // the lean road only when it is the carrier itself (any conversion is
        // `asType`'s, below); a reference-class site declines.
        let ret = cratonvm_native_api::poly_call_site::vh_param_site_return(site);
        if ret == 0 || ret >= b'A' {
            if let Some(Some(value)) = segment_vh_lean_native(ctx, args, false, ret) {
                return Ok(Some(value));
            }
        }
    }
    // A combinator's handle is its adapter's to check (round 14 wave 7).
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, site, &row) {
        return served;
    }
    if let (Some(Some(meta)), Some(call_site)) = (&row, call_site.as_deref()) {
        let expected = if let Some((elem, _)) = byte_view_kind(Some(meta)) {
            vh_view_mode_refusal(ctx, args, meta, mode)?;
            Some(format!("([BI){}", elem as char))
        } else if meta.kind == VH_KIND_INSTANCE && !meta.class_name.is_empty() {
            Some(format!(
                "({}){}",
                class_name_to_descriptor(&meta.class_name),
                meta.field_desc
            ))
        } else if meta.kind == VH_KIND_STATIC {
            Some(format!("(){}", meta.field_desc))
        } else {
            None
        };
        if let Some(expected) = expected {
            // Judge the complete access type before coordinate faults. Carrier
            // mismatches that the thin helper declines must not silently narrow
            // or reinterpret on this shared native fallback.
            if method_type_is_convertible_to(&expected, call_site) == Some(false) {
                let old = vh_invoker_descriptor(&expected).unwrap();
                let new = vh_invoker_descriptor(call_site).unwrap();
                if let (Some(old), Some(new)) = (
                    method_type_display_in(&*ctx, &old),
                    method_type_display_in(&*ctx, &new),
                ) {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/invoke/WrongMethodTypeException",
                        &format!("cannot convert MethodHandle{old} to {new}"),
                    ));
                }
            }
        }
    }
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, site, &row)?;
    let result = varhandle_get_mode_access(ctx, args, mode, site, &row);
    // Wave 59 (lane L4): a wrapper-typed variable's `null` at its own
    // primitive's site.
    vh_own_wrapper_null(ctx, own, result)
}

/// [`varhandle_get_mode`] after its prologue and return-site check: the
/// access itself.
pub(crate) fn varhandle_get_mode_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
    site: u64,
    row: &VhPrologueRow,
) -> MethodCallResult {
    // Round 15 wave 3 (lane vh4; VH3-2): the index coordinate as `asType`
    // converts it, so every decode below reads the REAL index.
    let coordinate = vh_index_coordinate_args(ctx, args, 0, site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): the array coordinate's cast.
    vh_array_coordinate_cast(ctx, args, 0, mode)?;
    vh_check_arity_row(ctx, args, 0, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // An instance or static field row cannot be an FFM handle (wave 47).
    if !vh_row_is_field(&row) {
        // `SegmentVarHandle` (JEP 454 FFM API): a distinct real class with its own
        // (real) fields, no synthetic VH_KIND entry. Must be checked before the
        // meta/array/byte-view paths below, which don't apply to it.
        if is_segment_var_handle(ctx, this) {
            if let Some(refusal) = segment_vh_ordered_mode_refusal(ctx, this, mode) {
                return Err(refusal);
            }
            if let Some(result) = segment_vh_get(ctx, this, args) {
                if result.is_err() {
                    if let Some(jdk) = segment_vh_raise_through_jdk(ctx, this, args, mode) {
                        return jdk;
                    }
                }
                return result;
            }
        }
        // An FFM LAYOUT handle (`ValueLayout.JAVA_INT.varHandle()`,
        // `layout.varHandle(PathElement...)`) is a synthetic receiver whose meaning
        // lives only in the shape table, so it must be answered before the slot
        // reads below — which would take slot 0 (its endianness flag) for a kind
        // tag and route the access to the instance-field arm, where it silently
        // did nothing.
        if let Some(shape) = p67_segment_vh_shape(ctx, this) {
            // `layout_vh_get`, not `segment_vh_get`: the latter takes the real
            // `SegmentVarHandle` receiver handled above, this one takes the shape.
            // The `set` twin twelve lines below already had it right.
            return layout_vh_get(ctx, shape, args);
        }
    }
    // Round-7 HIGH-2 fix: fetch the side-table meta exactly once and reuse
    // the bound Arc for `kind`/`field_index`/`class_name`/`field_name`/
    // `field_desc`. Previously each helper (`vh_meta_get`, `vh_type_desc`,
    // the by-name resolve fallback) re-locked the table 2–3× per native.
    let meta = vh_row_or_read(ctx, this, &row);
    // byte-array-view (`asLittleEndian.get([BI)J` etc.): MUST be handled BEFORE
    // the array-element fast path, because both have the [vh, array, index]
    // arg shape — but a view reads `width` bytes at a BYTE index, whereas the
    // array path would return a single signed byte.
    if let Some((elem, le)) = byte_view_kind(meta.as_deref()) {
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let index = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let idx = match byte_view_check_index(ctx, arr, index, elem) {
            Ok(i) => i,
            Err(fallback) => {
                let at = Value::Object(Some(arr));
                let view = Some((elem, le, false));
                return Err(vh_index_raise_through_jdk(
                    ctx, this, view, mode, at, index, fallback,
                ));
            }
        };
        let value = byte_view_get(ctx, arr, idx, elem, le);
        // CANONICAL — measured `vh.byteViewInt` = true and `vh.byteViewLong`
        // = true. `byte_view_get` and `byte_view_desc` switch on the same
        // `elem`, so variant and descriptor agree.
        return Ok(Some(box_value_canonical(ctx, value, byte_view_desc(elem))));
    }
    if let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) {
        let mut bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Object(None))),
        };
        let index = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
            Ok(i) => i,
            Err(fallback) => {
                let at = Value::Object(Some(bb));
                let view = Some((elem, le, true));
                return Err(vh_index_raise_through_jdk(
                    ctx, this, view, mode, at, index, fallback,
                ));
            }
        };
        let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
        let value = byte_buffer_view_get(ctx, bb, idx, elem, le).unwrap_or(Value::Object(None));
        // CANONICAL — the `ByteBuffer` twin of the `byte[]` view above; same
        // measurement. Note the `unwrap_or(Value::Object(None))` on the line
        // before: on a failed read the pair is `(desc, Value::Object(None))`,
        // which matches none of the six cached arms, so it falls back to
        // `box_value` and keeps that (pre-existing, separately nominated)
        // shape byte-for-byte rather than caching a null-carrying wrapper.
        return Ok(Some(box_value_canonical(ctx, value, byte_view_desc(elem))));
    }
    // C38: Array-element VarHandle call — detected by args[1] being an array
    // and args[2] being an Int. Handles real-JDK VarHandleLongs$Array and the
    // other primitive-array VarHandle subclasses produced by
    // MethodHandles.arrayElementVarHandle. Returns the raw primitive Value
    // (Long, Int, Float, Double, Reference) — unbox_poly_return / the
    // signature-polymorphic call-site descriptor steers it to the right
    // primitive return slot.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let desc = array_element_desc(ctx, arr);
        let value = ctx.get_array_element(arr, idx);
        // CANONICAL — measured `vh.arrInt` / `arrChar` / `arrBool` / `arrLong`
        // = true. NOTE the contrast this must not be "unified" with:
        // `java.lang.reflect.Array.get` on the SAME `int[]` is FRESH on both
        // VMs (`array.int` = false, and `array.selfid` = false — it is not
        // even identical to itself). Two reads of one array element, two
        // different contracts, because `Array.get` is `Reflection::array_get`
        // -> `create()` while a `VarHandle` gets a `valueOf`-shaped adapter.
        return Ok(Some(box_value_canonical(ctx, value, desc)));
    }
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] is the receiver object directly (signature-polymorphic dispatch)
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
            if field_idx >= 0 {
                // zt t2/nd: a VarHandle read of `Reference.referent` keeps the
                // referent alive (`slot0_read_keep_alive`).
                let slot = field_idx as usize;
                let raw = ctx.get_field(receiver, slot);
                let raw = crate::unsafe_natives::slot0_read_keep_alive(ctx, receiver, slot, raw);
                let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
                // CANONICAL — measured `vh.fieldInt` / `fieldChar` /
                // `fieldBool` / `fieldLong` / `fieldByte` / `fieldShort` =
                // true, and `vh.fieldBoolTRUE` = true (a `boolean` field read
                // through a VarHandle IS `Boolean.TRUE`, not a look-alike).
                //
                // `ctx.get_field` returns the RAW slot, which for a `long`
                // field can present as a compact `Value::Int` — which is why
                // the read is wrapped in `coerce_reflective_field_value`, the
                // SAME function `Field.get` uses. Without it this arm produced
                // a `Long` wrapper whose slot 0 held raw compact-`Int` bits: a
                // wrong ANSWER, not merely a non-canonical one, and one the
                // helper's variant guard could only downgrade to a fresh box
                // rather than repair.
                //
                // F19-1 §5.5 listed `vh.fieldLong`'s HotSpot verdict as
                // **unknown**. It is now MEASURED on OpenJDK 25.0.3+9
                // (`VhLong.java`, identical under `-Xint`), and the direction
                // is the same as `Field.get`'s — widen FIRST, then box
                // canonically, not the reverse:
                //
                //   vh.fieldLong      = java.lang.Long, value 5,  id true
                //   vh.fieldLongZero  = value 0,                  id true
                //   vh.fieldDouble    = java.lang.Double, 1.5,    id FALSE
                //   vh.getAndSetLong  = old value 5,              id true
                //   field.long        = value 5,                  id true
                //
                // The fix is `coerce_reflective_field_value` at this arm,
                // at the by-name arm below, at `VH_KIND_STATIC` (whose
                // `get_static_field` is equally raw), and at
                // `vh_box_access_result` — which the `getAndSet` row above
                // puts inside the family rather than beside it. All FOUR are
                // applied; three of them would have looked like the whole set.
                // `Double` is NOT an identity row (id false) but IS a value
                // row: a `double` slot carrying raw bits as a `Value::Long`
                // must be REINTERPRETED, which is the arm this file's own
                // `widen_primitive_to_descriptor` would get WRONG — it
                // converts numerically, so `1.5` comes back as `4.609e18`.
                // One rule, one implementation, and it is `lang_class`'s;
                // substituting the local widener here is a silent wrong
                // answer, not a shortcut.
                Ok(Some(box_value_canonical(ctx, val, &td)))
            } else {
                // Resolve by name (reuse the meta Arc we already hold).
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(idx) => {
                        // Cache for next time
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                        vh_meta_update_field_index(ctx, this, idx as i32);
                        // Widened through the SAME function as the
                        // `field_idx >= 0` arm above: a VarHandle that
                        // resolved late must not answer differently from one
                        // that resolved early, in the value any more than in
                        // the identity.
                        // zt t2: the same `Reference.referent` keep-alive as
                        // the by-index arm.
                        let raw = ctx.get_field(receiver, idx);
                        let raw =
                            crate::unsafe_natives::slot0_read_keep_alive(ctx, receiver, idx, raw);
                        let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
                        // Reuse already-computed type descriptor. CANONICAL,
                        // for the same measurement as the `field_idx >= 0`
                        // arm twenty lines up — this is the same read after a
                        // by-name resolve, and a VarHandle that happened to
                        // resolve late must not answer with a different
                        // identity than one that resolved early.
                        Ok(Some(box_value_canonical(ctx, val, &td)))
                    }
                    None => Ok(Some(Value::Object(None))),
                }
            }
        }
        VH_KIND_STATIC => {
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            let raw = match vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                Some((cid, sidx)) => ctx.get_static_field(cid, sidx),
                None => return Ok(Some(Value::Object(None))),
            };
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
            // Widen BEFORE boxing, and therefore after `td` — the read above
            // is deliberately named `raw`, because that is what
            // `get_static_field` hands back.
            let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
            // CANONICAL — measured `vh.staticInt` = true. The out-of-bound
            // twin is measured too and is the arm the helper delegates:
            // `vhoob.staticInt1000` = false.
            //
            // `get_static_field` is as raw as `get_field`, so this arm is a
            // member of the widening set — see the block on the
            // `VH_KIND_INSTANCE` arm above for the measurement and for why
            // this file's own `widen_primitive_to_descriptor` is the wrong
            // function for it.
            Ok(Some(box_value_canonical(ctx, val, &td)))
        }
        VH_KIND_ARRAY => {
            // args = [vh, array, index]
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Object(None)));
            };
            // W8-13: this branch returned the BARE element — the one
            // unboxed return left in `varhandle_get`, and the same defect the
            // RMW modes had. It is normally shadowed by the `vh_array_call`
            // fast path above, which matches whenever args[2] is an `Int`; it
            // is reachable only when the index coordinate is absent or not an
            // `Int`, i.e. exactly the shape a partial fix would leave behind.
            let value = ctx.get_array_element(arr, idx);
            let desc = array_element_desc(ctx, arr);
            // CANONICAL, for the same measurement as the `vh_array_call` fast
            // path above. Switched even though the comment above says this
            // branch is normally shadowed: an arm that is reachable only in
            // the shape a partial fix leaves behind is exactly the arm that
            // must not disagree with its twin.
            Ok(Some(box_value_canonical(ctx, value, desc)))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// VarHandle.set(receiver, value) → void
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, value] for instance fields.
pub(crate) fn varhandle_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_set_mode(ctx, args, "set")
}

/// [`varhandle_set`] for the access mode `mode` (`set`, `setOpaque`,
/// `setRelease`, and `setVolatile` through [`varhandle_set_volatile`]).
pub(crate) fn varhandle_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    // FIRST, before any early return: the cell belongs to this call only.
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`). It
    // holds no object reference, so the value adaptation's GC point below
    // leaves it valid.
    let row = vh_prologue_row(ctx, args);
    // SBR3 lane ffmperf: see `varhandle_get_mode`.
    if mode == "set"
        && matches!(row, Some(None))
        && segment_vh_lean_native(ctx, args, true, 0).is_some()
    {
        return Ok(None);
    }
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    vh_check_arity_row(ctx, args, 1, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // `asType`'s unbox/widen of the value for a primitive variable -- see
    // `vh_adapt_value_args`. Before this, `set(h, (Object) Integer.valueOf(i))`
    // on an `int` field stored the Integer's ADDRESS.
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    // Re-read: boxing a primitive into a reference variable allocates, and
    // the adapted vector carries the handle's post-GC address.
    let this = obj_arg(args, 0)?;
    // An instance or static field row cannot be an FFM handle (wave 47).
    if !vh_row_is_field(&row) {
        // `SegmentVarHandle` (JEP 454 FFM API): see the matching check in `varhandle_get`.
        if is_segment_var_handle(ctx, this) {
            if let Some(refusal) = segment_vh_ordered_mode_refusal(ctx, this, mode) {
                return Err(refusal);
            }
            if let Some(result) = segment_vh_set(ctx, this, args) {
                if result.is_err() {
                    if let Some(jdk) = segment_vh_raise_through_jdk(ctx, this, args, mode) {
                        return jdk;
                    }
                }
                return result;
            }
        }
        // FFM layout handle — see the matching check in `varhandle_get` for what
        // fell through here before (a silently dropped write).
        if let Some(shape) = p67_segment_vh_shape(ctx, this) {
            return layout_vh_set(ctx, shape, args);
        }
    }
    // Round-7 HIGH-2 fix: bind the Arc once and reuse for kind / field_index
    // / class+field lookups instead of re-locking `vh_meta_table` each branch.
    let meta = vh_row_or_read(ctx, this, &row);
    // byte-array-view (`asLittleEndian.set([BIJ)V` etc.): handle BEFORE the
    // array-element fast path — a view writes `width` bytes at a BYTE index,
    // not a single element. (Writing the wide value into one byte slot was the
    // SHA3/SHAKE-zeros bug.)
    if let Some((elem, le)) = byte_view_kind(meta.as_deref()) {
        if let Some(Value::Object(Some(arr))) = args.get(1).copied() {
            let index = match args.get(2).copied() {
                Some(Value::Int(i)) => i,
                _ => 0,
            };
            let idx = match byte_view_check_index(ctx, arr, index, elem) {
                Ok(i) => i,
                Err(fallback) => {
                    let at = Value::Object(Some(arr));
                    let view = Some((elem, le, false));
                    return Err(vh_index_raise_through_jdk(
                        ctx, this, view, mode, at, index, fallback,
                    ));
                }
            };
            let value = args.get(3).cloned().unwrap_or(Value::Int(0));
            byte_view_set(ctx, arr, idx, elem, le, &value);
        }
        return Ok(None);
    }
    if let Some((elem, le)) = byte_buffer_view_kind(meta.as_deref()) {
        if let Some(Value::Object(Some(mut bb))) = args.get(1).copied() {
            let index = match args.get(2).copied() {
                Some(Value::Int(i)) => i,
                _ => 0,
            };
            if byte_buffer_view_is_read_only(ctx, bb) {
                return Err(RuntimeError::ReadOnlyBufferException.into());
            }
            let idx = match byte_buffer_view_check_index(ctx, bb, index, elem) {
                Ok(i) => i,
                Err(fallback) => {
                    let at = Value::Object(Some(bb));
                    let view = Some((elem, le, true));
                    return Err(vh_index_raise_through_jdk(
                        ctx, this, view, mode, at, index, fallback,
                    ));
                }
            };
            let _window = byte_buffer_view_access_window(ctx, &mut bb)?;
            let value = args.get(3).cloned().unwrap_or(Value::Int(0));
            byte_buffer_view_set(ctx, bb, idx, elem, le, &value);
        }
        return Ok(None);
    }
    // C38: Array-element VarHandle.set — args = [vh, array, idx, value]. Handles
    // real-JDK VarHandleLongs$Array / VarHandleInts$Array / etc.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let value = args.get(3).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 5 (lane vh7; F3): the value's cast before the index,
        // as HotSpot 25 (`set` only; the RMW arms below check the index first).
        if let Some(refusal) = vh_array_casts_before_index(ctx, this, arr, value, mode) {
            return Err(refusal);
        }
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        if let Some(refusal) = vh_array_refusal_after_index(ctx, this, arr, value, mode) {
            return Err(refusal);
        }
        ctx.set_array_element(arr, idx, value);
        return Ok(None);
    }
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(None),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] = receiver, args[2] = value (signature-polymorphic dispatch)
            let receiver = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(None),
            };
            let value = args.get(2).cloned().unwrap_or(Value::Int(0));
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, Some(value))
            {
                return Err(refusal);
            }

            if field_idx >= 0 {
                ctx.set_field(receiver, field_idx as usize, value);
            } else {
                // By name in the field's holder, not the coordinate.
                if let Some(idx) = vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                    vh_meta_update_field_index(ctx, this, idx as i32);
                    ctx.set_field(receiver, idx, value);
                }
            }
            Ok(None)
        }
        VH_KIND_STATIC => {
            // args[1] = value
            let value = args.get(1).cloned().unwrap_or(Value::Int(0));
            // Judged like an instance store; see `vh_value_refusal`.
            if let Some(m) = meta.as_deref() {
                if let Some(refusal) = vh_value_refusal(ctx, m, value) {
                    return Err(refusal);
                }
            }
            let mut live = [value];
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut live,
                vh_static_init(mode, meta.as_deref()),
            )? {
                ctx.set_static_field(cid, sidx, live[0]);
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// `VarHandle.setVolatile` — [`varhandle_set`] between two SeqCst fences.
///
/// `varhandle_set` stores with the PLAIN store (`set_field`, `set_array_element`,
/// `set_static_field`, the view/segment writers), which is right for `set`,
/// `setOpaque` and `setRelease` on x86-64 but not for `setVolatile`: a volatile
/// store must not be reordered with a LATER volatile load (StoreLoad), which
/// is what a Dekker handshake written with `setVolatile`/`getVolatile` relies
/// on. The trailing fence is that barrier (HotSpot's `lock addl`); the leading
/// one orders every earlier access before the store, which `setRelease`
/// already gets from x86-64 TSO but a weaker target would not. Wrapping the
/// whole native rather than its instance arm covers every handle shape the
/// native serves — static, array element, byte view, segment — with one rule.
pub(crate) fn varhandle_set_volatile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    let result = varhandle_set_mode(ctx, args, "setVolatile");
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    result
}

/// VarHandle.compareAndSet(receiver, expected, new) → boolean
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, expected, new_value] for instance fields.
pub(crate) fn varhandle_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_compare_and_set_mode(ctx, args, "compareAndSet")
}

/// [`varhandle_compare_and_set`] for `mode` (`compareAndSet` and the four
/// `weakCompareAndSet*`).
pub(crate) fn varhandle_compare_and_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 2, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 2, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 2, mode)?;
    let kind = vh_check_arity_row(ctx, args, 2, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    // BOTH values are adapted: an `Integer` expectation through an `Object`
    // site never matched an `int` slot, so the CAS always failed.
    let adapted = vh_adapt_value_args_row(ctx, this, args, 2, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::CompareAndSet, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`. Not for an
    // instance or static field row (wave 47).
    if !vh_row_is_field(&row) {
        if let Some(shape) = p67_segment_vh_shape(ctx, obj_arg(args, 0)?) {
            return layout_vh_rmw(ctx, shape, args, VhViewRmw::CompareAndSet, mode);
        }
    }
    if is_byte_buffer_view_kind(kind) {
        return byte_buffer_view_rmw(ctx, args, VhViewRmw::CompareAndSet);
    }
    // Re-read, as in `varhandle_set`: the adapted vector is post-allocation.
    let this = obj_arg(args, 0)?;
    // C38: Array-element CAS — args = [vh, array, idx, expected, new_value].
    // Index first: HotSpot 25 answers an out-of-bounds CAS with a wrong-typed
    // value AIOOBE (`R15Vh7ArrayCastOrder` row `casOob`); only `set` casts
    // first (round 15 wave 5, lane vh7).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 4 (lane vh5): the expectation is cast first.
        if let Some(refusal) = vh_array_expected_refusal(ctx, this, arr, expected) {
            return Err(refusal);
        }
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // LOW-finding fix: atomic compare-and-set. The old get/compare/set was
        // a non-atomic RMW (could spuriously succeed against a value another
        // thread changed). `compare_and_swap_field` does the compare+write
        // under the VM's per-element CAS lock and returns whether it swapped.
        let ok = ctx.compare_and_swap_field(arr, idx, expected, new_val);
        return Ok(Some(Value::Int(ok as i32)));
    }
    // Round-7 HIGH-2 fix: fetch meta once and reuse across the kind probe
    // and the class/field fallback below.
    let meta = vh_row_or_read(ctx, this, &row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static CAS — args = [vh, expected, new_value]. (Array CAS was already
        // routed above via `vh_array_call`.)
        let expected = args.get(1).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
        // The expectation, then the NEW value, are cast to the variable's
        // type, as the JDK's body casts them (nio21 wave 12, lane L3; see
        // `vh_static_cas_refusal`).
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_static_cas_refusal(ctx, m, expected, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [expected, new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Int(0)));
        };
        let [expected, new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((_, ok)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
            vh_values_match(&current, &expected).then_some(new_val)
        }) {
            return Ok(Some(Value::Int(ok as i32)));
        }
        // LOW-finding fix: atomic static CAS — serialize the compare+set on the
        // per-class mirror monitor (no CAS primitive exists for static slots).
        let ok = vh_with_static_lock(ctx, cid, &[expected, new_val], |ctx, live| {
            let (expected, new_val) = (live[0], live[1]);
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((_, ok)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
                vh_values_match(&current, &expected).then_some(new_val)
            }) {
                return ok;
            }
            let current = ctx.get_static_field(cid, sidx);
            if vh_values_match(&current, &expected) {
                ctx.set_static_field(cid, sidx, new_val);
                true
            } else {
                false
            }
        });
        return Ok(Some(Value::Int(ok as i32)));
    }
    if kind != VH_KIND_INSTANCE {
        // Unsupported VarHandle kind — report CAS failure rather than a
        // false success.
        return Ok(Some(Value::Int(0)));
    }

    // args[1] = receiver, args[2] = expected, args[3] = new_value
    let receiver = match args.get(1).copied() {
        Some(Value::Object(Some(r))) => r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
    // The receiver, the expectation and the NEW value are cast in the JDK's
    // argument order. A wrong-typed expectation used to fail the comparison
    // here (answering `false`); HotSpot 25's body casts it
    // (`fieldType.cast(expected)`) and throws `ClassCastException` (nio21
    // wave 12, lane L3; `vh_instance_cas_refusal`).
    if let Some(refusal) =
        vh_instance_cas_refusal(ctx, meta.as_deref(), receiver, expected, new_val)
    {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                vh_meta_update_field_index(ctx, this, i as i32);
                i
            }
            None => return Ok(Some(Value::Int(0))),
        }
    };

    // LOW-finding fix: atomic compare-and-set on the instance field. The old
    // get/compare/set was a non-atomic RMW (could spuriously succeed against a
    // value another thread changed between the read and the write).
    // `compare_and_swap_field` does the compare+write under the VM's per-object
    // CAS lock and returns whether the swap happened.
    let ok = ctx.compare_and_swap_field(receiver, idx, expected, new_val);
    Ok(Some(Value::Int(ok as i32)))
}

/// VarHandle.compareAndExchange(receiver, expected, new) → witness value
/// Signature-polymorphic: args = [vh_ref, receiver, expected, new_value]
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub(crate) fn varhandle_compare_and_exchange(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_compare_and_exchange_mode(ctx, args, "compareAndExchange")
}

/// [`varhandle_compare_and_exchange`] for `mode` (and its `Acquire` /
/// `Release` variants).
pub(crate) fn varhandle_compare_and_exchange_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 2, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 2, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 2, mode)?;
    let kind = vh_check_arity_row(ctx, args, 3, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 2, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) =
        segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::CompareAndExchange, mode)
    {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::CompareAndExchange, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::CompareAndExchange)
    } else {
        varhandle_compare_and_exchange_raw(ctx, args, &row, mode)
    };
    let result = vh_box_access_result(ctx, args, raw);
    vh_own_wrapper_null(ctx, own, result)
}

pub(crate) fn varhandle_compare_and_exchange_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element compareAndExchange — args = [vh, array, idx, expected, new].
    // Index first, as HotSpot 25 (`R15Vh7ArrayCastOrder` row `caeOob`).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        // Round 15 wave 4 (lane vh5): the expectation is cast first.
        if let Some(refusal) = vh_array_expected_refusal(ctx, this, arr, expected) {
            return Err(refusal);
        }
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // LOW-finding fix: atomic compare-and-exchange. The previous
        // get/compare/set was a non-atomic RMW (lost updates under
        // contention). `compare_and_swap_field` runs the compare+write under
        // the VM's per-element CAS lock (its impl handles array refs).
        // compareAndExchange returns the *witness*; see
        // `vh_compare_and_exchange_slot` (interpreter round i2 wave 55, lane
        // L7) for why a failed CAS is not answered by one re-read.
        return Ok(Some(vh_compare_and_exchange_slot(
            ctx, arr, idx, true, expected, new_val,
        )));
    }
    // Meta side-table FIRST — real-JDK VarHandles don't carry our synthetic
    // 6-field layout (see `varhandle_get_and_bitwise`).
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static compareAndExchange — args = [vh, expected, new_value]; returns
        // the witness (value found), updated iff it equalled `expected`.
        let expected = args.get(1).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
        // The expectation, then the NEW value, are cast to the variable's
        // type, as the JDK's body casts them (nio21 wave 12, lane L3; see
        // `vh_static_cas_refusal`).
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_static_cas_refusal(ctx, m, expected, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [expected, new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Object(None)));
        };
        let [expected, new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((current, _)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
            vh_values_match(&current, &expected).then_some(new_val)
        }) {
            return Ok(Some(current));
        }
        // LOW-finding fix: no CAS primitive exists for static slots, so make
        // the compare-and-exchange atomic by serializing get+set on the
        // per-class mirror monitor (see `vh_with_static_lock`).
        let current = vh_with_static_lock(ctx, cid, &[expected, new_val], |ctx, live| {
            let (expected, new_val) = (live[0], live[1]);
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((current, _)) = ctx.static_field_hw_update(cid, sidx, &mut |current| {
                vh_values_match(&current, &expected).then_some(new_val)
            }) {
                return current;
            }
            let current = ctx.get_static_field(cid, sidx);
            if vh_values_match(&current, &expected) {
                ctx.set_static_field(cid, sidx, new_val);
            }
            current
        });
        return Ok(Some(current));
    }
    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
    // MEASURED on HotSpot 25.0.3+9 before wiring, `probes/ReflectArgTypeSweep.java`:
    // `getAndSet`, `getAndAdd` and `compareAndExchange` all raise
    // ClassCastException on a wrong receiver too, so all three doors get the
    // same check as `set`/`get`/`compareAndSet`. They were left out of the
    // first pass because only the latter three had been measured, and an
    // unmeasured door is where a fix at the wrong level starts. The
    // expectation is cast too, before the new value (nio21 wave 12, lane L3;
    // `vh_instance_cas_refusal`).
    if let Some(refusal) =
        vh_instance_cas_refusal(ctx, meta.as_deref(), receiver, expected, new_val)
    {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    // LOW-finding fix: atomic compare-and-exchange on the instance field.
    // The old get/compare/set was a non-atomic RMW and lost updates under
    // contention; `compare_and_swap_field` performs the compare+write under
    // the VM's per-object CAS lock. compareAndExchange returns the witness:
    // `expected` on a successful swap, else a value the field held that is
    // not `expected` (`vh_compare_and_exchange_slot`).
    Ok(Some(vh_compare_and_exchange_slot(
        ctx, receiver, idx, false, expected, new_val,
    )))
}

/// The witness of a `VarHandle.compareAndExchange` on slot `idx` of `obj` (an
/// array element when `is_array`): `expected` when the exchange happened,
/// otherwise a value the variable held at some instant during the call that
/// is NOT `expected` -- HotSpot's one `cmpxchg` answers with exactly such a
/// value (interpreter round i2 wave 55, lane L7;
/// `docs/feature-designs/profile-guided-inlining.md`).
///
/// The trait's CAS answers only "swapped or not". The witness used to be one
/// re-read after a failed CAS, and another thread may store `expected` in
/// between: the caller then saw `expected` although nothing was exchanged (a
/// `while ((w = cae(e, n)) != e) e = w;` loop exits without writing `n`, a
/// lost update). Here a re-read that equals `expected` is not an answer: the
/// exchange is attempted again, as the `Unsafe.compareAndExchange*` natives'
/// read-then-CAS loop does (`unsafe_compare_exchange_heap_slot`). The common
/// success path is still one CAS. `vh_values_match` is no looser than the
/// VM's CAS equality (`vm_exec::values_equal_for_cas`), so a re-read it calls
/// equal is one the next CAS compares equal too unless the variable moved
/// again. A context whose CAS equality is NARROWER (a test double comparing
/// floats with `==`, so a NaN never compares equal) would never converge, so
/// the retries of a FLOATING expectation are bounded (4096) and the last
/// re-read answers. An `int` / `long` / reference expectation gets a bound
/// 256 times larger (interpreter round i4 wave 92, lane L4): the VM's CAS
/// equality on those is exactly `vh_values_match`'s, so the loop ends unless
/// another thread restores `expected` between EVERY CAS and its re-read, and
/// the old 4096 could then answer `expected` for an exchange that did not
/// happen -- the witness HotSpot's `cmpxchg` never gives (probe
/// `L4W92CaeWitnessContention`). Not unbounded: a test double whose CAS reads
/// a slot differently from the re-read (an array element through
/// `get_field`) must still terminate.
pub(crate) fn vh_compare_and_exchange_slot(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    idx: usize,
    is_array: bool,
    expected: Value,
    new_val: Value,
) -> Value {
    let max_equal_rereads: u32 = if matches!(expected, Value::Float(_) | Value::Double(_)) {
        1 << 12
    } else {
        1 << 20
    };
    let mut attempts = 0u32;
    loop {
        if ctx.compare_and_swap_field(obj, idx, expected.clone(), new_val.clone()) {
            return expected;
        }
        let current = if is_array {
            ctx.get_array_element(obj, idx)
        } else {
            ctx.get_field_volatile(obj, idx)
        };
        attempts += 1;
        if !vh_values_match(&current, &expected) || attempts >= max_equal_rereads {
            return if is_array {
                current
            } else {
                crate::unsafe_natives::slot0_read_keep_alive(ctx, obj, idx, current)
            };
        }
        if attempts % 64 == 0 {
            std::thread::yield_now();
        }
    }
}

/// VarHandle.getAndSet(receiver, new) → old value
/// Signature-polymorphic: args = [vh_ref, receiver, new_value]
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub(crate) fn varhandle_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_set_mode(ctx, args, "getAndSet")
}

/// [`varhandle_get_and_set`] for `mode` (and its `Acquire` / `Release`
/// variants).
pub(crate) fn varhandle_get_and_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    let own = vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::GetAndSet, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::GetAndSet, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::GetAndSet)
    } else {
        varhandle_get_and_set_raw(ctx, args, &row, mode)
    };
    let result = vh_box_access_result(ctx, args, raw);
    vh_own_wrapper_null(ctx, own, result)
}

/// Where a [`vh_cas_rmw`] slot lives: an array element or an instance field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VhRmwSlot {
    ArrayElement,
    Field,
}

/// Yield the CPU every this many failed CASes of one [`vh_cas_rmw`].
pub(crate) const VH_RMW_YIELD_EVERY: usize = 1024;

/// The atomic read-modify-write behind the native `VarHandle` `getAndSet`,
/// `getAndAdd` and `getAndBitwise*` on an array element or an instance field:
/// read, compute `update(old)`, CAS, and retry until the CAS lands; answers
/// the `old` it replaced.
///
/// nio21 wave 6 L1
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// these loops gave up after 1024 failed CASes and finished with a plain read
/// and a plain store, so an update landing between the two was lost and two
/// threads in the fallback could both answer the same `old`. HotSpot never
/// loses one. A failed CAS means another thread's update landed (the element
/// CAS is a hardware atomic since nio21 wave 5 L3), so the loop terminates;
/// it yields every [`VH_RMW_YIELD_EVERY`] failures, as the `Unsafe` CAS loops
/// do. `CRATONVM_VH_RMW_UNBOUNDED=0` restores the bounded loop with the plain
/// fallback (read only once 1024 CASes have failed).
pub(crate) fn vh_cas_rmw(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    idx: usize,
    slot: VhRmwSlot,
    update: impl Fn(&Value) -> Value,
) -> Value {
    fn read(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize, slot: VhRmwSlot) -> Value {
        match slot {
            VhRmwSlot::ArrayElement => ctx.get_array_element(obj, idx),
            VhRmwSlot::Field => ctx.get_field_volatile(obj, idx),
        }
    }
    // The pre-wave-6 fallback: a plain read and a plain store.
    fn plain_store(
        ctx: &mut dyn NativeContext,
        obj: ObjectRef,
        idx: usize,
        slot: VhRmwSlot,
        update: &dyn Fn(&Value) -> Value,
    ) -> Value {
        let old = read(&*ctx, obj, idx, slot);
        let new_val = update(&old);
        match slot {
            VhRmwSlot::ArrayElement => ctx.set_array_element(obj, idx, new_val),
            VhRmwSlot::Field => ctx.set_field_volatile(obj, idx, new_val),
        }
        old
    }
    let mut failures = 0usize;
    // nio21 wave 7 L1: consecutive failed CASes whose read answered the
    // bit-identical value. Under contention a failure means the slot changed
    // between our read and our CAS, so the next read differs (it can only
    // repeat after an A-B-A between every read and CAS); a long run of
    // identical reads is a CAS that can never match what the read decodes
    // (an `Uninitialized` slot, which `values_equal_for_cas` never equates,
    // or a context whose compare is not bitwise, such as a `NaN` under a
    // `PartialEq` compare). The unbounded loop would spin there forever.
    let mut same_reads = 0usize;
    let mut last_read: Option<Value> = None;
    loop {
        let old = read(&*ctx, obj, idx, slot);
        let new_val = update(&old);
        if ctx.compare_and_swap_field(obj, idx, old, new_val) {
            return old;
        }
        failures += 1;
        if last_read.is_some_and(|l| vh_rmw_same_bits(&l, &old)) {
            same_reads += 1;
        } else {
            same_reads = 0;
        }
        last_read = Some(old);
        if same_reads == VH_RMW_STUCK_AFTER
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_RMW_STUCK_GUARD")
        {
            // Not contention: the CAS cannot land. Finish as a single-thread
            // HotSpot would (`update(old)` stored, `old` answered) rather than
            // hang the thread. `CRATONVM_VH_RMW_STUCK_GUARD=0` keeps spinning.
            return plain_store(ctx, obj, idx, slot, &update);
        }
        if failures % VH_RMW_YIELD_EVERY == 0 {
            if failures == VH_RMW_YIELD_EVERY
                && !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VH_RMW_UNBOUNDED")
            {
                return plain_store(ctx, obj, idx, slot, &update);
            }
            std::thread::yield_now();
        }
    }
}

/// Consecutive bit-identical failed reads after which [`vh_cas_rmw`] decides
/// its CAS cannot land (see `same_reads` there).
pub(crate) const VH_RMW_STUCK_AFTER: usize = 1 << 16;

/// Bit equality of two read values: `to_bits` for `float` / `double` (a `NaN`
/// equals itself), `PartialEq` for everything else.
pub(crate) fn vh_rmw_same_bits(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

#[cfg(test)]
mod nio21w6_vh_cas_rmw_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    /// The RMW answers the replaced value and leaves `update(old)` behind.
    #[test]
    fn r21w6_vh_cas_rmw_answers_old_and_stores_the_update() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(obj, 1, Value::Int(5));
        let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |v| match v {
            Value::Int(x) => Value::Int(x + 3),
            other => *other,
        });
        assert_eq!(old, Value::Int(5));
        assert_eq!(ctx.get_field(obj, 1), Value::Int(8));
        let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |_| Value::Int(-1));
        assert_eq!(old, Value::Int(8));
        assert_eq!(ctx.get_field(obj, 1), Value::Int(-1));
    }

    /// nio21 wave 7 L1: a CAS that can never match what the read decodes
    /// (the mock compares with `PartialEq`, so a `NaN` slot never equals the
    /// `NaN` it read) ends through the stuck guard instead of spinning.
    #[test]
    fn r21w7_vh_cas_rmw_stuck_guard_ends_a_cas_that_cannot_land() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(obj, 1, Value::Float(f32::NAN));
        let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |_| Value::Float(2.5));
        assert!(matches!(old, Value::Float(f) if f.is_nan()));
        assert_eq!(ctx.get_field(obj, 1), Value::Float(2.5));
    }

    #[test]
    fn r21w7_vh_rmw_same_bits_is_bitwise_for_floats() {
        assert!(vh_rmw_same_bits(
            &Value::Float(f32::NAN),
            &Value::Float(f32::NAN)
        ));
        assert!(vh_rmw_same_bits(
            &Value::Double(f64::NAN),
            &Value::Double(f64::NAN)
        ));
        assert!(!vh_rmw_same_bits(&Value::Float(0.0), &Value::Float(-0.0)));
        assert!(vh_rmw_same_bits(&Value::Int(3), &Value::Int(3)));
        assert!(!vh_rmw_same_bits(&Value::Int(0), &Value::Object(None)));
    }
}

#[cfg(test)]
mod mhn_set_call_site_target_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    /// Both setters store into the slot `getTarget` reads (`cs_target_slot`),
    /// and leave the old target replaced -- the retarget a Groovy call site
    /// makes on every cache miss.
    #[test]
    fn set_call_site_target_normal_and_volatile_store_the_new_target() {
        let mut ctx = MockNativeContext::new();
        let site = ctx.alloc_object(ClassId::new(0), 1);
        let old = ctx.alloc_object(ClassId::new(0), 1);
        let new = ctx.alloc_object(ClassId::new(0), 1);
        ctx.set_field(site, 0, Value::Object(Some(old)));

        let args = [Value::Object(Some(site)), Value::Object(Some(new))];
        assert!(matches!(
            native_mhn_set_call_site_target_normal(&mut ctx, &args),
            Ok(None)
        ));
        assert_eq!(cs_read_target(&mut ctx, site), Value::Object(Some(new)));

        let args = [Value::Object(Some(site)), Value::Object(Some(old))];
        assert!(matches!(
            native_mhn_set_call_site_target_volatile(&mut ctx, &args),
            Ok(None)
        ));
        assert_eq!(cs_read_target(&mut ctx, site), Value::Object(Some(old)));
    }

    /// A call site argument that is not an object is the caller's bug, not a
    /// silent no-op.
    #[test]
    fn set_call_site_target_refuses_a_non_object_site() {
        let mut ctx = MockNativeContext::new();
        let args = [Value::Int(0), Value::Object(None)];
        assert!(native_mhn_set_call_site_target_normal(&mut ctx, &args).is_err());
    }
}

pub(crate) fn varhandle_get_and_set_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element getAndSet — args = [vh, array, idx, new_value].
    // Index first, as HotSpot 25 (`R15Vh7ArrayCastOrder` row `gasOob`).
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
        if let Some(refusal) = vh_array_store_refusal(ctx, this, arr, new_val) {
            return Err(refusal);
        }
        // Atomic getAndSet: a CAS loop until it lands (`vh_cas_rmw`;
        // `compare_and_swap_field` runs on array refs). nio21 wave 6 L1: the
        // plain-store fallback after 1024 failures is gone.
        let old = vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |_| new_val);
        return Ok(Some(old));
    }
    // Meta side-table FIRST — real-JDK VarHandles don't carry our synthetic
    // 6-field layout (see `varhandle_get_and_bitwise`).
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    if kind == VH_KIND_STATIC {
        // Static getAndSet — args = [vh, new_value]; returns the old value.
        let new_val = args.get(1).cloned().unwrap_or(Value::Int(0));
        // Judged like an instance store; see `vh_value_refusal`.
        if let Some(m) = meta.as_deref() {
            if let Some(refusal) = vh_value_refusal(ctx, m, new_val) {
                return Err(refusal);
            }
        }
        let mut live = [new_val];
        // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
        let Some((cid, sidx)) = vh_static_slot_of(
            ctx,
            this,
            meta.as_deref(),
            &mut live,
            vh_static_init(mode, meta.as_deref()),
        )?
        else {
            return Ok(Some(Value::Object(None)));
        };
        let [new_val] = live;
        // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, default ON):
        // one hardware CAS on the static cell first, atomic against compiled
        // `putstatic`; the mirror-monitor lock below is the fallback.
        if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |_| Some(new_val)) {
            return Ok(Some(old));
        }
        // LOW-finding fix: atomic static getAndSet — serialize get+set on the
        // per-class mirror monitor (no CAS primitive for static slots).
        let old = vh_with_static_lock(ctx, cid, &[new_val], |ctx, live| {
            let new_val = live[0];
            // JIT round 22 wave 5c, lane sync: the hardware arm again under the
            // lock (a cell typed since the first try), so the lock path and a
            // hardware RMW of the slot cannot both succeed.
            if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |_| Some(new_val)) {
                return old;
            }
            let old = ctx.get_static_field(cid, sidx);
            ctx.set_static_field(cid, sidx, live[0]);
            old
        });
        return Ok(Some(old));
    }
    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));
    // MEASURED on HotSpot 25.0.3+9 before wiring, `probes/ReflectArgTypeSweep.java`:
    // `getAndSet`, `getAndAdd` and `compareAndExchange` all raise
    // ClassCastException on a wrong receiver too, so all three doors get the
    // same check as `set`/`get`/`compareAndSet`. They were left out of the
    // first pass because only the latter three had been measured, and an
    // unmeasured door is where a fix at the wrong level starts.
    if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, Some(new_val)) {
        return Err(refusal);
    }

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        // By name in the field's holder, not the coordinate.
        match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    // Atomic instance getAndSet: a CAS loop until it lands (`vh_cas_rmw`;
    // nio21 wave 6 L1 dropped the plain-store fallback).
    let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |_| new_val);
    Ok(Some(old))
}

/// VarHandle.getAndAdd(receiver, delta) → old value.
///
/// Signature-polymorphic for both array and instance-field VarHandles.
/// - Array kind: args = [vh, array, index, delta] (H2's `VarHandleInts$Array.getAndAdd([III)I`).
/// - Instance kind: args = [vh, receiver, delta].
/// - Static kind: args = [vh, delta].
///
/// Delta can be Int, Long, Float, or Double. Returns the previous value BOXED
/// through [`vh_box_access_result`], which the erased `Object` call site
/// requires; `unbox_poly_return` then unwraps it for a primitive call site such
/// as H2's `([III)I`.
/// `CRATONVM_VH_READ_ONLY_HANDLE_UOE=0` — restore the pre-2026-09-02 behaviour,
/// in which a `VarHandle` over a FINAL field performed the write.
///
/// Default on, and a third switch beside `CRATONVM_VH_NULL_COORDINATE_NPE` and
/// `CRATONVM_VH_UNSUPPORTED_MODE_UOE` rather than a shared one, for the reason
/// those two are separate: the rules interact, and a shared switch could not
/// isolate any of them. This is the one of the three that can change a
/// PASSING workload, because code that writes a final field succeeds today.
pub(crate) fn vh_read_only_handle_uoe_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_VH_READ_ONLY_HANDLE_UOE").as_deref(),
        Ok("0")
    )
}

/// JVMS §4.5 `ACC_FINAL`.
pub(crate) const ACC_FINAL: u16 = 0x0010;

/// Is `field_name` declared `final` on `class_id` or an ancestor?
///
/// Walks the superclass chain exactly as `lookup_require_field` does, and for
/// the same reason: the field a handle names may be inherited. Called ONCE per
/// handle, at `findVarHandle` time, never on an access.
pub(crate) fn vh_field_is_final(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    field_name: &str,
) -> bool {
    let mut cid = class_id;
    loop {
        for f in ctx.declared_fields(cid) {
            if f.name == field_name {
                return f.access_flags & ACC_FINAL != 0;
            }
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return false,
        }
    }
}

/// Mark an already-built handle's metadata read-only.
///
/// Separate from `alloc_instance_var_handle` rather than a parameter on it:
/// that helper has several callers which mint handles for variables that are
/// never final (array elements, byte views), and a parameter would put the
/// question to all of them. The two `Lookup.find*VarHandle` natives are the
/// only places a final field can enter.
///
/// Answers `vh`'s current address: under `CRATONVM_VH_REAL_VARFORM` the handle
/// is given its read-only `VarForm` here ([`vh_install_real_varform`]), and
/// building one is a GC point (interpreter round i6 wave 123, lane L4).
pub(crate) fn vh_mark_read_only(ctx: &mut dyn NativeContext, vh: ObjectRef) -> ObjectRef {
    if let Some(meta) = vh_meta_get(ctx, vh) {
        let mut updated = (*meta).clone();
        updated.read_only = true;
        vh_meta_put(ctx, vh, updated);
        return vh_install_real_varform(ctx, vh);
    }
    vh
}

/// `UnsupportedOperationException` for a WRITE mode on a read-only handle.
///
/// # The rule
///
/// `findVarHandle` on a `final` field yields a handle whose write modes are all
/// unsupported. Measured on JDK 25 with `RJdkVarHandleModeSupport`: every one
/// of `set` / `setVolatile` / `setOpaque` / `setRelease`, the five CAS modes,
/// the three compare-and-exchange modes, `getAndSet*`, `getAndAdd*` and the
/// nine `getAndBitwise*` raise it; the four READ modes still answer, and so
/// does a read of a final REFERENCE field and of a `static final`.
///
/// CratonVM performed the write. `VH.set(h, 5)` stored 5 into a `final int` —
/// a silent successful write to a field the language guarantees is immutable,
/// which anything caching that field's value is entitled to assume cannot
/// happen.
///
/// # Where it sits
///
/// FIRST, ahead of both other rules. Measured, not chosen:
/// `VH_FIN.set((H) null, 5)` on a final-field handle raises
/// `UnsupportedOperationException`, not `NullPointerException`, so read-only
/// beats the null coordinate. Read-only against an unsupported MODE is not
/// observable — both raise `UnsupportedOperationException` — so the order
/// between those two is free and this one is placed first for both.
pub(crate) fn vh_check_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    vh_check_read_only_row(ctx, args, mode, &None)
}

/// [`vh_check_read_only`] over the row the prologue read (wave 47,
/// [`VARHANDLE_ONE_ROW_READ`]).
pub(crate) fn vh_check_read_only_row(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
    row: &VhPrologueRow,
) -> Result<(), MethodCallFailed> {
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        return Ok(());
    };
    let read_only = vh_row_or_read(ctx, this, row)
        .as_deref()
        .map(|m| m.read_only)
        .unwrap_or(false);
    // The kill switch is asked only for a read-only handle (wave 47): it is
    // a flag-table lookup by name, which every `set` and `compareAndSet`
    // paid before the row was even read.
    if !read_only || !vh_read_only_handle_uoe_enabled() {
        return Ok(());
    }
    // HotSpot's message is the access mode's method name (`set`), as for
    // every unsupported mode (interpreter round i1 wave 32).
    Err(RuntimeError::UnsupportedOperationException {
        message: mode.to_string(),
    }
    .into())
}

/// `CRATONVM_VH_UNSUPPORTED_MODE_UOE=0` — restore the pre-2026-09-02 behaviour,
/// in which an access mode the variable's type does not admit ANSWERED instead
/// of raising `UnsupportedOperationException`.
///
/// Default on, and separate from `CRATONVM_VH_NULL_COORDINATE_NPE` on purpose:
/// the two rules interact (this one wins, which is HotSpot's order), so a
/// single switch for both could not isolate either. Each turns silence into an
/// exception on a path any workload can reach, and a suite that starts failing
/// has to be bisectable to the RULE rather than to a rebuild.
pub(crate) fn vh_unsupported_mode_uoe_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_VH_UNSUPPORTED_MODE_UOE").as_deref(),
        Ok("0")
    )
}

/// Which family of access mode is being attempted, for
/// [`vh_check_access_mode_supported`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VhModeFamily {
    /// `getAndAdd`, `getAndAddAcquire`, `getAndAddRelease`.
    Arithmetic,
    /// `getAndBitwise{Or,And,Xor}` and their ordering variants.
    Bitwise,
}

/// `UnsupportedOperationException` for an access mode the VARIABLE'S TYPE does
/// not admit.
///
/// # The rule, measured rather than assumed
///
/// It is not uniform, which is why `RJdkVarHandleModeSupport` sweeps every mode
/// against every variable type instead of picking a few. On JDK 25:
///
/// ```text
///   variable      getAndAdd*   getAndBitwise*
///   boolean       UNSUPPORTED  ok
///   byte char short int long   ok           ok
///   float double  ok           UNSUPPORTED
///   any reference UNSUPPORTED  UNSUPPORTED
/// ```
///
/// So arithmetic is the NUMERIC primitives and bitwise is the INTEGRAL ones
/// plus `boolean` — the two sets differ at both ends, and `boolean` versus
/// `float` is the pair that catches a guess. The `Acquire`/`Release` variants
/// follow their base mode exactly (checked for all four of `boolean`, `int`,
/// `float` and a reference).
///
/// # Why it runs BEFORE the null-coordinate check
///
/// Because HotSpot's does. Swept across all ten types: where the mode is
/// unsupported, a null receiver still yields `UnsupportedOperationException`;
/// where it is supported, the null receiver yields `NullPointerException`. The
/// two rules never disagree about which wins, so the order here is the whole
/// of the interaction and `vh_check_leading_coordinate` is called after this.
///
/// # What CratonVM did before
///
/// Answered. 57 of the sweep's 166 rows raise `UnsupportedOperationException`
/// on HotSpot, and CratonVM returned a value for every one — `null` 28 times,
/// a `float` 20 times (a bitwise op on a `float` variable), `false`/`true` 5
/// times, and two silent write successes.
///
/// # Cost
///
/// One `Arc` deref and one byte compare on the ordinary path: the meta is
/// already in the per-thread memo every access mode consults, and the
/// descriptor's FIRST byte is the whole test. No allocation, and no work at
/// all for the read/write/CAS families, which do not call this.
pub(crate) fn vh_check_access_mode_supported(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    family: VhModeFamily,
    mode: &'static str,
) -> Result<(), MethodCallFailed> {
    if !vh_unsupported_mode_uoe_enabled() {
        return Ok(());
    }
    let Some(this) = args.first().and_then(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) else {
        return Ok(());
    };
    // A `SegmentVarHandle` is a real JDK class with its own form; its mode
    // support was not swept, so it keeps today's behaviour.
    if is_segment_var_handle(ctx, this) {
        return Ok(());
    }
    // The VARIABLE's descriptor, cheapest source first. An array-element
    // handle minted by the real JDK has no side-table entry, and there the
    // ELEMENT type is what the rule is about — `real_array_var_handle_descriptors`
    // is the same accessor `vh_value_and_coordinate_descriptors` reaches for.
    let meta = vh_meta_get(ctx, this);
    let first = match meta.as_deref() {
        Some(m) => m.field_desc.as_bytes().first().copied(),
        None => match real_array_var_handle_descriptors(ctx, this) {
            Some((value_desc, _)) => value_desc.as_bytes().first().copied(),
            // `vh_read_string`, NOT `vh_field_desc`. The latter substitutes
            // `DESC_OBJECT` when the handle carries no descriptor at all, so an
            // UNDETERMINABLE descriptor arrives here spelled `L...` and the
            // `let Some(first) = first else { return Ok(()) }` escape two lines
            // below can never fire. The comment on that escape says the
            // intended behaviour is to keep today's rather than "inheriting a
            // refusal nothing measured", and a default is not a measurement.
            //
            // MEASURED: `get_and_add_on_long_array_boxes_long` builds a handle
            // with no metadata and a real `long[]` coordinate, and got
            // `UnsupportedOperationException: getAndAdd is not supported for a
            // VarHandle over a variable of type a reference` -- the fallback
            // describing itself.
            None => {
                vh_read_string(ctx, this, VH_FIELD_DESC).and_then(|s| s.as_bytes().first().copied())
            }
        },
    };
    // An undeterminable descriptor keeps today's behaviour rather than
    // inheriting a refusal nothing measured.
    let Some(first) = first else {
        return Ok(());
    };
    let supported = match family {
        VhModeFamily::Arithmetic => {
            matches!(first, b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
        }
        VhModeFamily::Bitwise => {
            matches!(first, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J')
        }
    };
    if supported {
        return Ok(());
    }
    // HotSpot's message is the access mode's method name (`getAndAdd`),
    // as for every unsupported mode (interpreter round i1 wave 32).
    Err(RuntimeError::UnsupportedOperationException {
        message: mode.to_string(),
    }
    .into())
}

pub(crate) fn varhandle_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_add_mode(ctx, args, "getAndAdd")
}

/// [`varhandle_get_and_add`] for `mode` (and its `Acquire` / `Release`
/// variants).
pub(crate) fn varhandle_get_and_add_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    vh_check_access_mode_supported(ctx, args, VhModeFamily::Arithmetic, mode)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    // The delta is adapted like any stored value: `getAndAdd(h, 1)` on a
    // `float` field reached `add_values(Float, Int)`, which has no arm for
    // the pair and returned the current value unchanged -- a lost add.
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::GetAndAdd, mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::GetAndAdd, mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::GetAndAdd)
    } else {
        varhandle_get_and_add_raw(ctx, args, &row, mode)
    };
    vh_box_access_result(ctx, args, raw)
}

pub(crate) fn varhandle_get_and_add_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Interpreter round i5 wave 102, lane L4: the sum is narrowed to the
    // variable's own type, as `Unsafe.getAndAddByte` / `Short` / `Char` store
    // `(byte) (v + delta)`: a `byte` 127 plus 1 is -128, a `char` 65535 plus 1
    // is 0. The plain `int` sum left 128 / 65536 in a `byte` / `char` static or
    // `Value`-cell field (probe `L4W102VarHandleSubwordGetAndAdd`).
    fn add_values_narrowed(current: &Value, delta: &Value, var_desc: &str) -> Value {
        match (add_values(current, delta), var_desc.as_bytes().first()) {
            (Value::Int(v), Some(b'B')) => Value::Int(i32::from(v as i8)),
            (Value::Int(v), Some(b'S')) => Value::Int(i32::from(v as i16)),
            (Value::Int(v), Some(b'C')) => Value::Int(i32::from(v as u16)),
            (sum, _) => sum,
        }
    }

    fn add_values(current: &Value, delta: &Value) -> Value {
        match (current, delta) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
            (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Double(a), Value::Double(b)) => Value::Double(a + b),
            // Mixed: widen delta to current's type.
            (Value::Long(a), Value::Int(b)) => Value::Long(a.wrapping_add(*b as i64)),
            (Value::Int(a), Value::Long(b)) => Value::Int((*a as i64).wrapping_add(*b) as i32),
            _ => current.clone(),
        }
    }

    // LOW-finding fix: atomic getAndAdd on an array element via a bounded CAS
    // retry loop (was a non-atomic get/add/set → lost updates under
    // contention). The add is recomputed from the freshly-read `old` each
    // iteration so a racing update is never clobbered.
    // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`); the plain-store
    // fallback after 1024 failures lost updates.
    fn array_get_and_add(
        ctx: &mut dyn NativeContext,
        arr: ObjectRef,
        idx: usize,
        delta: &Value,
    ) -> Value {
        let elem_desc = array_element_desc(&*ctx, arr);
        vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |old| {
            add_values_narrowed(old, delta, elem_desc)
        })
    }

    // C38: Array-element getAndAdd — args = [vh, array, idx, delta]. Route first
    // so real-JDK VarHandleLongs$Array / VarHandleInts$Array calls work even
    // when the VH's slot 0 is not our synthetic VH_KIND Int.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
        return Ok(Some(array_get_and_add(ctx, arr, idx, &delta)));
    }

    // Resolve kind + field via the meta side-table FIRST (real-JDK VarHandles —
    // e.g. `java.net.Socket.STATE` — do NOT carry our synthetic 6-field layout,
    // so reading `VH_KIND` off the object yields garbage and the update silently
    // no-ops returning 0). Fall back to the synthetic fields. Mirrors
    // `varhandle_get_and_bitwise` / `varhandle_compare_and_set`.
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_ARRAY => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Int(0)));
            };
            let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
            // LOW-finding fix: atomic via the shared CAS retry helper.
            Ok(Some(array_get_and_add(ctx, arr, idx, &delta)))
        }
        VH_KIND_INSTANCE => {
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Receiver only: the delta is numeric, so there is no reference to
            // judge and `vh_instance_refusal` is passed `None` for the value.
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let delta = args.get(2).cloned().unwrap_or(Value::Int(0));
            let idx = if field_idx >= 0 {
                field_idx as usize
            } else {
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(i) => {
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                        i
                    }
                    None => return Ok(Some(Value::Int(0))),
                }
            };
            // LOW-finding fix: atomic instance getAndAdd via a bounded CAS
            // retry loop (was a non-atomic get/add/set → lost updates under
            // contention). The sum is recomputed from the freshly-read `old`
            // each iteration so a racing update is never clobbered.
            // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
            // t1 wave 9, lane ATOM2 (page t1-rlatom-vh-getandadd-without-a-row-
            // keeps-the-sub-word-carry): a row-less handle narrows by its own
            // `VH_FIELD_DESC` slot (`vh_field_desc`), not by `""`.
            let var_desc_owned: Cow<'_, str> = match meta.as_deref() {
                Some(m) => Cow::Borrowed(m.field_desc.as_str()),
                None => vh_field_desc(ctx, this),
            };
            let var_desc: &str = &var_desc_owned;
            let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |old| {
                add_values_narrowed(old, &delta, var_desc)
            });
            Ok(Some(old))
        }
        VH_KIND_STATIC => {
            let delta = args.get(1).cloned().unwrap_or(Value::Int(0));
            // `delta` is numeric: nothing to keep alive across `<clinit>`.
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                // LOW-finding fix: atomic static getAndAdd — serialize the
                // get/add/set on the per-class mirror monitor (no CAS
                // primitive exists for static slots).
                // t1 wave 9, lane ATOM2: a row-less handle narrows by its
                // `VH_FIELD_DESC` slot (see the instance arm).
                let var_desc_owned: Cow<'_, str> = match meta.as_deref() {
                    Some(m) => Cow::Borrowed(m.field_desc.as_str()),
                    None => vh_field_desc(ctx, this),
                };
                let var_desc: &str = &var_desc_owned;
                // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`): a
                // hardware CAS loop on the static cell first.
                if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |old| {
                    Some(add_values_narrowed(&old, &delta, var_desc))
                }) {
                    return Ok(Some(old));
                }
                let old = vh_with_static_lock(ctx, cid, &[], |ctx, _| {
                    // JIT round 22 wave 5c, lane sync: the hardware arm again
                    // under the lock (see the CAS arm).
                    if let Some((old, _)) = ctx.static_field_hw_update(cid, sidx, &mut |old| {
                        Some(add_values_narrowed(&old, &delta, var_desc))
                    }) {
                        return old;
                    }
                    let old = ctx.get_static_field(cid, sidx);
                    let new_val = add_values_narrowed(&old, &delta, var_desc);
                    ctx.set_static_field(cid, sidx, new_val);
                    old
                });
                Ok(Some(old))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

/// Bitwise atomic op selector for `VarHandle.getAndBitwise{Or,And,Xor}`.
#[derive(Copy, Clone)]
pub(crate) enum VhBitOp {
    Or,
    And,
    Xor,
}

/// `VarHandle.getAndBitwise{Or,And,Xor}(receiver, mask) -> old value`.
///
/// Signature-polymorphic, same arg shapes as [`varhandle_get_and_add`]
/// (array / instance / static). Needed by `java.net.Socket`, whose `state`
/// field is updated with `STATE.getAndBitwiseOr(this, flag)` during
/// connect/accept; without it the real-JDK Socket path throws
/// `NoSuchMethodError`. See `reference_server_socket_gap`.
///
/// Boxes through [`vh_box_access_result`] — see there for why the erased
/// `Object` call site needs it and why the boxing cannot live at the
/// poly-return boundary.
pub(crate) fn varhandle_get_and_bitwise(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhBitOp,
    mode: &'static str,
) -> MethodCallResult {
    let param_site = cratonvm_native_api::poly_call_site::take_vh_param_site();
    // Cast: the low half is the value-site cell (round 15 wave 3, VH1-3).
    let site = param_site as u16;
    // The handle's row, read once (wave 47, `VARHANDLE_ONE_ROW_READ`).
    let row = vh_prologue_row(ctx, args);
    if let Some(served) = vh_indirect_serve_site(ctx, args, mode, param_site, &row) {
        return served;
    }
    vh_check_read_only_row(ctx, args, mode, &row)?;
    vh_check_access_mode_supported(ctx, args, VhModeFamily::Bitwise, mode)?;
    // Wave 57 (lane L4): `asType`'s return direction, before the access.
    vh_return_site_check(ctx, args, param_site, &row)?;
    // Round 15 wave 3 (lane vh4; VH3-2): see `varhandle_get_mode`.
    vh_check_value_site_row(ctx, args, 1, site, &row)?;
    let coordinate = vh_index_coordinate_args(ctx, args, 1, param_site, mode)?;
    let args: &[Value] = &coordinate;
    // Round 15 wave 4 (lane vh5; VH4-1): see `varhandle_get_mode`.
    vh_array_coordinate_cast(ctx, args, 1, mode)?;
    let kind = vh_check_arity_row(ctx, args, 4, mode, &row)?;
    vh_check_leading_coordinate_row(ctx, args, &row, mode)?;
    let this = obj_arg(args, 0)?;
    let adapted = vh_adapt_value_args_row(ctx, this, args, 1, site, &row)?;
    let args: &[Value] = &adapted;
    if let Some(done) = segment_vh_rmw_unless_row(ctx, args, &row, VhViewRmw::Bitwise(op), mode) {
        return done;
    }
    // An FFM layout handle (wave 38): see `layout_vh_rmw`.
    if let Some(shape) = vh_layout_shape_unless_field(ctx, obj_arg(args, 0)?, &row) {
        return layout_vh_rmw(ctx, shape, args, VhViewRmw::Bitwise(op), mode);
    }
    let raw = if is_byte_buffer_view_kind(kind) {
        byte_buffer_view_rmw(ctx, args, VhViewRmw::Bitwise(op))
    } else {
        varhandle_get_and_bitwise_raw(ctx, args, op, &row, mode)
    };
    vh_box_access_result(ctx, args, raw)
}

pub(crate) fn varhandle_get_and_bitwise_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: VhBitOp,
    row: &VhPrologueRow,
    mode: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    fn apply(current: &Value, mask: &Value, op: VhBitOp) -> Value {
        let f = |a: i64, b: i64| match op {
            VhBitOp::Or => a | b,
            VhBitOp::And => a & b,
            VhBitOp::Xor => a ^ b,
        };
        match (current, mask) {
            (Value::Int(a), Value::Int(b)) => Value::Int(f(*a as i64, *b as i64) as i32),
            (Value::Long(a), Value::Long(b)) => Value::Long(f(*a, *b)),
            (Value::Long(a), Value::Int(b)) => Value::Long(f(*a, *b as i64)),
            (Value::Int(a), Value::Long(b)) => Value::Int(f(*a as i64, *b) as i32),
            _ => current.clone(),
        }
    }

    // LOW-finding fix: atomic getAndBitwise* on an array element via a bounded
    // CAS retry loop (was a non-atomic get/apply/set → lost updates under
    // contention). Same shape as the getAndAdd array helper; the masked value
    // is recomputed from the freshly-read `old` each iteration.
    fn array_get_and_bitwise(
        ctx: &mut dyn NativeContext,
        arr: ObjectRef,
        idx: usize,
        mask: &Value,
        op: VhBitOp,
    ) -> Value {
        // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
        vh_cas_rmw(ctx, arr, idx, VhRmwSlot::ArrayElement, |old| {
            apply(old, mask, op)
        })
    }

    // Array-element form — args = [vh, array, idx, mask].
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let idx = vh_array_index_raising(ctx, this, mode, arr, idx)?;
        let mask = args.get(3).cloned().unwrap_or(Value::Int(0));
        return Ok(Some(array_get_and_bitwise(ctx, arr, idx, &mask, op)));
    }

    // Resolve kind + field via the meta side-table FIRST (real-JDK VarHandles —
    // e.g. `java.net.Socket.STATE` — do NOT carry our synthetic 6-field layout,
    // so reading `VH_KIND` off the object yields garbage). Fall back to the
    // synthetic fields. Mirrors `varhandle_compare_and_set`.
    let meta = vh_row_or_read(ctx, this, row);
    let (kind, field_idx) = match meta.as_deref() {
        Some(m) => (m.kind, m.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) {
                Value::Int(k) => k,
                _ => return Ok(Some(Value::Int(0))),
            };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            (k, i)
        }
    };

    match kind {
        VH_KIND_ARRAY => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Round 15 wave 3 (lane vh4): checked, and only for an array.
            let array_class = meta.as_deref().map(|m| m.class_name.as_str());
            let Some(idx) = vh_kind_array_fallback_index(ctx, arr, array_class, args)? else {
                return Ok(Some(Value::Int(0)));
            };
            let mask = args.get(3).cloned().unwrap_or(Value::Int(0));
            // LOW-finding fix: atomic via the shared CAS retry helper.
            Ok(Some(array_get_and_bitwise(ctx, arr, idx, &mask, op)))
        }
        VH_KIND_INSTANCE => {
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Receiver only -- the mask is numeric. See `getAndAdd` above.
            if let Some(refusal) = vh_instance_refusal(ctx, meta.as_deref(), receiver, None) {
                return Err(refusal);
            }
            let mask = args.get(2).cloned().unwrap_or(Value::Int(0));
            let idx = if field_idx >= 0 {
                field_idx as usize
            } else {
                // By name in the field's holder, not the coordinate.
                match vh_instance_field_by_name(ctx, this, meta.as_deref()) {
                    Some(i) => {
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                        i
                    }
                    None => return Ok(Some(Value::Int(0))),
                }
            };
            // LOW-finding fix: atomic instance getAndBitwise* via a bounded CAS
            // retry loop (was a non-atomic get/apply/set → lost updates under
            // contention). The masked value is recomputed from the
            // freshly-read `old` each iteration so a racing update is kept.
            // nio21 wave 6 L1: until the CAS lands (`vh_cas_rmw`).
            let old = vh_cas_rmw(ctx, receiver, idx, VhRmwSlot::Field, |old| {
                apply(old, &mask, op)
            });
            Ok(Some(old))
        }
        VH_KIND_STATIC => {
            let mask = args.get(1).cloned().unwrap_or(Value::Int(0));
            // `mask` is integral: nothing to keep alive across `<clinit>`.
            // j27 wave 1 (lane FFMVH): through the per-handle slot memo.
            if let Some((cid, sidx)) = vh_static_slot_of(
                ctx,
                this,
                meta.as_deref(),
                &mut [],
                vh_static_init(mode, meta.as_deref()),
            )? {
                // JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`): a
                // hardware CAS loop on the static cell first.
                if let Some((old, _)) =
                    ctx.static_field_hw_update(cid, sidx, &mut |old| Some(apply(&old, &mask, op)))
                {
                    return Ok(Some(old));
                }
                // LOW-finding fix: atomic static getAndBitwise* — serialize the
                // get/apply/set on the per-class mirror monitor (no CAS
                // primitive exists for static slots).
                let old = vh_with_static_lock(ctx, cid, &[], |ctx, _| {
                    // JIT round 22 wave 5c, lane sync: the hardware arm again
                    // under the lock (see the CAS arm).
                    if let Some((old, _)) =
                        ctx.static_field_hw_update(cid, sidx, &mut |old| Some(apply(&old, &mask, op)))
                    {
                        return old;
                    }
                    let old = ctx.get_static_field(cid, sidx);
                    let new_val = apply(&old, &mask, op);
                    ctx.set_static_field(cid, sidx, new_val);
                    old
                });
                Ok(Some(old))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn varhandle_get_and_bitwise_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::Or, "getAndBitwiseOr")
}
pub(crate) fn varhandle_get_and_bitwise_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::And, "getAndBitwiseAnd")
}
pub(crate) fn varhandle_get_and_bitwise_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    varhandle_get_and_bitwise(ctx, args, VhBitOp::Xor, "getAndBitwiseXor")
}

