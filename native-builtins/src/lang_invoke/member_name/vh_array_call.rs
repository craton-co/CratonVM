#[allow(unused_imports)]
use super::*;


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
pub const DESC_VOID: &str = "V";
pub const DESC_INT: &str = "I";
pub const DESC_LONG: &str = "J";
pub const DESC_FLOAT: &str = "F";
pub const DESC_DOUBLE: &str = "D";
pub const DESC_BOOLEAN: &str = "Z";
pub const DESC_BYTE: &str = "B";
pub const DESC_CHAR: &str = "C";
pub const DESC_SHORT: &str = "S";
pub const DESC_REF: &str = "L";
pub const DESC_OBJECT: &str = "Ljava/lang/Object;";
pub const DESC_MEMORY_SEGMENT: &str = "Ljava/lang/foreign/MemorySegment;";

pub const NAME_VOID: &str = "void";
pub const NAME_INT: &str = "int";
pub const NAME_LONG: &str = "long";
pub const NAME_FLOAT: &str = "float";
pub const NAME_DOUBLE: &str = "double";
pub const NAME_BOOLEAN: &str = "boolean";
pub const NAME_BYTE: &str = "byte";
pub const NAME_CHAR: &str = "char";
pub const NAME_SHORT: &str = "short";

pub const DESC_DEFAULT_METHOD: &str = "()V";
pub const DESC_DEFAULT_OBJECT_RETURN: &str = "()Ljava/lang/Object;";
pub const NAME_INVOKE: &str = "invoke";

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
pub fn vh_array_call(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<(ObjectRef, i32)> {
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
pub fn vh_array_index(
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
pub fn vh_index_body_descriptor(mode: &str, t: &str) -> Option<(String, usize)> {
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
pub fn vh_index_raise_through_jdk(
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
pub fn vh_array_index_raising(
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
pub fn vh_index_coordinate_kinds_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_VH_VIEW_VALUE_ADAPT` (default on, both modes; round 15 wave 3,
/// lane vh4): a byte-array or `ByteBuffer` view's VALUE parameters are
/// converted to the view's primitive as `asType` converts them
/// (`vh_primitive_var_desc`, then `vh_adapt_value_args`), as a field's or an
/// array element's already are. `0` hands the raw value to the byte encoder
/// again (its bits: `5` into a `float` view stored a denormal, a boxed value
/// stored `0`).
pub fn vh_view_value_adapt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Is `this` a handle whose SECOND coordinate is an `int` index: the VM's own
/// array-element, byte-array-view and `ByteBuffer`-view handles, and a real-JDK
/// `VarHandle*$Array` (no side-table row)? A view whose access mode `mode`
/// refuses (`UnsupportedOperationException`, which HotSpot raises from
/// `getMethodHandle` BEFORE `asType`) answers `false`, so the refusal keeps
/// its order. FFM layout and segment handles (`(MemorySegment, long)`) are
/// never index handles: a `long` there is their real coordinate.
pub fn vh_has_index_coordinate(
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
pub fn vh_index_coordinate_args<'a>(
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
pub fn vh_kind_array_fallback_index(
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
pub fn vh_array_coordinate_cast_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

thread_local! {
    /// Per-thread memo of [`vh_real_array_coordinate_desc`], on the terms of
    /// `VH_META_MEMO` ([`vh_memo_lookup`]): a real-JDK `VarHandle*$Array` has
    /// no side-table row, and its array type is a class-name read (plus two
    /// field reads for `References$Array`) that no access should pay.
    pub static VH_ARRAY_COORD_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<str>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );

    /// `(vm, class id)` of the last `ByteBuffer`-view coordinate class
    /// [`vh_byte_buffer_coordinate_admits`] admitted. Positive answers only:
    /// every `ByteBuffer` subclass is a boot class (package-private
    /// constructors), so an admitted id never changes meaning.
    pub static VH_BYTE_BUFFER_COORD_OK: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// The array coordinate type (`[I`, `[Ljava/lang/String;`) of a REAL-JDK
/// array-element handle (`real_array_var_handle_descriptors`'s first
/// coordinate), memoised per thread; `None` for every other handle.
pub fn vh_real_array_coordinate_desc(ctx: &dyn NativeContext, vh: ObjectRef) -> Option<Arc<str>> {
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
pub fn vh_array_coordinate_admits(
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

pub const DESC_OBJECT_ARRAY: &str = "[Ljava/lang/Object;";

/// Is `coord` a `java.nio.ByteBuffer` (the coordinate type of a `ByteBuffer`
/// view)? [`reference_arg_admitted`]'s answer, with the last admitted class
/// remembered per thread so a view's steady state pays one compare.
pub fn vh_byte_buffer_coordinate_admits(ctx: &mut dyn NativeContext, coord: ObjectRef) -> Option<bool> {
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
pub fn vh_array_coordinate_cast(
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
pub fn vh_cast_checkcast_message_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
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
pub fn vh_coordinate_cast_is_checkcast(target_desc: &str) -> bool {
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
pub fn vh_checkcast_message(from: &str, target_desc: &str) -> String {
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
pub fn vh_coordinate_cast_failure(
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


/// Read the VH_FIELD_DESC string from a VarHandle object. Prefers the
/// WP4.2 side table (see vh_meta_table comment) and falls back to the
/// raw object slot for VarHandles allocated outside our path.
pub fn vh_field_desc(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Cow<'static, str> {
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
pub fn vh_type_desc(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Cow<'static, str> {
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
pub fn vh_type_desc_from_meta(meta: &VarHandleMeta) -> Cow<'static, str> {
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
pub const VH_KIND: usize = 0; // i32: 0=instanceField, 1=staticField, 2=array
/// Target class name (JVM internal, e.g. "java/lang/Foo")
pub const VH_CLASS: usize = 1; // Object(String)
/// Field name
pub const VH_FIELD: usize = 2; // Object(String)
/// Field descriptor (e.g. "I", "J", "Ljava/lang/String;")
pub const VH_FIELD_DESC: usize = 3; // Object(String)
/// Resolved field index (slot in the object's field array)
pub const VH_FIELD_INDEX: usize = 4; // Int
/// Cached ClassId of the declaring class
pub const VH_CLASS_ID: usize = 5; // Int (ClassId raw)
pub const VH_FIELD_COUNT: usize = 6;

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
pub const VH_META_VAR_TYPE: usize = 6; // Object(Class)
/// Slot 7 (`reflect_invoke.rs` only): the `Class` mirror of the handle's
/// LEADING coordinate; `null` for a static-field handle, which has none.
pub const VH_META_COORD0: usize = 7; // Object(Class)
/// Allocation size for a VarHandle that carries the two slots above.
/// (`reflect_invoke.rs` re-publishes this as its `VH_META_NUM_FIELDS`; the
/// name differs here only so the two do not collide in `lib.rs`, which
/// glob-imports `lang_invoke::*`.)
pub const VH_META_SLOT_COUNT: usize = 8;

pub const VH_KIND_INSTANCE: i32 = 0;
pub const VH_KIND_STATIC: i32 = 1;
pub const VH_KIND_ARRAY: i32 = 2;
// Byte-array-view VarHandle (`MethodHandles.byteArrayViewVarHandle(<T>[].class,
// order)`): views a `byte[]` as a wider primitive at a BYTE index, with the
// element descriptor carried in `field_desc` ("J"/"I"/"S"/"C"/"D"/"F"). The
// little/big-endian variants are distinct kinds so the byte order is encoded
// without growing `VarHandleMeta`. See `byte_view_{get,set}`.
pub const VH_KIND_BYTE_VIEW_LE: i32 = 3;
pub const VH_KIND_BYTE_VIEW_BE: i32 = 4;
// ByteBuffer-view VarHandle (`MethodHandles.byteBufferViewVarHandle`). Same
// element/endianness metadata as byte-array views, but coordinates are
// `(ByteBuffer, byteIndex)` and direct buffers must hit native memory.
pub const VH_KIND_BYTE_BUFFER_VIEW_LE: i32 = 5;
pub const VH_KIND_BYTE_BUFFER_VIEW_BE: i32 = 6;
// An FFM LAYOUT VarHandle (`ValueLayout.JAVA_INT.varHandle()`,
// `layout.varHandle(PathElement...)`), whose coordinates are
// `(MemorySegment, long[, long])`. Reported by the describe-yourself pair;
// this kind is never written into a slot, because the handle's meaning lives
// in `P67_MEMORY_SEGMENT_VH_TABLE` (see `SegmentVhShape`). Deliberately NOT
// `3`: `phases_late/reflect_invoke.rs` numbers ITS memory-segment kind 3 under
// a different slot map, and 3 already means `VH_KIND_BYTE_VIEW_LE` here.
pub const VH_KIND_MEMORY_SEGMENT_LAYOUT: i32 = 7;

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
pub struct VarHandleMeta {
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
    /// (`withInvokeExactBehavior()`, `--real-jdk`, interpreter round i1 wave
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
pub struct VhMetaRow {
    pub vm: usize,
    pub meta: Arc<VarHandleMeta>,
}

/// The VarHandle meta rows, keyed by the handle's weak lock key.
#[derive(Default)]
pub struct VhMetaTable {
    pub rows: rustc_hash::FxHashMap<usize, VhMetaRow>,
}

impl VhMetaTable {
    /// The row of the handle whose weak lock key is `key`.
    pub fn get(&self, key: usize) -> Option<&Arc<VarHandleMeta>> {
        self.rows.get(&key).map(|row| &row.meta)
    }

    /// File (or refile) the row of the handle keyed `key`. A key names one
    /// object of one VM, so a refile keeps its VM.
    pub fn insert(&mut self, vm: usize, key: usize, meta: Arc<VarHandleMeta>) {
        match self.rows.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().meta = meta,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(VhMetaRow { vm, meta });
            }
        }
    }

    /// Drop the row keyed `key` (its handle died, or a test cleans up).
    pub fn remove(&mut self, key: usize) -> Option<Arc<VarHandleMeta>> {
        self.rows.remove(&key).map(|row| row.meta)
    }

    /// Drop every row `vm` filed; answers how many went.
    pub fn forget_vm(&mut self, vm: usize) -> usize {
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
pub fn existing_weak_lock_key_at(vm: usize, identity_hash: i32, addr: usize) -> Option<usize> {
    let hash = identity_hash as u32;
    let reg = crate::lock_key_shards::lock_key_shard(hash).lock();
    reg.buckets
        .get(&hash)?
        .iter()
        .find(|s| s.vm == vm && s.last_ptr == addr)
        .map(|slot| crate::pack_lock_key(hash, slot.generation))
}

pub static VH_META_TABLE: std::sync::OnceLock<parking_lot::Mutex<VhMetaTable>> =
    std::sync::OnceLock::new();

pub fn vh_meta_table() -> &'static parking_lot::Mutex<VhMetaTable> {
    VH_META_TABLE.get_or_init(|| parking_lot::Mutex::new(VhMetaTable::default()))
}

pub fn vh_meta_put(ctx: &mut dyn NativeContext, vh: ObjectRef, meta: VarHandleMeta) {
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
pub fn vh_lock_key(ctx: &dyn NativeContext, vh: ObjectRef) -> usize {
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
pub fn vh_memo_lookup<T: Clone + 'static>(
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

pub fn vh_meta_get(
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
pub fn vh_real_adoption_enabled() -> bool {
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
pub fn vh_real_adoption_gate() -> bool {
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
pub fn vh_adopt_exact_flag(
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
pub fn vh_adopt_real_field_handle(
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
    // `--real-jdk`): the proxy is no class-store class, so it has no name
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
        // `--real-jdk`). A static's holder is its declaring class already.
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
pub fn vh_adopt_real_byte_view(
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
pub fn forget_var_handle_meta_keys(keys: &[usize]) {
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
pub fn note_objects_moved() {
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
pub fn vh_ref_type_lookup_name(field_desc: &str) -> Option<&str> {
    let name = field_desc.strip_prefix('L')?.strip_suffix(';')?;
    (name != "java/lang/Object").then_some(name)
}

/// The component class name of a ONE-dimensional reference array field
/// descriptor (`[Ljava/lang/String;` -> `java/lang/String`), `None` for any
/// other descriptor. `java/lang/Object` is kept: an exact `Object[]` value is
/// still exactly the field's type.
pub fn vh_ref_array_component_lookup_name(field_desc: &str) -> Option<&str> {
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
pub fn varhandle_instance_field_plan_uncached(lock_key: usize) -> Option<VarHandleInstanceFieldPlan> {
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
    pub static VH_BYTE_VIEW_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<VarHandleByteArrayViewPlan>>> =
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
    pub static VH_BYTE_BUFFER_VIEW_PLAN_MEMO: std::cell::RefCell<Vec<VhMemoLine<VarHandleByteBufferViewPlan>>> =
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
pub static VH_META_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Version of the existing weak VarHandle metadata proofs. JIT primitive-read
/// caches pair this with VM identity, live handle address and the heap epoch;
/// a changed table, freed key or moved handle invalidates the complete proof.
#[inline]
pub fn varhandle_metadata_generation() -> u64 {
    VH_META_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Publish that [`vh_meta_table`] has changed.
pub fn vh_meta_bump_generation() {
    VH_META_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// One direct-mapped thread-local memo line for [`vh_meta_get`] /
/// [`p67_segment_vh_shape`] (see [`vh_memo_lookup`]).
///
/// `vm` is part of the key: one thread can run natives for two VMs (the unit
/// test binaries do), and the hash alone does not name a row any more. `addr`
/// too, since gc-common w29-b: two live handles of one VM can share a hash.
#[derive(Clone)]
pub struct VhMemoLine<T> {
    pub vm: usize,
    pub key: i32,
    pub addr: usize,
    pub generation: u64,
    pub value: Option<T>,
}

impl<T> VhMemoLine<T> {
    /// A line no lookup matches (`generation` is never `u64::MAX`).
    pub const fn empty() -> Self {
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
    pub static VH_META_MEMO: std::cell::RefCell<Vec<VhMemoLine<Arc<VarHandleMeta>>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );

    /// Per-thread memo of [`p67_segment_vh_shape`], on the same terms as
    /// [`VH_META_MEMO`] (gc-common w29-b: the lookup now also takes the
    /// lock-key registry's mutex, which this keeps off the steady state of an
    /// FFM layout handle's `get` / `set`).
    pub static P67_SHAPE_MEMO: std::cell::RefCell<Vec<VhMemoLine<SegmentVhShape>>> =
        std::cell::RefCell::new(
            (0..VH_PLAN_MEMO_SLOTS).map(|_| VhMemoLine::empty()).collect(),
        );
}

/// One direct-mapped thread-local line of the JIT plan memo
/// ([`varhandle_instance_field_plan_for_handle`]): the handle it answers for
/// (VM, identity hash and ADDRESS), the generation it was computed in, and the
/// answer.
#[derive(Clone, Copy)]
pub struct VhPlanMemoLine {
    pub vm: usize,
    pub key: i32,
    pub addr: usize,
    pub generation: u64,
    pub plan: Option<VarHandleInstanceFieldPlan>,
}

/// Slots in the per-thread memo. A power of two so the index is a mask, and
/// small enough to stay in L1 — a thread touches a handful of distinct handles
/// in practice (`CompletableFuture` uses three).
pub const VH_PLAN_MEMO_SLOTS: usize = 64;

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
    pub static VH_PLAN_MEMO: std::cell::RefCell<[VhPlanMemoLine; VH_PLAN_MEMO_SLOTS]> =
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

pub fn vh_meta_update_field_index(ctx: &mut dyn NativeContext, vh: ObjectRef, idx: i32) {
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
pub struct SegmentVhShape {
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
pub static P67_MEMORY_SEGMENT_VH_TABLE: std::sync::OnceLock<
    parking_lot::Mutex<rustc_hash::FxHashMap<usize, (usize, SegmentVhShape)>>,
> = std::sync::OnceLock::new();

pub fn p67_memory_segment_vh_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<usize, (usize, SegmentVhShape)>> {
    P67_MEMORY_SEGMENT_VH_TABLE
        .get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

pub fn register_p67_memory_segment_var_handle(
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

pub fn p67_segment_vh_shape(
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

pub fn p67_memory_segment_var_handle_width(
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
pub const FFM_ADDRESS_CARRIER: u8 = b'a';

/// The JVM descriptor of an FFM layout handle's carrier.
pub fn layout_vh_carrier_desc(carrier: u8) -> &'static str {
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
pub fn layout_vh_coordinates(shape: SegmentVhShape) -> Vec<String> {
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
pub enum LayoutVhAddr {
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
pub fn layout_vh_index_refusal(shape: SegmentVhShape, args: &[Value]) -> Option<MethodCallFailed> {
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
pub fn layout_vh_access(
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
pub fn layout_vh_locate(
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
pub fn layout_vh_decode(shape: SegmentVhShape, raw: u64) -> Value {
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
pub fn layout_vh_encode(shape: SegmentVhShape, value: Value) -> u64 {
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
pub fn layout_vh_raw_native(shape: SegmentVhShape, addr: usize) -> u64 {
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
pub fn layout_vh_read_native(shape: SegmentVhShape, addr: usize) -> Value {
    layout_vh_decode(shape, layout_vh_raw_native(shape, addr))
}

/// Write an FFM layout handle's variable into a native (off-heap) segment.
pub fn layout_vh_write_native(shape: SegmentVhShape, addr: usize, value: Value) {
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
pub fn layout_vh_read_heap(
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
pub fn layout_vh_write_heap(
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
pub fn layout_vh_get(
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
pub fn layout_vh_read_only_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_FFM_VH_ADDRESS_HEAP_REFUSAL` (default on; round 14 wave 1, lane
/// ffm): an `ADDRESS` layout VarHandle write of a heap segment is
/// `IllegalArgumentException: Heap segment not allowed`, as JDK 25's
/// `SharedUtils.unboxSegment` makes it. `0` restores the silent C null. j27
/// wave 4 (lane FFMMSG): the message is HotSpot's, the segment's own
/// `toString()` (`panama::heap_segment_not_allowed`); it named the carrier
/// class.
pub fn layout_vh_address_heap_refusal() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `VarHandle.set` for an FFM layout handle.
pub fn layout_vh_set(
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
        value = Value::Long(ptr);    }
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
pub enum LayoutVhCell {
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
pub fn layout_vh_heap_cell(
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
    pub fn load(self, ctx: &dyn NativeContext, width: usize) -> u64 {
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
    pub fn cas(
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
pub fn layout_vh_rmw(
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
pub fn segment_vh_rmw(
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
pub fn segment_vh_rmw_access(
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
pub fn segment_vh_rmw_unless_row(
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
pub fn class_name_to_descriptor(name: &str) -> Cow<'static, str> {
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
pub fn descriptor_to_class_name(desc: &str) -> Cow<'static, str> {
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
pub fn mirror_to_descriptor(
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
pub fn unnameable_mirror_descriptor(
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
pub fn lambda_proxy_descriptor_class(ctx: &dyn NativeContext, name: &str) -> Option<ClassId> {
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
/// `pub` since 2026-08-13: `phases_late`'s Quarkus logging mirror needs
/// the same split to fill an argument vector from a descriptor it reads off
/// the class rather than one it writes down, and a second copy of this walk is
/// a second place for the `[`/`L…;`/primitive rules to drift.
pub fn split_descriptor_params(desc: &str) -> Option<(Vec<String>, String)> {
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
pub fn wrapper_desc_for_primitive(prim: &str) -> Option<&'static str> {
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
pub fn primitive_desc_for_wrapper(wrapper: &str) -> Option<&'static str> {
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
pub fn primitive_widens_to(from: &str, to: &str) -> bool {
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
pub fn reference_accepts_wrapper(reference: &str, wrapper: &str) -> bool {
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
pub fn is_primitive_descriptor(tok: &str) -> bool {
    matches!(tok, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" | "V")
}

/// `MethodType.canConvert(src, dst)`, transcribed arm for arm.
///
/// Read it against the JDK source rather than against intuition: the third
/// reference->primitive test (`isWrapperType(src) && dw.isConvertibleFrom(...)`)
/// is what makes `Byte -> short` and `Character -> int` convertible while
/// `Number -> char` is not, and dropping it silently narrows 20 accepted cells
/// into refusals.
pub fn mh_can_convert(src: &str, dst: &str) -> bool {
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
pub fn method_type_is_convertible_to(old_desc: &str, new_desc: &str) -> Option<bool> {
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
pub fn descriptor_simple_name(tok: &str) -> Option<String> {
    descriptor_simple_name_with(tok, &|_| None)
}

/// `Class.getSimpleName()` of the LOADED class `name` (internal form), as the
/// JDK derives it: the class's own `InnerClasses` entry's `inner_name` (`""`
/// for an anonymous class, `Local` for a local `Outer$1Local`), else the
/// top-level name after the package, a `$` in it kept (`p/A$B` is `A$B`).
/// `None` when no class of that name is loaded, and the caller keeps its
/// last-`$` text. Interpreter round i1 wave 39, lane L4 (item 2 of
/// `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`).
pub fn loaded_simple_name(ctx: &dyn NativeContext, name: &str) -> Option<String> {
    simple_name_of_class(ctx, ctx.class_id_by_name(name)?, name)
}

/// [`loaded_simple_name`] of the class `id`, whose internal name is `name`.
/// `None` for a hidden class (its simple name is not derived this way).
pub fn simple_name_of_class(ctx: &dyn NativeContext, id: ClassId, name: &str) -> Option<String> {
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
pub fn descriptor_simple_name_with(
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
pub fn method_type_display(desc: &str) -> Option<String> {
    method_type_display_with(desc, &|_| None)
}

/// [`method_type_display`] with each LOADED class named by
/// `Class.getSimpleName()` ([`loaded_simple_name`]): what every
/// `WrongMethodTypeException` text below prints (wave 39).
pub fn method_type_display_in(ctx: &dyn NativeContext, desc: &str) -> Option<String> {
    method_type_display_with(desc, &|name| loaded_simple_name(ctx, name))
}

pub fn method_type_display_with(desc: &str, loaded: &dyn Fn(&str) -> Option<String>) -> Option<String> {
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
pub fn array_element_type_of_descriptor(comp: &str) -> cratonvm_types::ArrayElementType {
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
pub fn widen_primitive_to_descriptor(v: Value, comp: &str) -> Value {
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
pub fn collector_element_box_desc(component: &str, v: Value) -> Option<&'static str> {
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
pub fn collector_call_site_prim(entry_call_site: Option<&str>, index: usize) -> Option<&'static str> {
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
pub fn box_collector_element(
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
pub fn methodtype_to_descriptor(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
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
pub fn mh_type_render_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

/// One direct-mapped line of [`TYPE_RENDER_MEMO`].
pub struct TypeRenderLine {
    pub vm: usize,
    pub epoch: u64,
    pub mt: usize,
    pub desc: Option<std::rc::Rc<str>>,
}

/// Slots in [`TYPE_RENDER_MEMO`]; a power of two (the index is a mask).
pub const TYPE_RENDER_MEMO_SLOTS: usize = 32;

thread_local! {
    /// Per-thread memo of rendered `MethodType` descriptors (MH16-3). The
    /// rule is [`DIRECT_LANE_MEMO`]'s: within one collection epoch nothing
    /// moves or is freed, so an equal address is the same `MethodType`, and a
    /// `MethodType`'s `rtype` / `ptypes` are never rewritten after it is
    /// built (the JDK's are final; this file stores into a `MethodType` only
    /// while minting it, before any reader has it). A context reporting VM
    /// identity `0` (the mocks) is never memoised.
    pub static TYPE_RENDER_MEMO: std::cell::RefCell<Vec<TypeRenderLine>> =
        std::cell::RefCell::new(
            (0..TYPE_RENDER_MEMO_SLOTS)
                .map(|_| TypeRenderLine { vm: 0, epoch: 0, mt: 0, desc: None })
                .collect(),
        );
}

/// [`methodtype_to_descriptor`] through [`TYPE_RENDER_MEMO`]. The epoch is read
/// BEFORE the render, so a render that collected (and moved `mt`) is stored
/// under an epoch no later lookup reads. Only a rendered answer is stored.
pub fn methodtype_to_descriptor_memoised(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
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
pub fn methodtype_to_descriptor_rendered(ctx: &mut dyn NativeContext, mt: ObjectRef) -> Option<String> {
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
pub fn mh_adapter_desc(ctx: &mut dyn NativeContext, target: ObjectRef) -> Option<String> {
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

pub fn mh_type_descriptor(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
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
pub fn mh_downcall_adapt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The slot an `asType` copy of a downcall carrier keeps its C function's own
/// method descriptor in (a `String`; [`mh_downcall_as_type`]). The first slot
/// past this file's synthetic window ([`MH_SLOT_COUNT`]) and below
/// `panama::DOWNCALL_BASE`, which `alloc_downcall_handle` null-fills, so an
/// unadapted carrier reads `null` there.
pub const MH_DOWNCALL_LEAF_DESC: usize = MH_SLOT_COUNT;

/// The C function's own descriptor an `asType` copy recorded, or `None` (an
/// unadapted carrier, whose `type` is that descriptor).
pub fn mh_downcall_leaf_desc(ctx: &dyn NativeContext, mh: ObjectRef) -> Option<String> {
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
pub fn mh_downcall_door_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if !mh_downcall_adapt_enabled() || !crate::panama::is_downcall_handle(&*ctx, mh) {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

