// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Throwable, StackTraceElement, Enum, and Record native method implementations.

use cratonvm_native_api::{NativeCallback, NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::obj_arg;

/// Root the object `value` carries, if any (gc-common w19-e).
///
/// For a `Value` a native holds across a GC point: an argument it stores
/// later, or a Java call's result it passes to the next call. Non-reference
/// values need no root and yield `None`; [`w19e_rooted_value`] replays them.
pub fn w19e_root_value(
    scope: &mut NativeHandleScope<'_>,
    value: Value,
) -> Option<cratonvm_native_api::NativeHandle> {
    match value {
        Value::Object(Some(obj)) => Some(scope.root(obj)),
        _ => None,
    }
}

/// `value` at its current address, through the handle [`w19e_root_value`]
/// gave it.
pub fn w19e_rooted_value(
    scope: &NativeHandleScope<'_>,
    value: Value,
    handle: Option<&cratonvm_native_api::NativeHandle>,
) -> Value {
    match handle {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => value,
    }
}

// ---------------------------------------------------------------------------
// Per-throw allocation caches.
//
// `new Exception(msg)` and `printStackTrace` are extremely hot — on a
// `printStackTrace` of a 30-deep trace we allocate one StackTraceElement,
// three Strings (class/method/file), and look up the StackTraceElement class
// id per frame. The audit (audit-2026-05-16) flagged three sources of
// per-throw overhead worth eliminating:
//
//   1. `set_field_by_name(this, "detailMessage", ...)` walks the Throwable
//      class hierarchy on every call to resolve the slot index. Cache the
//      resolved index once and reuse across all `Throwable.<init>` paths.
//   2. `ClassId::new(0)` was hard-coded for the StackTraceElement class id
//      in `build_ste` — `getClass()` on the returned STE then surfaced the
//      Object class (id 0) instead of `java/lang/StackTraceElement`. Cache
//      the real class id on first use.
//   3. The dotted-form class name (`java.lang.Foo` from `java/lang/Foo`) is
//      already cached per ClassId by `lang_class::dotted_class_name`. The
//      audit suggests reusing that cache to share the `Arc<str>` across the
//      STE-build path and the `Class.getName()` path.
//
// The VM owns these caches. Keeping a slot index in a process global silently
// corrupts a second VM whose bootstrap layout differs from the first one's.
// Failed early-boot lookups still return `None`, so they are retried later.
// ---------------------------------------------------------------------------

/// Resolve & cache a Throwable field's slot index, falling back to
/// name-based set on cache miss / out-of-bounds.
///
/// `resolve_field_index` walks the class hierarchy each call; caching the
/// result eliminates that walk on the hot path. We bounds-check the cached
/// index against the actual object's field count because synthetic-stub
/// Throwable subclasses use a smaller layout (only 2 slots) than the
/// real-JDK Throwable layout (3 slots) — if the cache was populated from
/// the real layout but the runtime object is a synthetic stub, we must
/// fall back to the by-name path which handles both layouts.
#[inline]
fn cached_throwable_field_index(ctx: &dyn NativeContext, field_name: &str) -> Option<usize> {
    ctx.throwable_field_index(field_name)
}

/// Write a Throwable field, preferring the cached slot index and falling
/// back to name-based lookup if the cached index is stale (object has
/// fewer slots than expected — synthetic-stub layout).
#[inline]
/// Slot fallback for a SYNTHETIC `Throwable`.
///
/// `Throwable`'s fields are addressed by name, which is right for a real-JDK
/// receiver. A synthetically-allocated one has no field names at all
/// (`ensure_synthetic_class` mints unnamed slots), so both the write and the
/// read silently no-op and e.g. `initCause` followed by `getCause` yields null.
///
/// There is no declared layout for a synthetic Throwable, so this defines one.
/// It is consulted ONLY when the by-name lookup fails, so a real receiver is
/// untouched and the two paths can never disagree about the same object.
fn synthetic_throwable_slot(field_name: &str) -> Option<usize> {
    match field_name {
        "detailMessage" => Some(0),
        "cause" => Some(1),
        "suppressedExceptions" => Some(2),
        _ => None,
    }
}

/// The slot `java/lang/Throwable` itself declares for `field_name`, returned
/// ONLY when the receiver's own class declares a DIFFERENT field of the same
/// name — i.e. when the receiver SHADOWS one of `Throwable`'s fields.
///
/// A `Throwable` subclass may legally declare a field whose name collides with
/// one of `Throwable`'s own, and real code does: H2 1.2's
/// `org.h2.jdbc.JdbcSQLException` declares `private final Throwable cause` and
/// assigns it in its constructor on the line before it calls `initCause(cause)`.
///
/// Javac resolves `getfield`/`putfield` against the class named in the constant
/// pool, so `Throwable.initCause`'s own bytecode always reaches
/// `Throwable.cause` no matter what a subclass declares. A name-keyed lookup on
/// the RECEIVER does not — it answers the most-derived declaration. That is a
/// different slot from the one the writers in this file target:
/// `write_throwable_field_cached` resolves its index against
/// `java/lang/Throwable` explicitly (see `cached_throwable_field_index`). So the
/// two halves of every read/write pair addressed different memory the moment a
/// subclass shadowed the name.
///
/// The visible consequence: `native_exc_init_message` wrote the `cause = this`
/// sentinel into `Throwable`'s slot, `native_throwable_init_cause` read H2's
/// slot, saw the value H2's constructor had just stored there, and refused the
/// call with `IllegalStateException: Can't overwrite cause with ...`. Every
/// `JdbcSQLException` H2 built then failed to construct, each refusal wrapped by
/// the next JDBC layer — the five-deep cascade `org.h2.test.unit.TestUpgrade`
/// died on (found 2026-08-14).
///
/// Returning `None` for a receiver that merely INHERITS the field keeps every
/// existing path byte-identical; only the shadowing case is redirected.
fn shadowed_throwable_slot(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> Option<usize> {
    let declared = cached_throwable_field_index(ctx, field_name)?;
    let own = ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), field_name)?;
    if own == declared {
        return None;
    }
    (declared < ctx.object_num_fields(this)).then_some(declared)
}

/// Read a field `java/lang/Throwable` declares, from the slot `Throwable`
/// declares it in rather than whatever the receiver's class resolves the name
/// to. See [`shadowed_throwable_slot`].
pub fn throwable_field_get(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> Value {
    if let Some(idx) = shadowed_throwable_slot(ctx, this, field_name) {
        return ctx.get_field(this, idx);
    }
    ctx.get_field_by_name(this, field_name)
}

/// Write-side companion to [`throwable_field_get`]. Keeps a shadowing
/// receiver's reads and writes on the same slot.
pub fn throwable_field_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    value: Value,
) {
    if let Some(idx) = shadowed_throwable_slot(ctx, this, field_name) {
        ctx.set_field(this, idx, value);
        return;
    }
    ctx.set_field_by_name(this, field_name, value);
}

/// Write a Throwable field by name, falling back to
/// [`synthetic_throwable_slot`] when the receiver has no field names.
/// Companion to [`read_throwable_field`].
fn write_throwable_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    value: Value,
) {
    throwable_field_set(ctx, this, field_name, value);
    if throwable_field_get(ctx, this, field_name) != value {
        if let Some(slot) = synthetic_throwable_slot(field_name) {
            if slot < ctx.object_num_fields(this) {
                ctx.set_field(this, slot, value);
            }
        }
    }
}

/// Read a Throwable field by name, falling back to
/// [`synthetic_throwable_slot`] when the receiver has no field names.
fn read_throwable_field(ctx: &mut dyn NativeContext, this: ObjectRef, field_name: &str) -> Value {
    let by_name = throwable_field_get(ctx, this, field_name);
    if !matches!(by_name, Value::Object(None)) {
        return by_name;
    }
    // `Object(None)` is ambiguous: `get_field_by_name` answers it BOTH for a
    // name the receiver has no field for (a synthetic stub -> the slot layout
    // below is the only way to read it) AND for a real-JDK field that simply
    // holds null. Only the first case may consult `synthetic_throwable_slot`,
    // so ask whether the receiver's class actually declares/inherits the name.
    //
    // Without this check the fallback fired on every real-JDK `Throwable` whose
    // `cause` is genuinely null and returned raw slot 1 — `detailMessage` in the
    // real layout — so `getCause()` handed back the message `String`. Callers
    // walking the cause chain then dispatched `Throwable` methods on a `String`:
    // `NoSuchMethodError: java.lang.String.getMessage()` out of AssertJ's
    // `ShouldHaveCause` for the five Spring JCache
    // `cacheExceptionRewriteCallStack` tests, whose cached exception is a
    // `SerializationUtils.clone()` round-trip (deserialization writes the fields
    // directly, so `cause` really is null there rather than holding the JDK's
    // `cause = this` sentinel). The same fallback also aliased
    // `suppressedExceptions` onto slot 2 — the real layout's `cause` — so a real
    // Throwable with no suppressed list reported its cause as one.
    let class_id = ctx.class_id_of_object(this);
    if ctx
        .resolve_field_index_by_class_id(class_id, field_name)
        .is_some()
    {
        return by_name;
    }
    if let Some(slot) = synthetic_throwable_slot(field_name) {
        if slot < ctx.object_num_fields(this) {
            return ctx.get_field(this, slot);
        }
    }
    by_name
}

fn write_throwable_field_cached(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    this: ObjectRef,
    value: Value,
) {
    if let Some(idx) = cached_throwable_field_index(ctx, field_name) {
        if idx < ctx.object_num_fields(this) {
            ctx.set_field(this, idx, value);
            return;
        }
    }
    throwable_field_set(ctx, this, field_name, value);
    // A synthetic receiver resolves neither the cached index nor the name, so
    // the write above was a no-op. See `synthetic_throwable_slot`.
    if throwable_field_get(ctx, this, field_name) != value {
        if let Some(slot) = synthetic_throwable_slot(field_name) {
            if slot < ctx.object_num_fields(this) {
                ctx.set_field(this, slot, value);
            }
        }
    }
}

/// Resolve & cache the `java/lang/StackTraceElement` class id. Falls back
/// to `ClassId::new(0)` only if the class still isn't loaded — that's the
/// pre-fix behaviour, preserved here so we don't regress on early-boot
/// callers that pre-date class loading. Once the class loads, every
/// subsequent build_ste call gets the correct id.
#[inline]
fn cached_ste_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    if let Some(cached) = ctx.cached_stack_trace_element_class_id() {
        return cached;
    }
    if let Some(cid) = ctx.class_id_by_name("java/lang/StackTraceElement") {
        ctx.cache_stack_trace_element_class_id(cid);
        return cid;
    }
    if let Ok(cid) = ctx.ensure_class_initialized("java/lang/StackTraceElement") {
        ctx.cache_stack_trace_element_class_id(cid);
        return cid;
    }
    // Class not loaded yet — return the legacy fallback but DO NOT cache,
    // so a later call (after the class loads) can re-resolve correctly.
    ClassId::new(0)
}

/// Populate a freshly-allocated `java/lang/StackTraceElement`'s fields.
///
/// The real JDK 25 `StackTraceElement` instance-field layout is:
///   slot 0 `declaringClassObject` (Class), 1 `classLoaderName` (String),
///   2 `moduleName` (String), 3 `moduleVersion` (String),
///   4 `declaringClass` (String), 5 `methodName` (String),
///   6 `fileName` (String), 7 `lineNumber` (int), 8 `format` (byte).
///
/// Earlier CratonVM code wrote the class/method/file/line into raw slots
/// 0..=3 — which in the real layout are `declaringClassObject`,
/// `classLoaderName`, `moduleName`, `moduleVersion`. That meant
/// `getfield declaringClassObject` returned the *String* class name and
/// `StackTraceElement.computeFormat()` then did `String.getClassLoader0()`
/// → `NoSuchMethodError` (Keycloak 26 boot). It also left
/// `declaringClassObject` null so `computeFormat` would NPE.
///
/// This helper writes by field name when the real-JDK layout is present
/// (detected by resolving `declaringClass`), and additionally populates
/// `declaringClassObject` with the real `Class` mirror so the JDK's
/// `computeFormat()` runs cleanly. It falls back to the legacy raw-slot
/// `[class, method, file, line]` placement only for the synthetic-stub
/// layout (class not carrying the named fields).
///
/// `class_slashed` is the internal `/`-separated name (e.g.
/// `java/lang/Class`); `class_dotted` is the user-facing `.`-separated
/// form already resolved by the caller.
///
/// Returns `ste` at its CURRENT address (gc-common w19-e). This allocates, so
/// the caller's copy of `ste` may be stale afterwards; a caller that stores or
/// returns the element must use the return value (or its own root). Two
/// callers returned their stale copy: `phases_late::reflect_invoke`'s
/// `StackFrame.toStackTraceElement` (fixed) and the two named in
/// `docs/feature-designs/native-dispatch-architecture.md`
/// (both root the element and re-read it).
///
/// For a CAPTURED frame (every caller but the 4-argument constructor): the
/// element also gets the frame class's module and loader names, as HotSpot's
/// `java_lang_StackTraceElement::fill_in` gives it (round 13 wave 13, lane
/// trace3; [`stack_trace_module_prefix`]).
pub fn fill_stack_trace_element(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    class_slashed: &str,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
) -> ObjectRef {
    fill_stack_trace_element_as(
        ctx,
        ste,
        class_slashed,
        class_dotted,
        method_name,
        file_name,
        line,
        true,
        None,
        None,
        None,
    )
}

/// [`fill_stack_trace_element`] for a frame whose EXACT class the capture
/// recorded (`StackTraceEntry::class_id`). Round 14 wave 2 (lane trace;
/// `r13w13-trace3-stack-trace-element-origin-residuals` item 2): the name
/// lookup the plain form does can answer another loader's class of the same
/// name (a webapp and the application loader, an OSGi bundle), whose module,
/// loader name and `declaringClassObject` mirror the element then carried.
/// The id is used only while it still names `class_slashed`
/// (`CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS=0` ignores it).
#[allow(clippy::too_many_arguments)]
pub fn fill_captured_stack_trace_element(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    exact_class: Option<ClassId>,
    class_slashed: &str,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
) -> ObjectRef {
    fill_stack_trace_element_as(
        ctx,
        ste,
        class_slashed,
        class_dotted,
        method_name,
        file_name,
        line,
        true,
        exact_class,
        None,
        None,
    )
}

/// [`fill_captured_stack_trace_element`] for a walked frame whose method name
/// the walk already interned (`StackFrameInfo.name`): `method` is that
/// String, stored as the element's `methodName` as it is -- the interned
/// String `intern_string` of its text answers -- instead of interning the text
/// again. Its address must be current at the call (nothing may allocate
/// between the read of the frame's field and this call); it is rooted before
/// the first allocation. Interpreter round i4 wave 91, lane L2
/// (`StackTraceFillSwitches::walk_element_interned_names`, default off).
#[allow(clippy::too_many_arguments)]
pub fn fill_captured_stack_trace_element_interned_method(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    exact_class: Option<ClassId>,
    class_slashed: &str,
    class_dotted: &str,
    method: ObjectRef,
    file_name: Option<&str>,
    line: i32,
) -> ObjectRef {
    fill_stack_trace_element_as(
        ctx,
        ste,
        class_slashed,
        class_dotted,
        "",
        file_name,
        line,
        true,
        exact_class,
        None,
        Some(method),
    )
}

/// [`fill_captured_stack_trace_element`] for element `memo`'s current index of
/// a `StackTraceElement[]` being built: a frame of a class an earlier element
/// of the same array already named copies that element's origin fields
/// ([`SteOriginMemo`]).
#[allow(clippy::too_many_arguments)]
fn fill_captured_stack_trace_element_memo(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    exact_class: Option<ClassId>,
    class_slashed: &str,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
    memo: &mut SteOriginMemo<'_>,
) -> ObjectRef {
    fill_stack_trace_element_as(
        ctx,
        ste,
        class_slashed,
        class_dotted,
        method_name,
        file_name,
        line,
        true,
        exact_class,
        Some(memo),
        None,
    )
}

/// `CRATONVM_STACK_TRACE_ELEMENT_FRAME_COPY=1` (WALK11-1, JIT round 19 wave
/// 12, lane walk12; default on, default on): one `StackTraceElement[]` build
/// (`fill_stack_trace_elements_from_backtrace`) fills the first element of
/// each (exact class id, internal class name, method, file) as before; every
/// later frame of the same key copies that element's fields
/// ([`STE_FRAME_COPY_FIELDS`]) and stores its own `lineNumber`, instead of
/// deriving the class text, interning three names, materialising the mirror
/// and copying the origin. Every copied field is a function of the key (the
/// interned names are canonical, the mirror and the origin are per class),
/// and `lineNumber` is the only per-frame field of an element, so each
/// element is the one the slow path stores. A recursive trace (`a -> b -> a
/// -> b ...`) fills a handful of elements and copies the rest. Read once per
/// process.
fn stack_trace_element_frame_copy_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true    })}

/// WALK11-1: the elements one array build filled, by (exact class id,
/// internal class name, method name, file name).
type SteFrameCopyRows<'a> =
    std::collections::HashMap<(ClassId, &'a str, &'a str, Option<&'a str>), usize>;

/// The JDK 25 `StackTraceElement` fields a frame-copy takes from the earlier
/// element of its key: every field but `lineNumber`.
const STE_FRAME_COPY_FIELDS: [&str; 8] = [
    "declaringClassObject",
    "classLoaderName",
    "moduleName",
    "moduleVersion",
    "declaringClass",
    "methodName",
    "fileName",
    "format",
];

/// WALK11-1: give the fresh element `ste` every field of `source` (an element
/// of the same key filled earlier in the same array) but the line, which is
/// `line`. Allocates nothing; both addresses must be current.
fn copy_stack_trace_element_frame(
    scope: &NativeHandleScope<'_>,
    source: ObjectRef,
    ste: ObjectRef,
    line: i32,
) {
    let mut values = [Value::Object(None); STE_FRAME_COPY_FIELDS.len()];
    scope.get_fields_by_name(source, &STE_FRAME_COPY_FIELDS, &mut values);
    let mut fields = [("lineNumber", Value::Int(line)); STE_FRAME_COPY_FIELDS.len() + 1];
    for (slot, (name, value)) in fields
        .iter_mut()
        .zip(STE_FRAME_COPY_FIELDS.iter().zip(values))
    {
        *slot = (*name, value);
    }
    scope.set_fields_by_name(ste, &fields);
}

/// The origin fields [`fill_stack_trace_element_full_origin`] stores.
const STE_ORIGIN_FIELDS: [&str; 4] = ["moduleName", "moduleVersion", "classLoaderName", "format"];

/// Round 14 wave 3 (lane trace; proposal T3-3 of `jit-r13-trace3-proposals-RETIRED-20260929.md`):
/// the elements ONE `StackTraceElement[]` build has already filled, by the
/// frame's class. Every origin field is a function of the class (a
/// redefinition changes neither its module nor its loader), so the next frame
/// of that class copies the four fields from the earlier element -- already
/// stored in the array, which the caller roots -- instead of taking the class
/// manager's lock for the module, looking up the version, interning both and
/// materialising the mirror and its loader. Nothing is kept past the build: no
/// per-VM or process state. Only rows of the full-origin path are recorded, so
/// a copy never adds a field that path would not have set.
/// `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO=0` fills every element afresh
/// (`StackTraceFillSwitches::origin_memo`, read once per VM since interpreter
/// round i3 wave 77, lane L2; it was an env read per array).
struct SteOriginMemo<'h> {
    /// The array being built (rooted by the caller).
    array: &'h cratonvm_native_api::NativeHandle,
    /// The index the element being filled is stored at.
    current: usize,
    /// Class -> index of an element already filled for it.
    rows: std::collections::HashMap<ClassId, usize>,
    enabled: bool,
    /// [`ste_real_layout`], asked once per array (interpreter round i1 wave
    /// 49, lane L7): the element class cannot change layout between two
    /// elements of one build. Kept whether or not `enabled` is.
    real_layout: Option<bool>,
}

impl<'h> SteOriginMemo<'h> {
    /// A memo for the array `array` roots; `enabled` is the VM's
    /// `StackTraceFillSwitches::origin_memo`.
    fn new(array: &'h cratonvm_native_api::NativeHandle, enabled: bool) -> Self {
        Self {
            array,
            current: 0,
            rows: std::collections::HashMap::new(),
            enabled,
            real_layout: None,
        }
    }

    /// [`ste_real_layout`], answered once for the array being built.
    fn real_layout(&mut self, ctx: &mut dyn NativeContext) -> bool {
        *self.real_layout.get_or_insert_with(|| ste_real_layout(ctx))
    }

    /// The element filled next is stored at `index`.
    fn at(&mut self, index: usize) {
        self.current = index;
    }

    /// Copy the origin fields of the element an earlier frame of `class_id`
    /// got onto the element `ste_h` roots; `false` when there is none. Allocates
    /// nothing, so both addresses stay current.
    fn copy_into(
        &self,
        scope: &NativeHandleScope<'_>,
        ste_h: &cratonvm_native_api::NativeHandle,
        class_id: ClassId,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(&source) = self.rows.get(&class_id) else {
            return false;
        };
        let array = scope.get(self.array);
        let Value::Object(Some(source)) = scope.get_array_element(array, source) else {
            return false;
        };
        let ste = scope.get(ste_h);
        // One reference canonicalisation and class lookup per object, not per
        // field (wave 49, lane L7); the same fields, read then written.
        let mut values = [Value::Object(None); STE_ORIGIN_FIELDS.len()];
        scope.get_fields_by_name(source, &STE_ORIGIN_FIELDS, &mut values);
        let fields: [(&'static str, Value); STE_ORIGIN_FIELDS.len()] =
            std::array::from_fn(|i| (STE_ORIGIN_FIELDS[i], values[i]));
        scope.set_fields_by_name(ste, &fields);
        true
    }

    /// The element at the current index was filled for `class_id`.
    fn remember(&mut self, class_id: ClassId) {
        if self.enabled {
            self.rows.entry(class_id).or_insert(self.current);
        }
    }
}

/// Classes [`FrameOriginRecords`] keeps a row for; a class past it is
/// derived afresh on every element, as with the switch off.
const FRAME_ORIGIN_RECORD_CAP: usize = 1 << 16;

/// [`FrameOrigin::computed_format`] before the class's `computeFormat()`
/// answer is known.
const COMPUTED_FORMAT_UNKNOWN: i32 = i32::MIN;

/// The fields a [`FrameOrigin`] row is read from, on the first element the
/// full-origin fill stored for its class: the interned class name, then
/// [`STE_ORIGIN_FIELDS`].
const FRAME_ORIGIN_ROW_FIELDS: [&str; 5] = [
    "declaringClass",
    "moduleName",
    "moduleVersion",
    "classLoaderName",
    "format",
];

/// Interpreter round i3 wave 75, lane L2
/// (`CRATONVM_STACK_TRACE_ORIGIN_RECORD`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// what the full-origin fill ([`fill_stack_trace_element_full_origin`])
/// stored on the FIRST element of a class, in any array, so that every later
/// element of that class copies it instead of deriving it again (the class
/// manager's lock for the module, the version, two interns, the mirror,
/// `Class.getClassLoader`'s native and the loader's `name`). Every value is a
/// function of the class -- a redefinition changes neither its module nor
/// its loader, and a `ClassId` is never reused (`ClassStore::remove` leaves a
/// tombstone) -- and the row is read back from the element the slow path
/// filled, so a copy is byte for byte what the slow path stores. The
/// references are strong global roots (remapped by a moving collector): the
/// interned names are pool roots already, and the loader's `name` String
/// outlives the loader by one row at most.
struct FrameOrigin {
    /// The class's internal name, as the class records it: a row answers
    /// only a frame whose captured name is this one (what the exact-class
    /// filter of [`fill_stack_trace_element_as`] asks).
    slashed: std::sync::Arc<str>,
    /// The text the element's `declaringClass` was interned from; a caller
    /// rendering the class another way interns its own text.
    dotted: std::sync::Arc<str>,
    /// Global-root handles of the interned `declaringClass`, `moduleName`,
    /// `moduleVersion` and `classLoaderName` (`0`: the field was null).
    handles: [usize; 4],
    /// The `format` byte the fill stored.
    format: i32,
    /// `--real-jdk`: the `computeFormat()` answer
    /// [`stack_trace_element_computed_format`] gave once the module system
    /// was up (it cannot change after), or [`COMPUTED_FORMAT_UNKNOWN`].
    computed_format: std::sync::atomic::AtomicI32,
}

/// This VM's [`FrameOrigin`] rows by class
/// ([`NativeContext::stack_trace_origin_record_slot`]).
pub struct FrameOriginRecords {
    rows: cratonvm_types::lock_order::OrderedPlRwLock<
        rustc_hash::FxHashMap<ClassId, std::sync::Arc<FrameOrigin>>,
    >,
}

/// This VM's [`FrameOriginRecords`] under `CRATONVM_STACK_TRACE_ORIGIN_RECORD`,
/// made on first use; `None` with the switch off or for a context with no
/// slot.
fn frame_origin_records(ctx: &dyn NativeContext) -> Option<&FrameOriginRecords> {
    if !ctx.stack_trace_origin_record() {
        return None;
    }
    let slot = ctx.stack_trace_origin_record_slot()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(FrameOriginRecords {
            rows: cratonvm_types::lock_order::OrderedPlRwLock::new(
                rustc_hash::FxHashMap::default(),
                cratonvm_types::lock_order::LockLevel::Scratch,
            ),
        })
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<FrameOriginRecords>()
}

/// The row recorded for `class_id`, if any (the switch off: none).
fn frame_origin_row(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Option<std::sync::Arc<FrameOrigin>> {
    frame_origin_records(ctx)?
        .rows
        .read()
        .get(&class_id)
        .cloned()
}

/// A row's handle as the value it names: `Some(None)` for a null field,
/// `None` when the handle no longer resolves (the caller then fills afresh).
fn resolve_frame_origin_handle(
    ctx: &dyn NativeContext,
    handle: usize,
) -> Option<Option<ObjectRef>> {
    if handle == 0 {
        return Some(None);
    }
    ctx.resolve_global_root(handle).map(Some)
}

/// Record the origin the full-origin fill just stored on the element `ste_h`
/// roots, for a frame of `class_id` whose class name the element was given
/// as `class_dotted`. Does nothing with the switch off, when the class
/// already has a row, past [`FRAME_ORIGIN_RECORD_CAP`], or when a field
/// cannot be held (a context with no global roots). Allocates nothing.
fn record_frame_origin(
    scope: &mut NativeHandleScope<'_>,
    ste_h: &cratonvm_native_api::NativeHandle,
    class_id: ClassId,
    class_dotted: &str,
) {
    let wanted = match frame_origin_records(&**scope) {
        Some(records) => {
            let rows = records.rows.read();
            rows.len() < FRAME_ORIGIN_RECORD_CAP && !rows.contains_key(&class_id)
        }
        None => false,
    };
    if !wanted {
        return;
    }
    let Some(slashed) = scope.class_name_arc_of_id(class_id) else {
        return;
    };
    let mut values = [Value::Object(None); FRAME_ORIGIN_ROW_FIELDS.len()];
    let ste = scope.get(ste_h);
    scope.get_fields_by_name(ste, &FRAME_ORIGIN_ROW_FIELDS, &mut values);
    let (Value::Object(Some(_)), Value::Int(format)) = (values[0], values[4]) else {
        return;
    };
    let mut handles = [0usize; 4];
    let mut held = true;
    for (slot, value) in handles.iter_mut().zip(values) {
        if let Value::Object(Some(obj)) = value {
            *slot = scope.add_global_root(obj);
            if *slot == 0 {
                held = false;
                break;
            }
        }
    }
    if !held {
        release_frame_origin_handles(scope, &handles);
        return;
    }
    let row = std::sync::Arc::new(FrameOrigin {
        slashed,
        dotted: std::sync::Arc::from(class_dotted),
        handles,
        format,
        computed_format: std::sync::atomic::AtomicI32::new(COMPUTED_FORMAT_UNKNOWN),
    });
    let stored = match frame_origin_records(&**scope) {
        Some(records) => {
            let mut rows = records.rows.write();
            if rows.len() < FRAME_ORIGIN_RECORD_CAP && !rows.contains_key(&class_id) {
                rows.insert(class_id, row);
                true
            } else {
                false
            }
        }
        None => false,
    };
    if !stored {
        // Another thread recorded the class first: its row is the same.
        release_frame_origin_handles(scope, &handles);
    }
}

fn release_frame_origin_handles(scope: &mut NativeHandleScope<'_>, handles: &[usize]) {
    for &handle in handles {
        if handle != 0 {
            scope.remove_global_root(handle);
        }
    }
}

/// [`fill_stack_trace_element_as`] for a captured frame of `class_id` whose
/// class has a [`FrameOrigin`] row: the method and file names are interned
/// and the mirror materialised as the slow path does, and the class name and
/// origin fields come from the row; every field is stored in one batch. The
/// fields and values are the slow path's. `Err` (no field stored; the
/// element and the method-name String at their current addresses) when a
/// handle no longer resolves; the caller then fills afresh.
///
/// `method_interned` (interpreter round i4 wave 91, lane L2): the method
/// name's interned String when the caller already holds it
/// ([`fill_captured_stack_trace_element_interned_method`]); it is rooted
/// before the first allocation and stored instead of interning `method_name`.
#[allow(clippy::too_many_arguments)]
fn fill_stack_trace_element_from_record(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    row: &FrameOrigin,
    class_id: ClassId,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
    method_interned: Option<ObjectRef>,
) -> Result<ObjectRef, (ObjectRef, ObjectRef)> {
    let mut scope = NativeHandleScope::new(ctx);
    let ste_h = scope.root(ste);
    let meth_pre = method_interned.map(|m| scope.root(m));
    // Every allocation first (the interns, the mirror), then the reads.
    let cls_h = if &*row.dotted == class_dotted {
        None
    } else {
        let s = scope.intern_string(class_dotted);
        Some(scope.root(s))
    };
    let meth_h = match meth_pre {
        Some(h) => h,
        None => {
            let meth = scope.intern_string(method_name);
            scope.root(meth)
        }
    };
    let file_h = file_name.map(|f| {
        let s = scope.intern_string(f);
        scope.root(s)
    });
    if let (Some(text), Some(handle)) = (file_name, &file_h) {
        scope.retain_stack_trace_source_file(class_id, text, scope.get(handle));
    }
    crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Intern);
    let mirror = scope.get_class_mirror(class_id);
    crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Mirror);
    let cls_str = match &cls_h {
        Some(h) => Some(Some(scope.get(h))),
        None => resolve_frame_origin_handle(&*scope, row.handles[0]),
    };
    let module = resolve_frame_origin_handle(&*scope, row.handles[1]);
    let version = resolve_frame_origin_handle(&*scope, row.handles[2]);
    let loader = resolve_frame_origin_handle(&*scope, row.handles[3]);
    let (Some(cls_str), Some(module), Some(version), Some(loader)) =
        (cls_str, module, version, loader)
    else {
        // The element at its CURRENT address: the interns above can collect.
        return Err((scope.get(&ste_h), scope.get(&meth_h)));
    };
    crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Row);
    let ste = scope.get(&ste_h);
    let file_val = match &file_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => Value::Object(None),
    };
    let mut fields = [("", Value::Object(None)); 9];
    let mut n = 0;
    for field in [
        ("declaringClass", Value::Object(cls_str)),
        ("methodName", Value::Object(Some(scope.get(&meth_h)))),
        ("fileName", file_val),
        ("lineNumber", Value::Int(line)),
        ("declaringClassObject", Value::Object(Some(mirror))),
    ] {
        fields[n] = field;
        n += 1;
    }
    // The slow path stores a module, version or loader name only when there
    // is one; the element is fresh, so a skipped field stays null either way.
    for (name, value) in [
        ("moduleName", module),
        ("moduleVersion", version),
        ("classLoaderName", loader),
    ] {
        if value.is_some() {
            fields[n] = (name, Value::Object(value));
            n += 1;
        }
    }
    fields[n] = ("format", Value::Int(row.format));
    n += 1;
    scope.set_fields_by_name(ste, &fields[..n]);
    crate::lang_stackwalker::ste_sub_mark_row_hit(crate::lang_stackwalker::SteSub::Store);
    Ok(scope.get(&ste_h))
}

// `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` -- default ON (round 14 wave 2,
// lane trace, both modes): see [`fill_captured_stack_trace_element`]. `0`
// resolves every frame's class by name again. Read once per VM since
// interpreter round i3 wave 77, lane L2
// (`StackTraceFillSwitches::exact_class`; it was a process `OnceLock`).

/// Does the loaded `java/lang/StackTraceElement` have the real JDK layout (a
/// named `declaringClass` field)? The synthetic stub has no named fields. This
/// can load the class.
fn ste_real_layout(ctx: &mut dyn NativeContext) -> bool {
    ctx.resolve_field_index("java/lang/StackTraceElement", "declaringClass")
        .is_some()
}

/// [`fill_stack_trace_element`]; `captured_frame == false` is the public
/// 4-argument `StackTraceElement` constructor, which names no module or loader
/// (`this(null, null, null, declaringClass, ...)`), so neither is filled in.
#[allow(clippy::too_many_arguments)]
fn fill_stack_trace_element_as(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    class_slashed: &str,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
    captured_frame: bool,
    exact_class: Option<ClassId>,
    mut origin_memo: Option<&mut SteOriginMemo<'_>>,
    mut method_interned: Option<ObjectRef>,
) -> ObjectRef {
    // Interpreter round i3 wave 75, lane L2 (`CRATONVM_STACK_TRACE_ORIGIN_RECORD`):
    // a class some earlier element (of any array) was filled for copies that
    // element's origin from the per-VM record. Only where the slow path below
    // would resolve the frame to the same class: the captured exact id, still
    // naming `class_slashed`, under the default exact-class rule. With the
    // switch off this is one context call per element.
    let mut ste = ste;
    // The VM's fill switches, read once per element (wave 77: per VM, not
    // process `OnceLock`s).
    let switches = ctx.stack_trace_fill_switches();
    // OpenJDK 25 java_lang_StackTraceElement::decode_file_and_line caches
    // SourceFile on the Class mirror. Redefinition updates JVMTI metadata,
    // but a non-null decoding still uses the mirror's first cached filename.
    // Do this only for captured frames; the public constructor uses its text.
    let trace_class = captured_frame.then(|| {
        exact_class.filter(|&cid| switches.exact_class
            && ctx.class_name_arc_of_id(cid).as_deref() == Some(class_slashed))
            .or_else(|| ctx.class_id_by_name(class_slashed))
    }).flatten();
    let trace_file = trace_class.map(|cid| ctx.stack_trace_source_file(cid, file_name));
    let file_name = match trace_file.as_ref() {
        Some(file) => file.as_deref(),
        None => file_name,
    };
    if let Some(cid) =
        exact_class.filter(|_| captured_frame && switches.exact_class && switches.module_prefix)
    {
        let row = frame_origin_row(&*ctx, cid);
        // WALK8-2: the row lookup (census only).
        crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Row);
        if let Some(row) = row {
            if &*row.slashed == class_slashed {
                match fill_stack_trace_element_from_record(
                    ctx,
                    ste,
                    &row,
                    cid,
                    class_dotted,
                    method_name,
                    file_name,
                    line,
                    method_interned,
                ) {
                    Ok(filled) => return filled,
                    Err((current, meth_now)) => {
                        ste = current;
                        if method_interned.is_some() {
                            method_interned = Some(meth_now);
                        }
                    }
                }
            }
        }
    }
    // `ste` is a young object the caller allocated one statement ago, and every
    // `create_string` below can collect. Holding the caller's address across
    // them and storing through it writes the whole element into a copy nothing
    // reads: `getStackTrace()` then answers an array of blank frames, with no
    // probe firing, because the values stored were live the whole time and only
    // the RECEIVER was stale. Root it — and the strings, which have the same
    // problem in the other direction — and read every address back at the store.
    let mut scope = NativeHandleScope::new(ctx);
    let ste_h = scope.root(ste);
    // Wave 91 (lane L2): a method name the caller already holds interned is
    // rooted before the first allocation and stored as it is.
    let meth_pre = method_interned.map(|m| scope.root(m));
    // HotSpot interns all three (`java_lang_StackTraceElement::fill_in`), so
    // they name the pooled constructor (bigdec page design A, step 1).
    let cls_obj = scope.intern_string(class_dotted);
    let cls_h = scope.root(cls_obj);
    let meth_h = match meth_pre {
        Some(h) => h,
        None => {
            let meth_obj = scope.intern_string(method_name);
            scope.root(meth_obj)
        }
    };
    let file_h = match file_name {
        Some(f) => {
            let s = scope.intern_string(f);
            Some(scope.root(s))
        }
        None => None,
    };
    if let (Some(cid), Some(text), Some(handle)) = (trace_class, file_name, &file_h) {
        scope.retain_stack_trace_source_file(cid, text, scope.get(handle));
    }
    crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Intern);

    // Real-JDK layout iff the loaded class carries a named `declaringClass`
    // String field. Synthetic-stub StackTraceElement has no named fields.
    // (This can load the class, so it stays ahead of every address read.)
    // Asked once per array when the caller builds one (wave 49, lane L7).
    let real_layout = match origin_memo.as_deref_mut() {
        Some(memo) => memo.real_layout(&mut *scope),
        None => ste_real_layout(&mut *scope),
    };

    if real_layout {
        // Populate the transient `declaringClassObject` with the real Class
        // mirror so `computeFormat()` (which does
        // `getfield declaringClassObject; invokevirtual getClassLoader0`)
        // operates on a genuine Class — not null (NPE) and not a String
        // (NoSuchMethodError).
        //
        // ES-FAIL-06: the earlier assumption that "computeFormat tolerates a
        // null here" is WRONG for JDK 25 — `StackTraceElement.computeFormat()`
        // does `declaringClassObject.getClassLoader0()` with no null guard, so
        // a single null element NPEs the whole `getStackTrace()`. That fails
        // ~every Elasticsearch `ESTestCase` (RandomizedRunner augments a suite
        // throwable's stack trace). When the frame's class can't be resolved by
        // name (lambdas, not-yet-loaded, etc.), fall back to `java/lang/Object`'s
        // bootstrap-loaded mirror so `computeFormat` runs cleanly — only the
        // module/loader display prefix is affected, never correctness.
        // The frame's own class: the capture's exact id while it still names
        // this class (round 14 wave 2), else the name lookup as before.
        //
        // Wave 49 (lane L7): the mirror is materialised FIRST (it allocates),
        // and then all five fields are written in one batch, through one
        // reference canonicalisation and class lookup, where each used to pay
        // its own; the name compare reads the class's `Arc` name instead of
        // allocating a `String`. The fields and values are the same.
        let frame_cid = exact_class
            .filter(|&cid| {
                switches.exact_class
                    && scope.class_name_arc_of_id(cid).as_deref() == Some(class_slashed)
            })
            .or_else(|| scope.class_id_by_name(class_slashed));
        let mirror_cid = frame_cid.or_else(|| scope.class_id_by_name("java/lang/Object"));
        let mirror = mirror_cid.map(|cid| scope.get_class_mirror(cid));
        crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Mirror);
        let ste = scope.get(&ste_h);
        let cls_str = scope.get(&cls_h);
        let meth_str = scope.get(&meth_h);
        let file_val = match &file_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => Value::Object(None),
        };
        let fields = [
            ("declaringClass", Value::Object(Some(cls_str))),
            ("methodName", Value::Object(Some(meth_str))),
            ("fileName", file_val),
            ("lineNumber", Value::Int(line)),
            ("declaringClassObject", Value::Object(mirror)),
        ];
        let written = if mirror.is_some() {
            fields.len()
        } else {
            fields.len() - 1
        };
        scope.set_fields_by_name(ste, &fields[..written]);
        crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Store);
        // Round 13 wave 13 (lane trace3): the module and loader fields
        // HotSpot's `fill_in` sets too, so `toString()` prints
        // `java.base/java.lang.Thread.run(Thread.java:1583)`. Only for the
        // frame's own class, never the `java/lang/Object` fallback above.
        if captured_frame && switches.module_prefix {
            if let Some(cid) = frame_cid {
                let copied = origin_memo
                    .as_deref()
                    .is_some_and(|memo| memo.copy_into(&scope, &ste_h, cid));
                let full = !copied && fill_stack_trace_element_origin(&mut scope, &ste_h, cid);
                if full {
                    if let Some(memo) = origin_memo {
                        memo.remember(cid);
                    }
                }
                // Wave 75 (lane L2): the element now carries the full-origin
                // fields (filled, or copied from a full-origin element), so
                // it seeds the per-VM record. Nothing with the switch off.
                if copied || full {
                    record_frame_origin(&mut scope, &ste_h, cid, class_dotted);
                }
            }
        }
        crate::lang_stackwalker::ste_sub_mark(crate::lang_stackwalker::SteSub::Origin);
    } else {
        let ste = scope.get(&ste_h);
        let cls_str = scope.get(&cls_h);
        let meth_str = scope.get(&meth_h);
        let file_val = match &file_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => Value::Object(None),
        };
        // Legacy synthetic-stub layout: [class, method, file, line].
        scope.set_field(ste, 0, Value::Object(Some(cls_str)));
        scope.set_field(ste, 1, Value::Object(Some(meth_str)));
        scope.set_field(ste, 2, file_val);
        scope.set_field(ste, 3, Value::Int(line));
    }
    scope.get(&ste_h)
}

/// `CRATONVM_STACK_TRACE_MODULE_PREFIX` -- default ON (round 13 wave 13, lane
/// trace3, both modes; `r13w12-misc12-native-printer-omits-module-prefix-FIXED-20260929.md`).
/// A `StackTraceElement` the VM fills names its class's module (and a named
/// user loader), and the `--compatible` native printer prints the prefix
/// `StackTraceElement.toString()` derives from them. `0` restores the
/// prefix-less elements and text. Read once per VM since interpreter round
/// i3 wave 77, lane L2 (`StackTraceFillSwitches::module_prefix`; it was a
/// process `OnceLock`).
fn stack_trace_module_prefix(ctx: &dyn NativeContext) -> bool {
    ctx.stack_trace_fill_switches().module_prefix
}

/// A JDK module by name (`java.*` / `jdk.*`): the fallback of
/// [`boot_module_hashed_in_java_base`] when `java.base` records no hashes.
fn is_jdk_module_name(module: &str) -> bool {
    module.starts_with("java.") || module.starts_with("jdk.")
}

/// `StackTraceElement.isHashedInJavaBase(m)`'s set test for the BOOT-layer
/// module `module`: `java.base`, or a name `java.base`'s recorded hashes
/// list (`HashedModules`). An upgradeable JDK module (`java.compiler`) is
/// not hashed, so HotSpot's `toString()` keeps its `@version`. When
/// `java.base` records no hashes the VM keeps its historic name screen
/// ([`is_jdk_module_name`]); the JDK's `HashedModules` would then be empty
/// and print every version, which is not the image HotSpot runs.
/// Interpreter round i3 wave 79, lane L2
/// (`i74-L2-compatible-stack-trace-format-uses-a-module-name-screen-20261107.md`,
/// `i77-L2-compatible-format-bits-screen-module-names-20261110.md`): was the
/// name screen alone, in `--compatible`'s fill, printer and `computeFormat()`.
pub fn boot_module_hashed_in_java_base(ctx: &dyn NativeContext, module: &str) -> bool {
    hashed_answer_or_name_screen(ctx.module_hashed_in_java_base(module), module)
}

/// The loader id (0 bootstrap, 1 platform, 2 application, 3+ user-defined)
/// a captured frame of `class_id` names, for its `classLoaderName` and its
/// `BUILTIN_CLASS_LOADER` bit: the VM's record, except that a boot-namespace
/// class of a boot-layer named module other than `java.base` that the JDK's
/// table gives to the platform (or application) loader names that loader, as
/// `Class.getClassLoader()` reports it
/// (`classloader::builtin_loader_for_image_class`). The VM defines every
/// image class in its boot namespace; HotSpot's platform loader defines
/// `java.compiler`, `java.sql`, ... and names it `platform`. Interpreter
/// round i3 wave 79, lane L2 (`L2W79StackTraceDefects` `compiler-*`: was
/// `loader=null`).
pub fn frame_loader_id(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    boot_module: Option<&str>,
) -> i32 {
    let recorded = ctx.loader_id_of_class(class_id);
    if recorded != 0 {
        return recorded;
    }
    match boot_module {
        Some(module) if module != "java.base" && !module.is_empty() => {}
        _ => return 0,
    }
    let Some(name) = ctx.class_name_arc_of_id(class_id) else {
        return 0;
    };
    crate::classloader::image_class_builtin_loader_id(ctx, &name).unwrap_or(0)
}

/// The pure half of [`boot_module_hashed_in_java_base`].
fn hashed_answer_or_name_screen(recorded: Option<bool>, module: &str) -> bool {
    recorded.unwrap_or_else(|| is_jdk_module_name(module))
}

/// `(moduleName, moduleVersion)` of a frame of `class_id` as they end up
/// printed: the class's named module (`None` for the unnamed one), and its
/// version only outside the JDK's own modules (HotSpot fills the JDK's too,
/// but `computeFormat` never prints it; see the page).
///
/// By the module's identity: a class of a non-boot `ModuleLayer`'s module
/// (`--real-jdk`) names that module and its descriptor's version, as
/// HotSpot's `fill_in` does from the class's `ModuleEntry`; its
/// `module_name` is `None` (interpreter round i1 wave 45, lane L5).
fn frame_module(ctx: &dyn NativeContext, class_id: ClassId) -> (Option<String>, Option<String>) {
    let module = match ctx.module_identity_of_class(class_id) {
        cratonvm_native_api::ClassModuleIdentity::Layer { name, version, .. } => {
            return (Some(name), version.filter(|v| !v.is_empty()));
        }
        cratonvm_native_api::ClassModuleIdentity::Named(name) => name,
        cratonvm_native_api::ClassModuleIdentity::Unnamed => {
            return (builtin_dynamic_module_name(ctx, class_id), None);
        }
    };
    let version = if boot_module_hashed_in_java_base(ctx, &module) {
        None
    } else {
        ctx.module_version(&module).filter(|v| !v.is_empty())    };
    (Some(module), version)}

/// The name of the DYNAMIC module (no layer) a built-in loader's class is in,
/// for a class the module registry calls unnamed: `java.lang.reflect.Proxy`'s
/// `jdk.proxyN` and `MethodHandleProxies`' `jdk.MHProxyN`, which
/// `Modules.defineModule` defines to the app / platform loader and HotSpot's
/// `defineModule0` files the class's package under. HotSpot's
/// `StackTraceElement` fill names it (`jdk.proxy1/jdk.proxy1.$Proxy0.run(Unknown
/// Source)`); this VM's element read the registry alone and printed no module
/// (interpreter round i4 wave 87, lane L2; probe
/// `L2W87PrintStackTraceShapes.java`, row `proxy-frame`).
/// The record is the one `Class.getModule()` answers from (`jboss_jdkspecific`
/// `layer_module_of_class`, namespace 0, wave 85 lane L4). Such modules carry
/// no version (their descriptors are built without one).
///
/// Cost: only a class of a `jdk/` package of a built-in loader, after some
/// dynamic module was recorded, pays the map lookup; every other frame pays a
/// name-prefix test (the JDK names these packages `jdk.proxyN` / `jdk.MHProxyN`,
/// and only `java.base` code can call `Modules.defineModule`).
fn builtin_dynamic_module_name(ctx: &dyn NativeContext, class_id: ClassId) -> Option<String> {
    let name = ctx.class_name_arc_of_id(class_id)?;
    if !name.starts_with("jdk/") || !ctx.has_layer_module_packages() {
        return None;
    }
    // 1 platform, 2 application: the built-in loaders `record_layer_module`
    // files dynamic modules for.
    if !matches!(ctx.loader_id_of_class(class_id), 1 | 2) {
        return None;
    }
    let package = &name[..name.rfind('/')?];
    let module = ctx.layer_module_of_package(0, package)?;
    match ctx.get_field_by_name(module, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).filter(|n| !n.is_empty()),
        _ => None,
    }
}

/// The `name` String of a USER-DEFINED loader of `class_id`, or `None`: the
/// built-in loaders' names (`app`, `platform`) are what `computeFormat` drops,
/// and an unnamed loader prints nothing. Reads the mirror, which may allocate.
fn frame_loader_name(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<ObjectRef> {
    // 0 bootstrap, 1 platform, 2 application, 3+ user-defined.
    if ctx.loader_id_of_class(class_id) < 3 {
        return None;
    }
    let mirror = ctx.get_class_mirror(class_id);
    let Value::Object(Some(loader)) = ctx.get_field_by_name(mirror, "classLoader") else {
        return None;
    };
    match crate::classloader_real::loader_declared_name(&*ctx, loader) {
        Value::Object(Some(name)) => Some(name),
        _ => None,
    }
}

/// `StackTraceElement.toString()`'s `loader/module@version/` prefix, from the
/// pieces as printed (already filtered by [`frame_module`] /
/// [`frame_loader_name`]). The shared half of proposal M12-1 of
/// `jit-r13-misc12-proposals-RETIRED-20260929.md`: [`throwable_frame_text`] renders with it, and
/// [`fill_stack_trace_element`] stores the fields the JDK's `toString()` derives
/// the same text from.
pub fn stack_frame_prefix(
    loader: Option<&str>,
    module: Option<&str>,
    version: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(loader) = loader.filter(|l| !l.is_empty()) {
        out.push_str(loader);
        out.push('/');
    }
    if let Some(module) = module.filter(|m| !m.is_empty()) {
        out.push_str(module);
        if let Some(version) = version.filter(|v| !v.is_empty()) {
            out.push('@');
            out.push_str(version);
        }
    }
    if !out.is_empty() {
        out.push('/');
    }
    out
}

/// The printed `loader/module@version/` prefix of a frame of `class_id`, or
/// `""` under `CRATONVM_STACK_TRACE_MODULE_PREFIX=0`. Round 14 wave 2 (lane
/// trace; `r13w13-trace3-stack-trace-element-origin-residuals` item 3): the
/// renderers outside this file (`logmanager.rs`'s `java.util.logging` frames,
/// `reflect_invoke.rs`'s `StackFrame.toString()`) print what
/// [`throwable_frame_text`] prints. Reads the mirror, which may allocate.
pub fn printed_frame_prefix(ctx: &mut dyn NativeContext, class_id: ClassId) -> String {
    if !stack_trace_module_prefix(&*ctx) {
        return String::new();
    }
    let (module, version) = frame_module(&*ctx, class_id);
    let loader = frame_loader_name(ctx, class_id).and_then(|s| ctx.read_string(s));
    stack_frame_prefix(loader.as_deref(), module.as_deref(), version.as_deref())
}

/// Store `moduleName`, `moduleVersion` and `classLoaderName` on the element
/// `ste_h` roots, for a frame of `class_id` (real layout only; see
/// [`fill_stack_trace_element`]). Every allocation happens before the element
/// is read back. `true` when the full-origin form stored every field of
/// [`STE_ORIGIN_FIELDS`] (what [`SteOriginMemo`] may copy).
fn fill_stack_trace_element_origin(
    scope: &mut NativeHandleScope<'_>,
    ste_h: &cratonvm_native_api::NativeHandle,
    class_id: ClassId,
) -> bool {
    // Round 14 wave 2 (lane trace; `r13w13-trace3-stack-trace-element-origin-
    // residuals` item 1): HotSpot's `fill_in` stores the JDK module's version
    // and the BUILT-IN loaders' names too (`getModuleVersion()` answers
    // `25.0.3`, `getClassLoaderName()` answers `app`); only the TEXT drops
    // them, through the `format` bits `computeFormat` sets. The elements the
    // `--compatible` natives hand out never meet `computeFormat`, so the bits
    // are stored here with the fields; under `--real-jdk` `computeFormat`
    // recomputes the same bits from `declaringClassObject`.
    if stack_trace_element_full_origin(&**scope)
        && scope
            .resolve_field_index("java/lang/StackTraceElement", "format")
            .is_some()
    {
        fill_stack_trace_element_full_origin(scope, ste_h, class_id);
        return true;
    }
    let (module, version) = frame_module(&**scope, class_id);
    if let Some(module) = module {
        // HotSpot interns the module name (`StringTable::intern`).
        let name = scope.intern_string(&module);
        let name_h = scope.root(name);
        let version_h = version.map(|v| {
            let s = scope.intern_string(&v);
            scope.root(s)
        });
        let ste = scope.get(ste_h);
        let name = scope.get(&name_h);
        scope.set_field_by_name(ste, "moduleName", Value::Object(Some(name)));
        if let Some(h) = &version_h {
            let version = scope.get(h);
            scope.set_field_by_name(ste, "moduleVersion", Value::Object(Some(version)));
        }
    }
    if let Some(loader_name) = frame_loader_name(&mut **scope, class_id) {
        // Read from the loader after the last allocation: nothing moved it.
        let ste = scope.get(ste_h);
        scope.set_field_by_name(ste, "classLoaderName", Value::Object(Some(loader_name)));
    }
    false
}

/// `CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN` -- default ON (round 14 wave 2,
/// lane trace, both modes): a captured element carries every origin field
/// HotSpot's `fill_in` gives it, plus the matching `format` bits. `0` keeps
/// round 13's printed-subset fields. Read once per VM since interpreter round
/// i3 wave 77, lane L2 (`StackTraceFillSwitches::full_origin`; it was a
/// process `OnceLock`).
fn stack_trace_element_full_origin(ctx: &dyn NativeContext) -> bool {
    ctx.stack_trace_fill_switches().full_origin
}

/// `StackTraceElement.BUILTIN_CLASS_LOADER` / `JDK_NON_UPGRADEABLE_MODULE`
/// (JDK 25 `java/lang/StackTraceElement.java`).
const STE_FORMAT_BUILTIN_CLASS_LOADER: i32 = 0x1;
const STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE: i32 = 0x2;

/// The `format` byte `StackTraceElement.computeFormat` derives for a frame of
/// a class whose recorded loader id is `loader_id` (0 bootstrap, 1 platform,
/// 2 application, 3+ user-defined) and whose named module is `module`.
/// `computeFormat` tests `loader instanceof BuiltinClassLoader` -- true for
/// the platform and application loaders, false for the bootstrap loader
/// (`null`) and a user loader -- and whether the module is hashed in
/// `java.base` (`hashed_in_java_base`: [`boot_module_hashed_in_java_base`]
/// of a boot-layer named module; `false` for the unnamed module and a
/// non-boot layer's).
fn stack_trace_element_format_bits(loader_id: i32, hashed_in_java_base: bool) -> i32 {
    let mut bits = 0;
    if loader_id == 1 || loader_id == 2 {
        bits |= STE_FORMAT_BUILTIN_CLASS_LOADER;
    }
    if hashed_in_java_base {
        bits |= STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE;
    }
    bits
}

/// [`fill_stack_trace_element_origin`]'s full form: `moduleName`,
/// `moduleVersion` of every named module (the JDK's included),
/// `classLoaderName` of any NAMED loader (the built-in `app` / `platform`
/// included), and `format`. Every allocation happens before the element is
/// read back.
fn fill_stack_trace_element_full_origin(
    scope: &mut NativeHandleScope<'_>,
    ste_h: &cratonvm_native_api::NativeHandle,
    class_id: ClassId,
) {
    // A class of a non-boot `ModuleLayer`'s module names that module and its
    // descriptor's version by the module's identity, as [`frame_module`] does
    // (interpreter round i1 wave 45, lane L5): its `module_name` is `None`.
    let (module, version, boot_layer) = match scope.module_identity_of_class(class_id) {
        cratonvm_native_api::ClassModuleIdentity::Layer { name, version, .. } => {
            (Some(name), version.filter(|v| !v.is_empty()), false)
        }
        _ => match scope
            .module_name_of_class(class_id)
            .filter(|m| !m.is_empty())
        {
            None => {
                // A built-in loader's dynamic module (`jdk.proxyN`), wave 87:
                // named, no version, not a boot-layer module
                // (`builtin_dynamic_module_name`).
                match builtin_dynamic_module_name(&**scope, class_id) {
                    Some(dynamic) => (Some(dynamic), None, false),
                    None => (None, None, true),
                }
            }
            module => {
                let version = module
                    .as_deref()
                    .and_then(|m| scope.module_version(m))
                    .filter(|v| !v.is_empty());
                (module, version, true)
            }
        },
    };
    let loader_id = frame_loader_id(
        &mut **scope,
        class_id,
        if boot_layer { module.as_deref() } else { None },
    );
    // Only a boot-layer named module can be hashed in `java.base`.
    let hashed = boot_layer
        && module
            .as_deref()
            .is_some_and(|m| boot_module_hashed_in_java_base(&**scope, m));
    let format = stack_trace_element_format_bits(loader_id, hashed);
    if let Some(module) = &module {
        // HotSpot interns both (`StringTable::intern`).
        let name = scope.intern_string(module);
        let name_h = scope.root(name);
        let version_h = version.map(|v| {
            let s = scope.intern_string(&v);
            scope.root(s)
        });
        let ste = scope.get(ste_h);
        let name = scope.get(&name_h);
        scope.set_field_by_name(ste, "moduleName", Value::Object(Some(name)));
        if let Some(h) = &version_h {
            let version = scope.get(h);
            scope.set_field_by_name(ste, "moduleVersion", Value::Object(Some(version)));
        }
    }
    // The loader's own `name` String, as HotSpot stores it; the bootstrap
    // loader (`null`) names nothing.
    if loader_id != 0 {
        let mirror = scope.get_class_mirror(class_id);
        // Through `Class.getClassLoader`'s native, not the mirror's
        // `classLoader` field: the VM leaves that field null for built-in
        // loaders (measured on w1a: `loader=null` for an application frame),
        // and the native answers the side table / app-loader singleton.
        let loader = match crate::lang_class::native_class_get_class_loader(
            &mut **scope,
            &[Value::Object(Some(mirror))],
        ) {
            Ok(Some(Value::Object(Some(loader)))) => Some(loader),
            _ => None,
        };
        if let Some(loader) = loader {
            if let Value::Object(Some(name)) =
                crate::classloader_real::loader_declared_name(&**scope, loader)
            {
                // Read after the mirror's allocation: nothing moved it since.
                let ste = scope.get(ste_h);
                scope.set_field_by_name(ste, "classLoaderName", Value::Object(Some(name)));
            }
        }
    }
    let ste = scope.get(ste_h);
    scope.set_field_by_name(ste, "format", Value::Int(format));
}

/// Round 14 wave 4 (lane trace3): the elements `Thread.dumpThreads` (behind
/// `Thread.getAllStackTraces()`) answers carry `format == 0`, as HotSpot's do:
/// its VM fills them (`java_lang_StackTraceElement::fill_in`) and no
/// `StackTraceElement.of` / `computeFormat` ever runs on them, so `toString()`
/// prints every origin field -- `java.base@25.0.3/java.lang.Object.wait0(..)`,
/// `app//Main.run(..)` -- where `Thread.getStackTrace()` prints
/// `java.base/...`. The full-origin fill stored `computeFormat`'s bits; this
/// clears them on one built array. Allocates nothing. Only when the
/// full-origin fields are stored (`CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN`);
/// `CRATONVM_DUMP_THREADS_UNFORMATTED_ELEMENTS=0` keeps the bits.
pub fn clear_dumped_stack_trace_element_formats(
    ctx: &mut dyn NativeContext,
    elements: ObjectRef,
) {
    if !stack_trace_element_full_origin(&*ctx)
        || ctx
            .resolve_field_index("java/lang/StackTraceElement", "format")
            .is_none()
        || !true
    {
        return;
    }
    for i in 0..ctx.array_length(elements) {
        if let Value::Object(Some(ste)) = ctx.get_array_element(elements, i) {
            ctx.set_field_by_name(ste, "format", Value::Int(0));
        }
    }
}

/// Helper: write Throwable.detailMessage on a Throwable subclass.
///
/// Real-JDK Throwable layout: slot 0 = `backtrace` (an internal Object
/// reference), slot 1 = `detailMessage`, slot 2 = `cause`. We resolve the
/// slot index once per VM (`NativeContext::throwable_field_index`)
/// and reuse it; falls back to name-based resolution if the cached index
/// is out of bounds for `this` (synthetic-stub layout). The previous
/// implementation also mirrored to slot 0 to support a now-removed
/// synthetic-stub layout — that mirror clobbered Throwable.backtrace with
/// a String reference and corrupted any downstream consumer that read
/// backtrace as an Object[].
pub fn write_throwable_detail_message(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    msg: Value,
) {
    write_throwable_field_cached(ctx, "detailMessage", this, msg);
}

/// Helper: write Throwable.cause on a Throwable subclass.
///
/// Real-JDK Throwable layout: `cause` is at slot 2. We resolve the slot
/// index once per VM and reuse it; falls back to name-based resolution on
/// it; falls back to name-based resolution on bounds mismatch. The
/// previous implementation also mirrored to slot 1 (the synthetic-stub
/// cause slot), but slot 1 in the real-JDK layout is `detailMessage` —
/// the mirror clobbered the message field whenever both helpers ran
/// (e.g. via `<init>(String, Throwable)`).
pub fn write_throwable_cause(ctx: &mut dyn NativeContext, this: ObjectRef, cause: Value) {
    if crate::nbflags().dbg_cause {
        let this_cls = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        let cause_desc = match cause {
            Value::Object(Some(c)) if c == this => "SELF".to_string(),
            Value::Object(Some(c)) => {
                let cn = ctx
                    .class_name_of_id(ctx.class_id_of_object(c))
                    .unwrap_or_default();
                format!("{cn} hash={}", ctx.identity_hash_code(c))
            }
            Value::Object(None) => "NULL".to_string(),
            other => format!("{other:?}"),
        };
        eprintln!(
            "CAUSE_DBG_WRITE this={this_cls} hash={} ptr={:?} cause={cause_desc}",
            ctx.identity_hash_code(this),
            this.as_ptr()
        );
    }
    write_throwable_field_cached(ctx, "cause", this, cause);

    // ES-FAIL-FAMILY-20260710 hunt: arm a dynamic write-watchpoint (see
    // `NativeContext::dbg_set_watch_cell`) on the cause slot right after
    // writing the self-referential "uninitialized" sentinel into it, for
    // the next constructed instance of a specific class (set via
    // `CRATONVM_DBG_WATCH_CAUSE_SELF=<slash-separated class name>`) — to
    // catch, with a full Rust backtrace, whatever later overwrites that
    // exact memory slot with something else. Re-arms on every matching
    // construction (last one wins), since we don't know in advance which
    // instance will end up being the one that's actually printed/observed.
    if let Value::Object(Some(c)) = cause {
        if c == this {
            if let Some(watch_cls) = crate::nbflags().dbg_watch_cause_self.as_deref() {
                let this_cls = ctx
                    .class_name_of_id(ctx.class_id_of_object(this))
                    .unwrap_or_default();
                if this_cls == watch_cls {
                    if let Some(idx) = ctx.throwable_field_index("cause") {
                        let addr = this.as_ptr() as usize
                            + cratonvm_types::HEADER_SIZE
                            + idx * cratonvm_types::SLOT_SIZE;
                        eprintln!(
                            "CAUSE_DBG_ARM watch={addr:#x} for {this_cls} hash={}",
                            ctx.identity_hash_code(this)
                        );
                        ctx.dbg_set_watch_cell(addr);
                    }
                }
            }
        }
    }
}

/// Pins `this` across [`capture_throwable_trace_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
///
/// This form has no error channel: what an application `fillInStackTrace()`
/// override throws is dropped here. The constructor shadows in this file use
/// [`capture_throwable_trace_ctor`], which propagates it out of `new` as
/// HotSpot does; the callers in `lib.rs` still take this form
/// (`r12w8-compat2-lib-rs-throwable-ctors-propagate-override-throw-patch`).
pub fn capture_throwable_trace(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let _ = capture_throwable_trace_ctor(ctx, this);
}

/// [`capture_throwable_trace`] for a constructor shadow that can fail: `Err`
/// is what an application `fillInStackTrace()` override threw, which the
/// shadow must return so `new` throws it (round 12 wave 8, lane compat2).
pub fn capture_throwable_trace_ctor(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if constructor_defers_to_fill_override(ctx, this)? {
        return Ok(());
    }
    capture_throwable_trace_with_mode(ctx, this, true);
    // SBR5 lane jfr: what the JDK's constructors do under `jfrTracing`.
    if crate::hooks::jfr_any_recording() {
        crate::hooks::jfr_throwable_created(ctx, *this);
    }
    Ok(())
}



/// Does a class outside the bootstrap and platform loaders, from `class_id`
/// up, declare `fillInStackTrace()`? A built-in class's superclasses are
/// built-in too, so the walk stops at the first one: `new IllegalStateException()`
/// pays one `loader_id_of_class`.
fn application_fill_override(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    // 0 bootstrap, 1 platform, 2 application, 3+ user-defined.
    if ctx.loader_id_of_class(class_id) < 2 {
        return false;
    }
    // A covariant override's `()Ljava/lang/Throwable;` is a bridge, which
    // `class_declares_method` does not count; the real method returns the
    // class itself or (round 12 wave 8) one of its ancestors below
    // `Throwable` -- `RuntimeException fillInStackTrace()` in a subclass of
    // it. So the chain's names are the candidate return types.
    let mut chain: Vec<(ClassId, Option<String>)> = Vec::new();
    let mut cursor = Some(class_id);
    while let Some(cid) = cursor {
        let name = ctx.class_name_of_id(cid);
        if chain.len() >= 256 || name.as_deref() == Some("java/lang/Throwable") {
            break;
        }
        chain.push((cid, name));
        cursor = ctx.superclass_of(cid);
    }
    // The application part of the chain: everything below the first built-in
    // class.
    let mut application_len = chain.len();
    for (depth, (cid, _)) in chain.iter().enumerate() {
        if ctx.loader_id_of_class(*cid) < 2 {
            application_len = depth;
            break;
        }
        if ctx.class_declares_method(*cid, "fillInStackTrace", "()Ljava/lang/Throwable;") {
            return true;
        }
        for (_, returned) in chain.iter().skip(depth) {
            let Some(returned) = returned else {
                continue;
            };
            if ctx.class_declares_method(*cid, "fillInStackTrace", &format!("()L{returned};")) {
                return true;
            }
        }
    }
    // Round 13 wave 1 (lane mhffm): a covariant override whose return type is
    // NOT on the chain (`public Error fillInStackTrace()` in a
    // `RuntimeException` subclass) is visible only through the
    // `()Ljava/lang/Throwable;` bridge javac emits next to it, which
    // `class_declares_method` skips. Scanned last, so the switch is read only
    // when this finds what the name checks above missed (before, the walk
    // answered `false` here).
    let bridged = chain[..application_len]
        .iter()
        .any(|(cid, _)| declares_fill_override_bridge(&ctx.declared_methods(*cid)));
    bridged
}

/// Does this method table carry an instance `fillInStackTrace()` returning
/// `Throwable` -- the bridge javac emits for a covariant override, or the
/// override itself? Split out of [`application_fill_override`] for its tests.
fn declares_fill_override_bridge(methods: &[cratonvm_native_api::MethodMetadata]) -> bool {
    const ACC_STATIC: u16 = 0x0008;
    methods.iter().any(|m| {
        m.name == "fillInStackTrace"
            && m.descriptor == "()Ljava/lang/Throwable;"
            && m.access_flags & ACC_STATIC == 0
    })
}



/// `Throwable.<init>` calls the VIRTUAL `fillInStackTrace()`: for a class
/// that overrides it, run the override instead of capturing, as the real
/// constructor would, and let it decide (`return this` records nothing; a
/// `super.fillInStackTrace()` reaches `fillInStackTrace(int)`, which captures
/// and trims the override's frame like any fill frame).
///
/// `Ok(true)` when the override ran (the caller must not capture), `Err` when
/// it threw and [`ctor_fill_override_exact`] is on.
fn constructor_defers_to_fill_override(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    let class_id = ctx.class_id_of_object(*this);
    if !application_fill_override(&*ctx, class_id) {
        return Ok(false);
    }
    // The field initialisers the real constructor has run by then.
    // `Throwable.fillInStackTrace()` fills only when `stackTrace != null ||
    // backtrace != null`, so a `super` call needs `stackTrace =
    // UNASSIGNED_STACK` (which `native_throwable_get_stack_trace_array` reads
    // as "not set", by identity).
    init_suppressed_sentinel_for_constructor(ctx, *this);
    if let Some(tid) = ctx.class_id_by_name("java/lang/Throwable") {
        if let Some(idx) = ctx.static_field_index_by_name(tid, "UNASSIGNED_STACK") {
            let unassigned = ctx.get_static_field(tid, idx);
            if matches!(unassigned, Value::Object(Some(_))) {
                throwable_field_set(ctx, *this, "stackTrace", unassigned);
            }
        }
    }
    if ctor_fill_override_exact() {
        run_fill_override_in_ctor_order(ctx, this)?;
        return Ok(true);
    }
    // Round 12 wave 7 behaviour (the kill switch below is off): the override
    // sees the message the shadow already wrote, and what it throws is dropped.
    let pin = ctx.pin_native_root(*this);
    let _ = ctx.invoke_virtual(*this, "fillInStackTrace", "()Ljava/lang/Throwable;", &[]);
    *this = ctx.read_native_pin(pin, *this);
    ctx.unpin_native_roots(pin);
    Ok(true)
}

/// `CRATONVM_THROWABLE_CTOR_FILL_OVERRIDE_EXACT` -- default ON (round 12 wave
/// 8, lane compat2, both modes). The override runs in `Throwable.<init>`'s
/// order and what it throws leaves `new`; `0` restores wave 7's call (message
/// already assigned, exception dropped). Read only for a class with an
/// application override, so it is not cached.
fn ctor_fill_override_exact() -> bool {
    true
}

/// Run `this`'s `fillInStackTrace()` override as `Throwable.<init>` does:
/// FIRST, with only the field initialisers applied (`detailMessage == null`,
/// `cause == this`). The shadows have already assigned the message and the
/// cause, so both are set aside across the call and put back afterwards,
/// which is what the constructor's own assignments after the call do. A
/// cause still at the `this` sentinel is not put back (no constructor assigns
/// it), so a cause the override itself set stays, as on HotSpot. An exception
/// the override throws is returned, for `new` to throw.
fn run_fill_override_in_ctor_order(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let message = read_throwable_field(ctx, *this, "detailMessage");
    let cause = read_throwable_field(ctx, *this, "cause");
    let restore_cause = !matches!(cause, Value::Object(Some(c)) if c == *this);
    let base = ctx.pin_native_root(*this);
    let message_pin = match message {
        Value::Object(Some(m)) => Some(ctx.pin_native_root(m)),
        _ => None,
    };
    let cause_pin = match cause {
        Value::Object(Some(c)) if restore_cause => Some(ctx.pin_native_root(c)),
        _ => None,
    };
    write_throwable_detail_message(ctx, *this, Value::Object(None));
    if restore_cause {
        write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
    }
    let outcome = ctx.invoke_virtual(*this, "fillInStackTrace", "()Ljava/lang/Throwable;", &[]);
    *this = ctx.read_native_pin(base, *this);
    let message = match (message, message_pin) {
        (Value::Object(Some(m)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, m))),
        (other, _) => other,
    };
    let cause = match (cause, cause_pin) {
        (Value::Object(Some(c)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, c))),
        (other, _) => other,
    };
    ctx.unpin_native_roots(base);
    write_throwable_detail_message(ctx, *this, message);
    if restore_cause {
        write_throwable_cause(ctx, *this, cause);
    }
    outcome.map(|_| ())
}

/// Refresh an existing throwable's trace. Unlike the constructor form, this
/// must preserve a populated suppressed-exception list.
pub fn refresh_throwable_trace(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    capture_throwable_trace_with_mode(ctx, this, false);
}

fn capture_throwable_trace_with_mode(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    fresh_constructor: bool,
) {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = capture_throwable_trace_body(ctx, *this, fresh_constructor);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Capture the current call stack for a freshly-constructed throwable.
///
/// These `native_exc_init_*` natives SHADOW the JDK `Throwable.<init>`
/// bytecode (registered per-class by `register_throwable_subclass_natives`).
/// The JDK constructor is the only thing that calls `fillInStackTrace()` —
/// so when our native replaces it, nothing records the stack trace, and a
/// later `printStackTrace()` / `getStackTrace()` (which read the VM-owned trace
/// store) come back empty. That hid the origin of every exception built via
/// `new SomeException(...)` bytecode.
///
/// We mirror `fillInStackTrace` here: capture the current frames into the
/// VM-owned trace store (keyed by the throwable's address), and set
/// the `backtrace`/`depth` fields so the real-JDK `getOurStackTrace()` path
/// also works for callers that hit it directly.
///
/// `pub` so the *other* exception-constructor natives in `lib.rs`
/// (`native_exception_init_msg` / `native_exception_init_empty`, registered
/// by `register_exception_extras_natives`, and the RKC16N ctor closures in
/// `register_essential_natives`) can route through the same capture logic.
/// Those natives once won the registry slot for ~50 exception subclasses and
/// left `getStackTrace()` empty without this capture. Since E43 (2026-08-13)
/// `register_exception_extras_natives` no longer registers constructors from
/// its list; `lib.rs`'s two ctor bodies now own only a handful of rows
/// (`NullPointerException(String)`, `UnsatisfiedLinkError`,
/// `VirtualMachineError`, `StreamCorruptedException()`), and the per-class
/// table in this file serves the rest.
pub fn capture_throwable_trace_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    fresh_constructor: bool,
) {
    // The identity hash is DIAGNOSTIC ONLY here -- the retained trace is keyed
    // by address (`SharedVm::store_throwable_stack_trace`). Minting it on every
    // capture was not free: under `--real-jdk` the real
    // `synchronized Throwable.fillInStackTrace()` reaches this body with `this`
    // THIN-LOCKED, and hashing a thin-locked object inflates its monitor -- a
    // `MonitorTable` insert and an `Arc<Monitor>` allocation per throwable.
    // `ThrowCost.java`'s `javaNewOnly` measured it.
    let dbg_sttrace = crate::nbflags().dbg_sttrace;
    let hash = if dbg_sttrace {
        ctx.identity_hash_code(this)
    } else {
        0
    };
    if dbg_sttrace {
        // The throwable's CLASS, not just its identity. Counting captures tells
        // you a workload throws a lot; only the class tells you what. Naming it
        // here is what separated "Quartz leaks memory" from "Quartz throws the
        // same exception 25,000 times" — see
        // known-issues/springboot/quartz-endpoint-web-jit-only-spin-loop-20260818.
        eprintln!("STTRACE_DBG_CTOR_CAP this={:?} hash={hash}", this.as_ptr());
    }
    // Cast: bounded by the frame count or `MaxJavaStackTraceDepth`; fits i32.
    let depth = ctx.capture_throwable_stack_trace(this) as i32;
    // `-XX:+ShowHiddenFrames`: the spun `LambdaForm` frames of the served
    // method-handle calls the throwable came through (may run Java, may move
    // `this`).
    let (this, depth) = if ctx.show_hidden_frames_flag() {
        add_spun_frames_to_capture(ctx, this, depth)
    } else {
        (this, depth)
    };
    if dbg_sttrace {
        // The VM hands back only the depth; read the retained frames back.
        let trace = ctx.get_throwable_stack_trace(this).unwrap_or_default();
        // The top frames of THIS throwable's own trace, in order, on one line.
        // Printing frames from a separate site and pairing them up afterwards
        // is not sound — captures from several threads interleave in the log,
        // and reading "the deepest frame" out of a merged group names a throw
        // site that never existed. One line per throwable cannot be mispaired.
        // Captured traces are OUTERMOST-first, so the throw site is the LAST
        // entry, not the first. Print both ends labelled — an unlabelled
        // "top=" that is really the thread entry point reads as a perfectly
        // plausible answer, which is how the first version of this line sent
        // the investigation at `TaskThread.run`.
        let fmt = |f: &cratonvm_native_api::registry::StackTraceEntry| {
            format!("{}.{}:{}", f.class_name, f.method_name, f.line_number)
        };
        let innermost: Vec<String> = trace.iter().rev().take(5).map(&fmt).collect();
        // Class AND frames on ONE line. They were two `eprintln!`s, and
        // `eprintln!` from several threads interleaves, so pairing "the last
        // class line" with "the next frame line" attributes frames to the
        // wrong throwable — the same unsound pairing this file already fixed
        // once, reintroduced by splitting the print. One line, one throwable.
        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_else(|| "?".to_string());
        eprintln!(
            "STTRACE_DBG_TOP hash={hash} class={cls} depth={depth} innermost={}",
            innermost.join(" <- ")
        );
    }
    // `getOurStackTrace()` only materialises frames when `backtrace != null`;
    // park a self-reference as the non-null marker (the real frame data lives
    // in the VM's address-keyed trace store).
    // These are fields declared by Throwable itself, so write the same
    // VM-local resolved slots used by the constructor message/cause helpers.
    // A subclass may declare a same-named field; by-name lookup on the
    // receiver would hit that shadow, whereas HotSpot updates Throwable's own
    // backtrace and depth. The helper falls back safely for synthetic layouts.
    // JIT round 16 (lane throwpath, TP16-2): one call for the stores and the
    // refresh's suppressed-list test where the VM can answer it
    // (`CRATONVM_THROWABLE_FILL_DIRECT_STORES=1`); a list already there means
    // the refresh writes nothing, exactly `init_suppressed_sentinel`'s fast
    // path. Every other case takes the per-field path below.
    if let Some(suppressed_present) = ctx.finish_throwable_fill(this, depth) {
        if fresh_constructor {
            init_suppressed_sentinel_for_constructor(ctx, this);
        } else if !suppressed_present {
            init_suppressed_sentinel(ctx, this);
        }
        return;
    }
    // Interpreter round i3 wave 82, lane L2
    // (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`): the capture stored
    // `backtrace` itself (a throwable-owned holder, or this marker); only a
    // `null` it could not write is marked here.
    if !ctx.throwable_backtrace_in_object()
        || !matches!(
            read_throwable_field(ctx, this, "backtrace"),
            Value::Object(Some(_))
        )
    {
        write_throwable_field_cached(ctx, "backtrace", this, Value::Object(Some(this)));
    }
    write_throwable_field_cached(ctx, "depth", this, Value::Int(depth));
    // A constructor receives a freshly zeroed object, so it can install the
    // sentinel directly. `fillInStackTrace` is a refresh and must first prove
    // it is not about to overwrite a user-populated suppression list.
    if fresh_constructor {
        init_suppressed_sentinel_for_constructor(ctx, this);
    } else {
        init_suppressed_sentinel(ctx, this);
    }
}



/// Under `-XX:+ShowHiddenFrames`, the trace just captured for `this` with the
/// spun `LambdaForm` frames of each served method-handle call in it, as a
/// `SHOW_HIDDEN_FRAMES` `StackWalker` walk lists them
/// (`lang_stackwalker::add_spun_lambda_form_frames`: the spread invoker and
/// `asType` adapter of a served `invokeWithArguments`, the spun linker of an
/// `invokeExact` site), stored back, and its new depth.
///
/// HotSpot lists them because its JDK body spun and ran those forms; the
/// served calls never made them, so they are asked of the JDK, which only
/// Java can do. This runs inside the capture native (`fillInStackTrace(int)`
/// or a constructor native), a native frame like the walk's, where Java may
/// run; the served call's own records (`invoke_with_arguments_adapters`) are
/// still live, as during a walk inside the target. The depth cap
/// (`MaxJavaStackTraceDepth`) counts the inserted frames, as HotSpot's does.
/// A capture made inside the step's own Java lists no spun frames (the step's
/// re-entry guard). Returns `this` read back from its pin.
fn add_spun_frames_to_capture(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    depth: i32,
) -> (ObjectRef, i32) {
    if !crate::lang_stackwalker::spun_lambda_form_steps_on()
        || !ctx.lists_hidden_standin_frames()
    {
        return (this, depth);
    }
    let Some(trace) = ctx.get_throwable_stack_trace(this) else {
        return (this, depth);
    };
    if trace.len() < 2 {
        return (this, depth);
    }
    // The steps read a walk's order, INNERMOST first.
    let innermost_first: Vec<_> = trace.into_iter().rev().collect();
    let key = |e: &cratonvm_native_api::registry::StackTraceEntry| {
        (
            std::sync::Arc::clone(&e.class_name),
            std::sync::Arc::clone(&e.method_name),
            e.byte_code_index,
        )
    };
    let before: Vec<_> = innermost_first.iter().map(key).collect();
    let pin = ctx.pin_native_root(this);
    let mut spliced = crate::lang_stackwalker::add_spun_lambda_form_frames(ctx, innermost_first);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    if spliced.iter().map(key).eq(before) {
        return (this, depth);
    }
    let max_depth = ctx.max_java_stack_trace_depth();
    if max_depth != 0 && spliced.len() > max_depth {
        spliced.truncate(max_depth); // the innermost frames stay
    }
    spliced.reverse();
    // Cast: bounded by the frame count or `MaxJavaStackTraceDepth`; fits i32.
    let new_depth = spliced.len() as i32;
    if ctx.replace_throwable_stack_trace(this, spliced) {
        (this, new_depth)
    } else {
        (this, depth)
    }
}

fn throwable_suppressed_sentinel(ctx: &mut dyn NativeContext) -> Option<Value> {
    ctx.throwable_suppressed_sentinel().or_else(|| {
        ctx.class_id_by_name("java/lang/Throwable").and_then(|cid| {
            ctx.static_field_index_by_name(cid, "SUPPRESSED_SENTINEL")
                .map(|idx| ctx.get_static_field(cid, idx))
        })
    })
}

/// Constructor-only fast path for `suppressedExceptions = SUPPRESSED_SENTINEL`.
/// The receiver is newly allocated, so it cannot already have a user list; the
/// slow refresh path below retains that check for `fillInStackTrace`.
fn init_suppressed_sentinel_for_constructor(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let Some(v @ Value::Object(Some(_))) = throwable_suppressed_sentinel(ctx) else {
        return;
    };
    if let Some(index) = cached_throwable_field_index(ctx, "suppressedExceptions") {
        if index < ctx.object_num_fields(this) {
            ctx.set_field(this, index, v);
            return;
        }
    }
    throwable_field_set(ctx, this, "suppressedExceptions", v);
}

/// Mirror the JDK `Throwable` instance-field initializer
/// `suppressedExceptions = SUPPRESSED_SENTINEL`.
///
/// Our `native_exc_init_*` natives SHADOW `Throwable.<init>`, so the real
/// field initializer never runs and `suppressedExceptions` stays `null`.
/// `Throwable.addSuppressed` treats a `null` list as "suppression disabled"
/// and silently drops every call, so `getSuppressed()` always returned an
/// empty array — e.g. Hibernate's `NamedQueryValidationException` aggregates
/// each invalid-query error via `addSuppressed`, and
/// `LoaderWithInvalidQueryTest` asserts `getSuppressed().length == 2`. This is
/// the suppressed-list sibling of the `cause = this` mirroring in
/// `write_throwable_cause`.
///
/// Only initializes when the field is still `null`, so a re-entrant
/// `fillInStackTrace()` (which also funnels through `capture_throwable_trace`)
/// never clobbers a list that `addSuppressed` already populated. Reading the
/// real static keeps `addSuppressed`/`getSuppressed`'s `== SUPPRESSED_SENTINEL`
/// identity checks valid.
fn init_suppressed_sentinel(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // Fast path: `Throwable`'s own slot, through the VM-cached index, already
    // holds a list. This is THE common case under `--real-jdk`, where the real
    // `Throwable.<init>` ran the field initialiser before its
    // `fillInStackTrace()` reached this refresh -- and the by-name read below
    // costs two hierarchy walks per throw to rediscover it. The slot is the
    // one `throwable_field_get` answers for every real-layout receiver
    // (`shadowed_throwable_slot` redirects a shadowing subclass TO it), and
    // the bounds test is the same one `write_throwable_field_cached` trusts.
    // Anything else -- a null, an unset slot, a synthetic layout -- takes the
    // unchanged path below.
    if let Some(index) = cached_throwable_field_index(ctx, "suppressedExceptions") {
        if index < ctx.object_num_fields(this)
            && matches!(ctx.get_field(this, index), Value::Object(Some(_)))
        {
            return;
        }
    }
    // Don't overwrite an already-initialized list — a populated `ArrayList`
    // from `addSuppressed`, or the sentinel itself (a re-entrant
    // `fillInStackTrace()` also funnels through `capture_throwable_trace`).
    // Skip ONLY when the field already holds a non-null object reference: an
    // unset slot reads `Int(0)` on a legacy object and `Object(None)` on a
    // compact one, and both mean "no list yet".
    let current = read_throwable_field(ctx, this, "suppressedExceptions");
    if let Value::Object(Some(_)) = current {
        return;
    }
    // JIT round 18 wave 10 (lane throws;
    // `r18-throws-refill-re-enables-disabled-suppression-FIXED-20261006.md`,
    // `CRATONVM_THROWABLE_REFILL_KEEPS_SUPPRESSION`, default ON, `=0` kill):
    // this runs only for a REFRESH (`fillInStackTrace()` on a built
    // throwable). A built throwable's `null` list is the JDK's own encoding
    // of "suppression disabled": the real constructor ran the field
    // initialiser and only the four-argument constructor nulls it (after its
    // own fill), and every constructor native installs the sentinel first.
    // HotSpot's `fillInStackTrace` never touches the list, so a refill must
    // not turn suppression back on (`addSuppressed` would then record what
    // HotSpot drops). Only an UNSET legacy slot (`Int(0)`, no verdict) is
    // still mirrored.
    if matches!(current, Value::Object(None)) && refill_keeps_suppression_enabled() {
        return;
    }
    let sentinel = throwable_suppressed_sentinel(ctx);
    // Only mirror once `Throwable.<clinit>` has populated the sentinel; before
    // that (bootstrap-era throwables) leave the field as-is.
    if let Some(v @ Value::Object(Some(_))) = sentinel {
        throwable_field_set(ctx, this, "suppressedExceptions", v);
    }
}

/// `CRATONVM_THROWABLE_REFILL_KEEPS_SUPPRESSION` -- default ON, `=0` restores
/// the old mirror (JIT round 18 wave 10, lane throws): may a refresh of a built
/// throwable leave a `null` suppressed list alone ([`init_suppressed_sentinel`])?
/// Read per call: only a refill of a throwable whose list is `null` (built with
/// suppression disabled) reaches it, so no process static is kept for it.
fn refill_keeps_suppression_enabled() -> bool {
    true
}

/// Whether `this` was built with suppression turned off — the JDK encodes that
/// as `suppressedExceptions == null`, written only by the four-arg protected
/// `Throwable(String, Throwable, boolean enableSuppression, boolean)`.
///
/// Every clause here exists to keep a *null we cannot explain* from being read
/// as a deliberate "disabled", because that reading makes `addSuppressed` a
/// silent no-op and try-with-resources loses the `close()` failure with nothing
/// looking wrong:
///
/// 1. The receiver's class must actually declare/inherit the field. A synthetic
///    stub that has no such field also answers `Object(None)` through
///    `read_throwable_field`'s slot fallback — that is a missing field, not a
///    disabled one.
/// 2. `Throwable.<clinit>` must have populated `SUPPRESSED_SENTINEL`. Before it
///    has, no throwable in the process can carry the sentinel, so a null field
///    means "too early to mirror the initialiser" rather than "disabled".
/// 3. Only `Object(None)` counts. An *unset* slot on a LEGACY object reads
///    back as `Int(0)`, which is again not a verdict. On a COMPACT object -- as
///    every real-layout throwable is -- an unset slot reads `Object(None)` and
///    so does count, which is HotSpot's answer too: a throwable no
///    constructor initialised (`Unsafe.allocateInstance`) has a null list
///    there, and `addSuppressed` drops (`NoCtorThrowableProbe`). Every
///    throwable the VM itself builds has the sentinel mirrored in first.
///
/// Each failing clause fails OPEN — append the suppressed exception — because
/// an extra entry in `getSuppressed()` is visible and a dropped one is not.
fn suppression_disabled(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(this);
    if ctx
        .resolve_field_index_by_class_id(class_id, "suppressedExceptions")
        .is_none()
    {
        return false;
    }
    if !matches!(
        throwable_field_get(ctx, this, "suppressedExceptions"),
        Value::Object(None)
    ) {
        return false;
    }
    let sentinel = ctx.class_id_by_name("java/lang/Throwable").and_then(|cid| {
        ctx.static_field_index_by_name(cid, "SUPPRESSED_SENTINEL")
            .map(|idx| ctx.get_static_field(cid, idx))
    });
    matches!(sentinel, Some(Value::Object(Some(_))))
}

/// Exception <init>(Ljava/lang/String;)V — sets detailMessage.
///
/// JDK semantics: `Throwable.cause` is declared `private Throwable cause = this;`
/// — a self-reference sentinel meaning "no cause set yet". A later
/// `initCause(c)` checks `cause != this` and throws `IllegalStateException`
/// ("Can't overwrite cause") otherwise. Because we shadow the JDK
/// `Throwable.<init>` with this native, we must mirror the field
/// initializer ourselves; without it, the sentinel stays null and the
/// real-JDK `initCause` bytecode (which still runs because we don't
/// shadow it on every dispatch path) treats the field as already-set.
pub fn native_exc_init_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(msg) = args.get(1) {
            write_throwable_detail_message(ctx, *this, *msg);
        }
        // Initialize cause to self-sentinel so a later initCause() succeeds.
        write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

/// `ExceptionInInitializerError(Throwable)` -- `super(null, thrown)`.
///
/// See the class-specific row in [`throwable_ctor_native`] for the measured
/// bytecode. The point of a separate body rather than the generic
/// `(Ljava/lang/Throwable;)V` arm is the MESSAGE: `Throwable(Throwable)`
/// derives `detailMessage` from `cause.toString()`, and this class passes an
/// explicit null one.
pub fn native_exception_in_initializer_init_thrown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(v @ Value::Object(Some(_))) => *v,
        _ => return Ok(None),
    };
    let thrown = args.get(1).copied().unwrap_or(Value::Object(None));
    native_exc_init_message_cause(ctx, &[this, Value::Object(None), thrown])
}

/// A no-arg constructor that leaves `cause` at a REAL null rather than at the
/// JDK's `cause == this` sentinel -- so a later `initCause` on the instance is
/// an `IllegalStateException` and not a success.
///
/// Three classes in [`THROWABLE_FAMILY_CLASSES`] do this, and the generic
/// [`native_exc_init_noargs`] arm got all three wrong by writing the sentinel.
/// Disassembled on the 25.0.4+7 image, all 62 classes in the list:
///
/// ```text
///   ClassNotFoundException()        aconst_null; checkcast Throwable;
///                                   invokespecial ReflectiveOperationException.<init>(Throwable)
///   InvocationTargetException()     the same, then `target = null`
///   ExceptionInInitializerError()   super(); aconst_null; invokevirtual initCause
/// ```
///
/// Every other class either has no no-arg constructor or reaches
/// `Throwable()`, which assigns `cause = this`. `Error()` and `Throwable()`
/// contain an `aconst_null` of their own -- the `null` message argument to
/// `ThrowableTracer.trace*` under the `jfrTracing` flag -- which is why the
/// discriminator has to be the SUPER CALL and not the presence of a null.
///
/// MEASURED by `ThrowableFamilySweep.java`:
/// `new ClassNotFoundException().initCause(new Error("later"))` is an
/// `IllegalStateException` on HotSpot and was a silent success here.
pub fn native_exc_init_noargs_null_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_exc_init_noargs(ctx, args)?;
    if let Some(Value::Object(Some(this))) = args.first() {
        write_throwable_cause(ctx, *this, Value::Object(None));
    }
    Ok(None)
}

/// The `(Ljava/lang/String;)V` sibling of
/// [`native_exc_init_noargs_null_cause`]: sets the message and leaves `cause`
/// at a real null.
///
/// `ClassNotFoundException(String)` and `ExceptionInInitializerError(String)`
/// are both `aload_1; aconst_null; invokespecial super.<init>(String,Throwable)`
/// on the 25.0.4+7 image, so they own the same refusal as their no-arg forms:
/// `new ClassNotFoundException("c").initCause(x)` is an `IllegalStateException`
/// on HotSpot and was a silent success here. MEASURED by
/// `ThrowableFamilySweep.java`.
pub fn native_exc_init_message_null_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(v @ Value::Object(Some(_))) => *v,
        _ => return Ok(None),
    };
    let msg = args.get(1).copied().unwrap_or(Value::Object(None));
    native_exc_init_message_cause(ctx, &[this, msg, Value::Object(None)])
}

/// Exception <init>(Ljava/lang/String;Ljava/lang/Throwable;)V
pub fn native_exc_init_message_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(msg) = args.get(1) {
            write_throwable_detail_message(ctx, *this, *msg);
        }
        if let Some(cause) = args.get(2) {
            write_throwable_cause(ctx, *this, *cause);
        }
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

/// Exception <init>(Ljava/lang/Throwable;)V — sets cause AND detailMessage.
///
/// JDK semantics (`java.lang.Throwable(Throwable cause)`):
/// ```text
///     fillInStackTrace();
///     detailMessage = (cause == null ? null : cause.toString());
///     this.cause = cause;
/// ```
/// The `detailMessage = cause.toString()` step is NOT optional — it is the
/// whole reason a cause-only constructor produces a non-null `getMessage()`.
/// Omitting it left `detailMessage` null. `native_throwable_get_message`
/// then fell back to reading raw slot 0 (which `capture_throwable_trace`
/// populates with a self-reference as the `backtrace` non-null marker), so
/// `getMessage()` returned the *exception object itself*. Any caller doing
/// `String msg = ex.getMessage(); msg.contains(...)` then dispatched
/// `String.contains` against the exception's runtime class and crashed with
/// `NoSuchMethodError: <ExceptionClass>.contains(Ljava/lang/CharSequence;)Z`
/// — the canonical Spring Boot `throw new IllegalStateException(cause)`
/// rewrap path. Mirroring the JDK initializer here keeps `detailMessage`
/// a real `String`.
pub fn native_exc_init_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(this))), Some(cause)) = (args.first(), args.get(1)) {
        write_throwable_cause(ctx, *this, *cause);
        let mut this = *this;
        // `Throwable(Throwable)` runs `fillInStackTrace()` FIRST and
        // `cause.toString()` after (round 12 wave 8 orchestrator,
        // `r12w8-compat2-throwable-fill-override-residuals` item 2): an
        // override and a `toString()` that observe each other see HotSpot's
        // order. The cause is read back from the field, which a moving
        // collection updates (the `args` copy it does not).
        // `CRATONVM_THROWABLE_CAUSE_CTOR_FILL_FIRST=0` restores message first.
        let fill_first = true;
        if fill_first {
            capture_throwable_trace_ctor(ctx, &mut this)?;
        }
        let cause = if fill_first {
            read_throwable_field(ctx, this, "cause")
        } else {
            *cause
        };
        // gen r4w3/rooting: `cause.toString()` below is a Java upcall (GC
        // point) and `this` is written/captured after it; pin it across.
        let this_pin = ctx.pin_native_root(this);
        // detailMessage = (cause == null ? null : cause.toString())
        let detail_msg: Value = match cause {
            Value::Object(Some(cause_ref)) if cause_ref != this => {
                match ctx.invoke_virtual(cause_ref, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(v @ Value::Object(Some(_)))) => v,
                    // toString returned null / non-object, or dispatch failed:
                    // leave detailMessage null rather than smuggle a bad value.
                    _ => Value::Object(None),
                }
            }
            _ => Value::Object(None),
        };
        // gen r4w3/rooting: the post-`toString` address.
        let mut this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        write_throwable_detail_message(ctx, this, detail_msg);
        if !fill_first {
            capture_throwable_trace_ctor(ctx, &mut this)?;
        }
    }
    Ok(None)
}



// ---------------------------------------------------------------------------
// The constructors the blanket four-descriptor set did not cover
// ---------------------------------------------------------------------------
//
// Every message below is the string JDK 25 actually produces, measured by
// constructing the real exception and reading `getMessage()`
// (`probes/ThrowableCtorCensusProbe.java`'s sibling run) — not inferred from
// the javadoc. Where the JDK's message is a format ("Index out of range: 3"),
// reproducing it matters: these are the strings that end up in a user's log.

/// `AssertionError(Object)` — the single most reachable constructor in the
/// census, and the one that was missing everywhere.
///
/// `AssertionError`'s `(String)V` is **private** in the JDK, so BOTH
/// `throw new AssertionError(msg)` and `assert cond : msg` compile to
/// `<init>:(Ljava/lang/Object;)V`. Neither the registry nor the synthetic stub
/// declared it, so in synthetic-JDK mode the error path of anything using
/// `assert` raised `NoSuchMethodError` — replacing the assertion's own message
/// with a dispatch failure at exactly the moment the program was trying to
/// report what went wrong.
///
/// JDK semantics (`AssertionError(Object detailMessage)`):
/// ```text
///     this(String.valueOf(detailMessage));
///     if (detailMessage instanceof Throwable)
///         initCause((Throwable) detailMessage);
/// ```
/// Measured: `new AssertionError((Object) null)` → message `null` (the JDK
/// stores `String.valueOf(null)`, i.e. the four-character string "null" —
/// `getMessage()` reports it as such), and a `Throwable` argument yields both
/// `getMessage() == cause.toString()` and `getCause() == cause`.
pub fn native_assertion_error_init_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(mut this))) = args.first().copied() else {
        return Ok(None);
    };
    let mut detail = args.get(1).copied().unwrap_or(Value::Object(None));
    // `String.valueOf(Object)` — through the JDK so a custom `toString()` is
    // honoured, exactly as the real constructor does.
    //
    // GC-safety: this is the `assert` failure path, so it runs at whatever
    // allocation pressure the program had reached, and the `toString()`
    // bytecode below allocates and can relocate BOTH `this` and the argument.
    // Pin the two across the call and REBIND them to the forwarded references —
    // every write below targets `this`, and the Throwable-cause test below
    // reads `detail`, so a stale local here corrupts the object it is building.
    let message = match detail {
        Value::Object(Some(obj)) => {
            let this_pin = ctx.pin_native_root(this);
            let detail_pin = ctx.pin_native_root(obj);
            let rendered = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
            this = ctx.read_native_pin(this_pin, this);
            detail = Value::Object(Some(ctx.read_native_pin(detail_pin, obj)));
            // Releases from `this_pin` onward, i.e. both handles.
            ctx.unpin_native_roots(this_pin);
            match rendered {
                Ok(Some(v @ Value::Object(Some(_)))) => v,
                _ => Value::Object(None),
            }
        }
        // `String.valueOf((Object) null)` is the STRING "null", not a null ref.
        _ => Value::Object(Some(ctx.create_string("null"))),
    };
    write_throwable_detail_message(ctx, this, message);
    // A Throwable argument becomes the cause as well as the message.
    let cause_is_throwable = match detail {
        Value::Object(Some(obj)) => {
            let obj_class = ctx.class_id_of_object(obj);
            ctx.class_id_by_name("java/lang/Throwable")
                .is_some_and(|throwable| {
                    obj_class == throwable || ctx.is_subclass(obj_class, throwable)
                })
        }
        _ => false,
    };
    if cause_is_throwable {
        write_throwable_cause(ctx, this, detail);
    } else {
        // Self-sentinel, so a later `initCause()` still succeeds — the same
        // contract `native_exc_init_message` maintains.
        write_throwable_cause(ctx, this, Value::Object(Some(this)));
    }
    capture_throwable_trace_ctor(ctx, &mut this)?;
    Ok(None)
}

/// Which primitive an `AssertionError(<primitive>)` overload was handed, so the
/// message is `String.valueOf` of the RIGHT type: the interpreter delivers a
/// `boolean`, a `char` and an `int` all as `Value::Int`, and
/// `String.valueOf(true)` is "true" where `String.valueOf(1)` is "1".
#[derive(Clone, Copy)]
pub enum ScalarKind {
    Boolean,
    Char,
    Int,
    Long,
    Float,
    Double,
}

fn scalar_to_string(kind: ScalarKind, value: Option<&Value>) -> String {
    match (kind, value) {
        (ScalarKind::Boolean, Some(Value::Int(v))) => (*v != 0).to_string(),
        (ScalarKind::Char, Some(Value::Int(v))) => char::from_u32(*v as u32)
            .map(|c| c.to_string())
            .unwrap_or_default(),
        (ScalarKind::Int, Some(Value::Int(v))) => v.to_string(),
        (ScalarKind::Long, Some(Value::Long(v))) => v.to_string(),
        (ScalarKind::Long, Some(Value::Int(v))) => (*v as i64).to_string(),
        // Java's float/double text rules are NOT Rust's `to_string`:
        // `Float.toString(0.1f)` is "0.1", but widening that f32 to f64 and
        // printing gives "0.10000000149011612", and `Double.toString(1e7)` is
        // "1.0E7" where Rust prints "10000000". `format_float`/`format_double`
        // in `lang_string` ARE the in-tree implementations of Java's rules
        // (both delegate to the shared `cratonvm_types` formatter); a second
        // local copy here would drift away from them.
        (ScalarKind::Float, Some(Value::Float(v))) => crate::lang_string::format_float(*v),
        (ScalarKind::Double, Some(Value::Double(v))) => crate::lang_string::format_double(*v),
        _ => String::new(),
    }
}

fn assertion_error_init_scalar(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: ScalarKind,
) -> MethodCallResult {
    if let Some(Value::Object(Some(mut this))) = args.first().copied() {
        let text = scalar_to_string(kind, args.get(1));
        // Uninterned, like every `String.valueOf` native in `lang_string`: the
        // JDK hands back a fresh String, so `new AssertionError(42).getMessage()
        // == "42"` is false, and interning one object per distinct value would
        // pin an unbounded set of messages in the intern table forever.
        let message = ctx.create_string_uninterned(&text);
        write_throwable_detail_message(ctx, this, Value::Object(Some(message)));
        write_throwable_cause(ctx, this, Value::Object(Some(this)));
        capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

/// `IndexOutOfBoundsException(int|long)` and its two subclasses, whose messages
/// differ by one word each — measured on JDK 25:
///
/// ```text
///   new IndexOutOfBoundsException(3)        Index out of range: 3
///   new ArrayIndexOutOfBoundsException(3)   Array index out of range: 3
///   new StringIndexOutOfBoundsException(3)  String index out of range: 3
/// ```
fn index_exception_init_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    prefix: &str,
) -> MethodCallResult {
    if let Some(Value::Object(Some(mut this))) = args.first().copied() {
        let index = match args.get(1) {
            Some(Value::Int(v)) => i64::from(*v),
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        // Uninterned: one distinct message per index, and the JDK's is a fresh
        // String built by concatenation, never an interned constant.
        let message = ctx.create_string_uninterned(&format!("{prefix}{index}"));
        write_throwable_detail_message(ctx, this, Value::Object(Some(message)));
        write_throwable_cause(ctx, this, Value::Object(Some(this)));
        capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

/// `UncheckedIOException(IOException)` — message is the cause's `toString()`,
/// and a null cause is a `NullPointerException` (`Objects.requireNonNull`),
/// not a silently message-less exception. Measured.
pub fn native_unchecked_io_init_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("cause".to_string()),
        }
        .into());
    }
    native_exc_init_cause(ctx, args)
}

/// `UncheckedIOException(String, IOException)` — same null-cause refusal, with
/// the caller's own message.
pub fn native_unchecked_io_init_message_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !matches!(args.get(2), Some(Value::Object(Some(_)))) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("cause".to_string()),
        }
        .into());
    }
    native_exc_init_message_cause(ctx, args)
}

/// `ParseException(String, int)` — message is the string; the offset also goes
/// to the JDK's `errorOffset` field.
///
/// NOTE the boundary: this constructor is what was missing, and it is what this
/// change fixes. `getErrorOffset()` is a separate method on a separate surface —
/// the write below reaches it only if the class model declares the field, and
/// nothing here fabricates the accessor.
pub fn native_parse_exception_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(mut this))) = args.first().copied() {
        let msg = args.get(1).copied().unwrap_or(Value::Object(None));
        write_throwable_detail_message(ctx, this, msg);
        write_throwable_cause(ctx, this, Value::Object(Some(this)));
        capture_throwable_trace_ctor(ctx, &mut this)?;
        // `super(s); this.errorOffset = errorOffset;`: after the fill, so an
        // overriding `fillInStackTrace()` sees 0 as it does on HotSpot.
        if let Some(offset @ Value::Int(_)) = args.get(2) {
            ctx.set_field_by_name(this, "errorOffset", *offset);
        }
    }
    Ok(None)
}

/// `MissingResourceException(String s, String className, String key)` — the
/// message is `s`; `className`/`key` are the JDK's own named fields. Same
/// accessor boundary as `ParseException` above.
pub fn native_missing_resource_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(mut this))) = args.first().copied() {
        let msg = args.get(1).copied().unwrap_or(Value::Object(None));
        write_throwable_detail_message(ctx, this, msg);
        write_throwable_cause(ctx, this, Value::Object(Some(this)));
        let mut fields = Vec::with_capacity(2);
        if let Some(class_name) = args.get(2) {
            fields.push(("className", *class_name));
        }
        if let Some(key) = args.get(3) {
            fields.push(("key", *key));
        }
        capture_then_assign_subclass_fields(ctx, &mut this, &fields)?;
    }
    Ok(None)
}

/// A throwable subclass constructor's own fields are assigned AFTER
/// `super(..)` returns, so after `fillInStackTrace()` ran: an application
/// override reading `getTargetException()` / `getClassName()` sees `null`, as
/// on HotSpot (round 12 wave 8 orchestrator,
/// `r12w8-compat2-throwable-fill-override-residuals` item 3). Reference values
/// are pinned across the fill, which can run Java.
/// `CRATONVM_THROWABLE_SUBCLASS_FIELDS_AFTER_FILL=0` assigns them first again.
fn capture_then_assign_subclass_fields(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    fields: &[(&str, Value)],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let mut base = None;
    let pins: Vec<Option<usize>> = fields
        .iter()
        .map(|(_, value)| match value {
            Value::Object(Some(obj)) => {
                let handle = ctx.pin_native_root(*obj);
                base.get_or_insert(handle);
                Some(handle)
            }
            _ => None,
        })
        .collect();
    let filled = capture_throwable_trace_ctor(ctx, this);
    if filled.is_ok() {
        for ((name, value), pin) in fields.iter().zip(&pins) {
            let value = match (value, pin) {
                (Value::Object(Some(obj)), Some(handle)) => {
                    Value::Object(Some(ctx.read_native_pin(*handle, *obj)))
                }
                _ => *value,
            };
            ctx.set_field_by_name(*this, name, value);
        }
    }
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
    filled
}

/// `TypeNotPresentException(String typeName, Throwable t)` — the message is
/// NOT the raw `typeName` (what the generic `(String,Throwable)` arm would
/// write). `javap -c java.lang.TypeNotPresentException` on the 25.0.3+9
/// image:
///
/// ```text
///   aload_0
///   new java/lang/StringBuilder
///   ...
///   ldc "Type "
///   aload_1            // typeName
///   ldc " not present"
///   invokespecial Throwable.<init>(String, Throwable)
///   aload_0
///   aload_1
///   putfield typeName
/// ```
///
/// so the built message is `"Type " + typeName + " not present"`, and
/// `typeName` is also stored verbatim in the class's own field for
/// `getTypeName()`. MEASURED, `H22-1` §5:
/// `new TypeNotPresentException("T", null).getMessage()` was `"T"` here where
/// HotSpot gives `"Type T not present"`.
pub fn native_type_not_present_exception_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let type_name_val = args.get(1).copied().unwrap_or(Value::Object(None));
        let type_name = match type_name_val {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        // gc-common w19-e: the message's allocation can collect; the receiver,
        // the type name and the cause (all from `args`) were stored through
        // their addresses from before it. Rooted and re-read.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let type_name_h = w19e_root_value(&mut scope, type_name_val);
        let cause = args.get(2).copied();
        let cause_h = cause.and_then(|c| w19e_root_value(&mut scope, c));
        let message = scope.create_string_uninterned(&format!("Type {type_name} not present"));
        let mut this = scope.get(&this_h);
        write_throwable_detail_message(&mut *scope, this, Value::Object(Some(message)));
        if let Some(cause) = cause {
            let cause = w19e_rooted_value(&scope, cause, cause_h.as_ref());
            write_throwable_cause(&mut *scope, this, cause);
        }
        let type_name_val = w19e_rooted_value(&scope, type_name_val, type_name_h.as_ref());
        scope.set_field_by_name(this, "typeName", type_name_val);
        capture_throwable_trace_ctor(&mut *scope, &mut this)?;
    }
    Ok(None)
}

/// InvocationTargetException(Throwable) — store the wrapped throwable in the
/// JDK `target` field rather than the inherited Throwable `cause` field.
pub fn native_invocation_target_exception_init_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let target = args.get(1).cloned().unwrap_or(Value::Object(None));
        write_throwable_cause(ctx, *this, Value::Object(None));
        write_throwable_detail_message(ctx, *this, Value::Object(None));
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_then_assign_subclass_fields(ctx, &mut this, &[("target", target)])?;
    }
    Ok(None)
}

/// InvocationTargetException(Throwable,String) — same target-field semantics,
/// with an explicit detail message.
pub fn native_invocation_target_exception_init_target_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let target = args.get(1).cloned().unwrap_or(Value::Object(None));
        let msg = args.get(2).cloned().unwrap_or(Value::Object(None));
        write_throwable_cause(ctx, *this, Value::Object(None));
        write_throwable_detail_message(ctx, *this, msg);
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_then_assign_subclass_fields(ctx, &mut this, &[("target", target)])?;
    }
    Ok(None)
}

/// InvocationTargetException.getCause()/getTargetException() — both return
/// the wrapped target throwable, independent of the inherited Throwable.cause.
pub fn native_invocation_target_exception_get_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = ctx.get_field_by_name(this, "target");
    match target {
        Value::Object(Some(target_obj)) if target_obj == this => Ok(Some(Value::Object(None))),
        Value::Object(_) => Ok(Some(target)),
        _ => {
            let cause = throwable_field_get(ctx, this, "cause");
            match cause {
                Value::Object(Some(cause_obj)) if cause_obj == this => {
                    Ok(Some(Value::Object(None)))
                }
                Value::Object(_) => Ok(Some(cause)),
                _ => Ok(Some(Value::Object(None))),
            }
        }
    }
}

/// Exception <init>()V — no message, no cause. Mirrors the JDK
/// `cause = this` sentinel so later `initCause()` calls succeed.
pub fn native_exc_init_noargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// java.lang.Throwable natives
// ---------------------------------------------------------------------------

pub fn native_throwable_fill_in_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (the Throwable), args[1] = dummy int
    let mut this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("fillInStackTrace on null".to_string()),
            }
            .into());
        }
    };

    refresh_throwable_trace(ctx, &mut this);

    // Return `this` (Throwable.fillInStackTrace returns the Throwable itself)
    Ok(Some(Value::Object(Some(this))))
}

/// `StackTraceElement.initStackTraceElements([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V`
///
/// Real-JDK `Throwable.getOurStackTrace()` allocates a `StackTraceElement[]`
/// of length `depth` and hands it, the opaque `backtrace` object, and `depth`
/// to this native to populate. Our `backtrace` marker is the throwable
/// itself, so we look up its captured trace (the VM-wide store keyed by the
/// throwable object) and fill each STE. Previously registered as a no-op, so
/// real-JDK `printStackTrace()` / `getStackTrace()` produced empty traces.
pub fn native_init_stack_trace_elements(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let elements = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let backtrace = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    fill_stack_trace_elements_from_backtrace(ctx, elements, Some(backtrace))}

/// The dotted class name [`captured_frame_dotted_name`] answers: the shared
/// cache's `Arc` (the by-name path) or text derived from the frame's name.
enum CapturedDotted {
    Shared(std::sync::Arc<str>),
    Text(String),
}

impl std::ops::Deref for CapturedDotted {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            CapturedDotted::Shared(s) => &**s,
            CapturedDotted::Text(s) => s.as_str(),
        }
    }
}

/// The `declaringClass` text a `StackTraceElement[]` fill gives a captured
/// frame of internal name `cls_slashed`.
///
/// The by-name form (`from_text == false`, the behaviour before wave 76):
/// the class looked up by name, then the process-wide dotted-name cache
/// (`lang_class::dotted_class_name`) -- whose value for a found class is
/// `dotted_binary_name` of that same name, or the name itself when it has no
/// `/` -- and for a class not found by name, [`trace_binary_name`]. Both
/// answers are `dotted_binary_name(cls_slashed)` except for a hidden class's
/// `/0x…` name under `CRATONVM_STACK_TRACE_BINARY_NAMES=0`.
///
/// `from_text` (`CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT`, interpreter round
/// i3 wave 76, lane L2): that text directly, without the class manager's
/// lock, the name hash and the global cache probe the lookup cost per
/// element; a name whose binary form keeps a `/` (a hidden class) takes the
/// by-name form, so every configuration answers the same text.
fn captured_frame_dotted_name(
    ctx: &dyn NativeContext,
    from_text: bool,
    cls_slashed: &str,
) -> CapturedDotted {
    if from_text {
        let dotted = crate::lang_class::dotted_binary_name(cls_slashed);
        if !dotted.contains('/') {
            return CapturedDotted::Text(dotted);
        }
    }
    match ctx.class_id_by_name(cls_slashed) {
        Some(cid) => CapturedDotted::Shared(crate::lang_class::dotted_class_name(
            ctx.vm_identity(),
            cid,
            cls_slashed,
        )),
        // Round 15 wave 2 (lane trace2): a hidden class keeps its `/0x…`.
        None => CapturedDotted::Text(trace_binary_name(
            ctx.stack_trace_fill_switches().binary_names,
            cls_slashed,
        )),
    }
}

/// [`native_init_stack_trace_elements`]'s body: fill `elements` (a
/// `StackTraceElement[]`) from the trace captured for `backtrace`, innermost
/// frame first, each slot a freshly allocated element; a slot the trace does
/// not reach (or every slot, for `None`) gets the `(unknown)` placeholder.
/// Shared with [`native_stack_trace_element_of_backtrace`] (interpreter round
/// i3 wave 74, lane L2), which hands it an array of nulls.
fn fill_stack_trace_elements_from_backtrace(
    ctx: &mut dyn NativeContext,
    elements: ObjectRef,
    backtrace: Option<ObjectRef>,
) -> MethodCallResult {
    // The captured trace, read once (an owned copy, its lines resolved).
    // Iterated in place below: interpreter round i3 wave 76, lane L2 dropped
    // the second, tuple-by-tuple `Arc` copy this took "to release the
    // borrow" -- the answer is already owned, so nothing is borrowed.
    // Keyed by the object, not its identity hash: see
    // `NativeExceptionAccess::get_throwable_stack_trace`.
    // THROWS6-2 (`CRATONVM_DBG_WALK_SECTIONS`): the section marks.
    let started =
        crate::lang_stackwalker::walk_sections_census_enabled().then(std::time::Instant::now);
    let trace_data = match backtrace {
        Some(backtrace) => ctx.get_throwable_stack_trace(backtrace).unwrap_or_default(),
        None => Vec::new(),
    };
    let looked_up = started.map(|_| std::time::Instant::now());
    // WALK8-2: the build's parts per element, from here (census only).
    let sub_build = looked_up.map(crate::lang_stackwalker::SteSubBuild::begin);
    use crate::lang_stackwalker::{ste_sub_mark, SteSub};
    let name_from_text = ctx.stack_trace_fill_name_from_text();

    let cap = ctx.array_length(elements);
    if crate::nbflags().dbg_sttrace {
        eprintln!(
            "[STTRACE init] cap={cap} trace_data.len={}",
            trace_data.len()
        );
        for (i, e) in trace_data.iter().rev().take(cap).enumerate() {
            eprintln!("[STTRACE init]   [{i}] {}.{}", e.class_name, e.method_name);
        }
    }
    // `Throwable.getStackTrace()` requires index 0 = the most-recent (innermost)
    // frame — the throw site. The stored trace is **outermost-first** (`main`
    // first): that is the order `capture_stack_trace` documents and the order
    // the StackWalker / `Reflection.getCallerClass` consumers rely on, so we do
    // NOT change the capture. Reverse only here, when materialising the
    // user-facing `StackTraceElement[]`, so it matches the JDK (throw site at
    // [0], `main` last) instead of being upside-down.
    // GC-safety: every element allocation, and the strings and mirror
    // `fill_stack_trace_element` allocates, can collect. `fill_stack_trace_element`
    // roots its own copy of `ste`, but the address this loop then stored into
    // the array was the pre-collection one, and so was `elements`. Both are
    // rooted here and read back at the store — the discipline
    // `native_throwable_get_stack_trace_array` already follows.
    let mut scope = NativeHandleScope::new(ctx);
    let elements_h = scope.root(elements);
    let memo_on = scope.stack_trace_fill_switches().origin_memo;
    let mut origin_memo = SteOriginMemo::new(&elements_h, memo_on);
    let mut filled = 0usize;
    // WALK11-1 (`CRATONVM_STACK_TRACE_ELEMENT_FRAME_COPY`): (exact class id,
    // internal name, method, file) -> index of the element filled for it.
    // Only over the real JDK layout (the copied fields are named).
    let mut frame_copy: Option<SteFrameCopyRows<'_>> = (stack_trace_element_frame_copy_enabled()
        && trace_data.len() > 1
        && cap > 1
        && origin_memo.real_layout(&mut *scope))
    .then(std::collections::HashMap::new);
    // The element class's layout is resolved once for the whole array
    // (interpreter round i1 wave 49, lane L7), not once per element.
    let ste_allocator = if trace_data.is_empty() || cap == 0 {
        None
    } else {
        Some(
            crate::util_concurrent_ext::ConcurrentSyntheticAllocator::new(
                &mut *scope,
                "java/lang/StackTraceElement",
                4,
            )?,
        )
    };
    for (i, entry) in trace_data.iter().rev().take(cap).enumerate() {
        let Some(allocator) = &ste_allocator else {
            break;
        };
        // gcg g6/oo (door 4 of `gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`):
        // a refused element collects and retries before the catchable
        // `OutOfMemoryError`, as HotSpot's `StackTraceElement` allocation does.
        // Safe: what the loop uses after the call is rooted in `scope` or is
        // Rust data.
        let ste = allocator.alloc_reclaiming(&mut *scope, "java/lang/StackTraceElement")?;
        let ste_h = scope.root(ste);
        ste_sub_mark(SteSub::Alloc);
        let cls_slashed: &str = &entry.class_name;
        // WALK11-1 (`CRATONVM_STACK_TRACE_ELEMENT_FRAME_COPY`): a frame whose
        // (exact class, name, method, file) an earlier element of this array
        // already has copies that element's fields with its own line.
        let copy_key = frame_copy.as_ref().and(entry.class_id).map(|cid| {
            (
                cid,
                cls_slashed,
                &*entry.method_name,
                entry.source_file.as_deref(),
            )
        });
        if let Some(source) = copy_key.and_then(|key| frame_copy.as_ref()?.get(&key).copied()) {
            let elements = scope.get(&elements_h);
            if let Value::Object(Some(source)) = scope.get_array_element(elements, source) {
                let ste = scope.get(&ste_h);
                copy_stack_trace_element_frame(&scope, source, ste, entry.line_number);
                scope.set_array_element(elements, i, Value::Object(Some(ste)));
                ste_sub_mark(SteSub::Store);
                filled = i + 1;
                continue;
            }
        }
        let cls_dotted = captured_frame_dotted_name(&*scope, name_from_text, cls_slashed);
        ste_sub_mark(SteSub::Name);
        origin_memo.at(i);
        fill_captured_stack_trace_element_memo(
            &mut *scope,
            ste,
            entry.class_id,
            cls_slashed,
            &cls_dotted,
            &entry.method_name,
            entry.source_file.as_deref(),
            entry.line_number,
            &mut origin_memo,
        );
        let elements = scope.get(&elements_h);
        let ste = scope.get(&ste_h);
        scope.set_array_element(elements, i, Value::Object(Some(ste)));
        ste_sub_mark(SteSub::Store);
        filled = i + 1;
        // WALK11-1: later frames of this key copy this element.
        if let (Some(rows), Some(key)) = (frame_copy.as_mut(), copy_key) {
            rows.entry(key).or_insert(i);
        }
    }
    // WALK8-2: the backfill below is not an element build of the trace.
    drop(sub_build);
    // ES-FAIL-06 (root): `StackTraceElement.of(x, depth)` pre-fills the array
    // with empty `new StackTraceElement()` objects (null `declaringClass`).
    // `depth` comes from `getStackTraceDepth()` (keyed on the throwable), but
    // this native looks the trace up by the `backtrace` object (arg #1; by
    // identity hash until 2026-09-23). For most throwables CratonVM stores `backtrace == throwable`
    // (self-reference) so the keys agree, but for some (observed: the
    // suite-level failure thrown through RandomizedRunner) they don't — the
    // lookup misses (len 0) while `cap` is large, leaving every slot empty.
    // Consumers such as RandomizedRunner.seedFromThrowable do
    // `element.getClassName().startsWith(...)` and NPE on the null name.
    // Backfill any slot we didn't populate with a non-null placeholder so no
    // StackTraceElement ever has a null class/method name. (A proper fix would
    // unify the depth/elements lookup key; tracked separately.)
    if filled < cap {
        for i in filled..cap {
            // gcg g6/oo: the backfill collects on a refusal too (door 4).
            let ste = crate::util_concurrent_ext::try_alloc_concurrent_synthetic_reclaiming(
                &mut *scope,
                "java/lang/StackTraceElement",
                4,
            )?;
            let ste_h = scope.root(ste);
            fill_stack_trace_element(
                &mut *scope,
                ste,
                "(unknown)",
                "(unknown)",
                "(unknown)",
                None,
                -1,
            );
            let elements = scope.get(&elements_h);
            let ste = scope.get(&ste_h);
            scope.set_array_element(elements, i, Value::Object(Some(ste)));
        }
    }
    if let (Some(started), Some(looked_up)) = (started, looked_up) {
        let built = std::time::Instant::now();
        crate::lang_stackwalker::note_sections(
            crate::lang_stackwalker::SectionKind::Elements,
            filled,
            &[started, looked_up, built, built, built],
        );
    }
    Ok(None)
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on the JDK's
/// `StackTraceElement.of(Ljava/lang/Object;I)[Ljava/lang/StackTraceElement;`
/// (`CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC`, `--real-jdk` over a real JDK
/// only; interpreter round i3 wave 74, lane L2). `Throwable.getOurStackTrace()`
/// calls it for the first `getStackTrace()` / `printStackTrace()` of every
/// throwable. The JDK body is
///
/// ```text
/// StackTraceElement[] stackTrace = new StackTraceElement[depth];
/// for (int i = 0; i < depth; i++) stackTrace[i] = new StackTraceElement();
/// initStackTraceElements(stackTrace, x, depth);   // the VM fills each slot
/// return of(stackTrace);                          // computeFormat() on each
/// ```
///
/// Interpreted, that is `depth` element allocations the fill throws away
/// ([`native_init_stack_trace_elements`] stores fresh elements), then per
/// element a `synchronized` `computeFormat()` making six calls
/// (`getClassLoader0`, `getModule`, `isHashedInJavaBase` ->
/// `VM.isModuleSystemInited`, `ModuleLayer.boot`, `Module.getLayer`,
/// `HashedModules.contains`): wave 73 measured `--real-jdk`
/// `getStackTrace()` at 2.3x `--compatible`'s native (11 705 against
/// 5 181 ns).
///
/// This answers the same array: the fill `initStackTraceElements` does
/// ([`fill_stack_trace_elements_from_backtrace`]), then the `format` byte
/// `computeFormat` derives from each element's `declaringClassObject`
/// ([`stack_trace_element_computed_format`]), and `declaringClassObject`
/// cleared as `computeFormat`'s `finally` clears it. The format's inputs are
/// asked the way the Java asks them: the loader through `Class.getClassLoader0()`
/// (dispatched, so the same body answers) tested against
/// `jdk/internal/loader/BuiltinClassLoader`; `VM.initLevel` read from the
/// static (what the `--real-jdk` `isModuleSystemInited()` body reads); a
/// boot-layer named module looked up in `java.base`'s recorded hash names,
/// the set `HashedModules` builds from the same VM answer (plus `java.base`).
/// One answer per class per array.
pub fn native_stack_trace_element_of_backtrace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let backtrace = match args.first() {
        Some(Value::Object(r)) => *r,
        _ => None,
    };
    let depth = match args.get(1) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    if depth < 0 {
        // `new StackTraceElement[depth]`'s refusal; HotSpot's message is the
        // length.
        return Err(throwable_refusal(
            ctx,
            "java/lang/NegativeArraySizeException",
            &depth.to_string(),
            None,
        ));
    }
    // Cast: checked non-negative above.
    let depth = depth as usize;
    let ste_cid = cached_ste_class_id(ctx);
    let mut scope = NativeHandleScope::new(ctx);
    let backtrace_h = backtrace.map(|b| scope.root(b));
    let module_system_inited = jdk_module_system_inited(&mut scope)?;
    // The array the JDK body allocates first; it collects once before the
    // `OutOfMemoryError`, as `native_throwable_get_stack_trace_array`'s does.
    let arr = crate::util_concurrent_ext::new_ref_array_reclaiming(
        &mut *scope,
        ste_cid,
        depth,
        &mut [],
        "StackTraceElement.of",
    )?;
    let arr_h = scope.root(arr);
    let backtrace = backtrace_h.as_ref().map(|h| scope.get(h));
    let arr = scope.get(&arr_h);
    fill_stack_trace_elements_from_backtrace(&mut *scope, arr, backtrace)?;
    finish_stack_trace_element_formats(&mut scope, &arr_h, module_system_inited)?;
    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on the JDK's
/// `StackTraceElement.of(Ljava/lang/StackFrameInfo;)Ljava/lang/StackTraceElement;`
/// (`CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC`, `--real-jdk` over a
/// real JDK only, default off; interpreter round i3 wave 77, lane L2,
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// `StackFrameInfo.toStackTraceElement()` calls it (inside its own
/// `synchronized (this)` and `ste` memo, which stay the JDK's) for the first
/// element of every walked frame. The JDK 25 body is
///
/// ```text
/// StackTraceElement ste = new StackTraceElement();
/// initStackTraceElement(ste, sfi);   // the VM fills it
/// ste.computeFormat();               // Java: getClassLoader0, getModule, isHashedInJavaBase, ...
/// return ste;
/// ```
///
/// This answers the same element: allocated as `new` does (the private
/// no-argument constructor stores nothing), filled by the very native
/// `initStackTraceElement` is bound to (`crate::native_init_stack_trace_element`,
/// so a lazy frame is settled against a redefinition since the walk exactly
/// as before), then the `format` byte `computeFormat()` derives
/// ([`class_computed_format`]: the class's recorded answer once the module
/// system is up, else asked the way the Java asks it) and
/// `declaringClassObject` cleared as `computeFormat`'s `finally` clears it --
/// the per-element half of [`native_stack_trace_element_of_backtrace`]. An
/// element the fill named no class for keeps `format` 0 (the JDK body would
/// throw from `computeFormat`; no frame the VM walks has no class), as that
/// intrinsic does.
pub fn native_stack_trace_element_of_frame(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(sfi))) = args.first().copied() else {
        // `initStackTraceElement(ste, null)`: HotSpot resolves the frame
        // non-null. Unreachable from Java (the only caller passes `this`).
        let npe = cratonvm_types::error::RuntimeError::NullPointerException { message: None };
        return Err(npe.into());
    };
    let mut scope = NativeHandleScope::new(ctx);
    let sfi_h = scope.root(sfi);
    // Both may load or initialise a class (Java): after the frame is rooted.
    let ste_cid = cached_ste_class_id(&mut *scope);
    let module_system_inited = jdk_module_system_inited(&mut scope)?;
    // `new StackTraceElement()`: the class's own layout once it is loaded
    // (it is: `of` is its static method), collecting and retrying before the
    // catchable `OutOfMemoryError` as `new` does.
    let slots = if ste_cid == ClassId::new(0) {
        0
    } else {
        scope.class_num_total_fields(ste_cid)
    };
    let ste = match (slots > 0)
        .then(|| scope.try_alloc_object(ste_cid, slots))
        .flatten()
    {
        Some(ste) => ste,
        None => crate::util_concurrent_ext::try_alloc_concurrent_synthetic_reclaiming(
            &mut *scope,
            "java/lang/StackTraceElement",
            4,
        )?,
    };
    let ste_h = scope.root(ste);
    let sfi = scope.get(&sfi_h);
    let ste = scope.get(&ste_h);
    crate::native_init_stack_trace_element(
        &mut *scope,
        &[Value::Object(Some(ste)), Value::Object(Some(sfi))],
    )?;
    // `ste.computeFormat()`.
    let ste = scope.get(&ste_h);
    let mirror = match scope.get_field_by_name(ste, "declaringClassObject") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let class_id = mirror.and_then(|m| crate::lang_class::mirror_class_id(&*scope, m));
    let format = match (mirror, class_id) {
        (Some(mirror), Some(cid)) => {
            let mut hashed = None;
            class_computed_format(&mut scope, cid, mirror, module_system_inited, &mut hashed)?        }
        _ => 0,
    };
    let ste = scope.get(&ste_h);
    scope.set_fields_by_name(
        ste,
        &[
            ("format", Value::Int(format)),
            ("declaringClassObject", Value::Object(None)),
        ],
    );
    Ok(Some(Value::Object(Some(scope.get(&ste_h)))))
}

/// `jdk.internal.misc.VM.isModuleSystemInited()` as the `--real-jdk` body
/// answers it: `initLevel >= MODULE_SYSTEM_INITED (2)`, read from the static.
/// When the static cannot be read, the method itself is called.
fn jdk_module_system_inited(
    scope: &mut NativeHandleScope<'_>,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    const MODULE_SYSTEM_INITED: i32 = 2;
    if let Some(vm) = scope.class_id_by_name("jdk/internal/misc/VM") {
        if let Some(index) = scope.static_field_index_by_name(vm, "initLevel") {
            if let Value::Int(level) = scope.get_static_field(vm, index) {
                return Ok(level >= MODULE_SYSTEM_INITED);
            }
        }
    }
    let answer = scope.invoke_static("jdk/internal/misc/VM", "isModuleSystemInited", "()Z", &[])?;
    Ok(match answer {
        Some(Value::Int(v)) => v != 0,
        _ => true,
    })
}

/// `StackTraceElement.of(StackTraceElement[])`'s loop without running it:
/// every element of the array `arr_h` roots gets the `format` byte
/// `computeFormat()` would store and a null `declaringClassObject`. An
/// element with no `declaringClassObject` (the fill makes none: it falls back
/// to `Object`'s mirror) keeps `format` 0 where the JDK body would throw.
fn finish_stack_trace_element_formats(
    scope: &mut NativeHandleScope<'_>,
    arr_h: &cratonvm_native_api::NativeHandle,
    module_system_inited: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // `java.base`'s recorded hash names, asked on the first class that needs
    // them (wave 75: an array whose classes all have a recorded answer asks
    // none).
    let mut hashed: Option<Option<Vec<String>>> = None;
    let mut by_class: std::collections::HashMap<ClassId, i32> = std::collections::HashMap::new();
    let len = {
        let arr = scope.get(arr_h);
        scope.array_length(arr)
    };
    for i in 0..len {
        let arr = scope.get(arr_h);
        let Value::Object(Some(ste)) = scope.get_array_element(arr, i) else {
            continue;
        };
        let mirror = match scope.get_field_by_name(ste, "declaringClassObject") {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        };
        let class_id = mirror.and_then(|m| crate::lang_class::mirror_class_id(&**scope, m));
        let format = match (mirror, class_id) {
            (Some(mirror), Some(cid)) => match by_class.get(&cid) {
                Some(&f) => f,
                None => {
                    // May run Java (`getClassLoader0`), so the element is
                    // read again below.
                    let f = class_computed_format(
                        scope,
                        cid,
                        mirror,
                        module_system_inited,
                        &mut hashed,
                    )?;
                    by_class.insert(cid, f);
                    f                }            },
            _ => 0,
        };
        let arr = scope.get(arr_h);
        if let Value::Object(Some(ste)) = scope.get_array_element(arr, i) {
            scope.set_fields_by_name(
                ste,
                &[
                    ("format", Value::Int(format)),
                    ("declaringClassObject", Value::Object(None)),
                ],
            );
        }
    }
    Ok(())
}

/// The `format` byte `computeFormat()` stores for an element of class
/// `class_id` (whose mirror is `mirror`), as [`finish_stack_trace_element_formats`]
/// and [`native_stack_trace_element_of_frame`] ask it: the class's recorded
/// answer (wave 75, lane L2, `CRATONVM_STACK_TRACE_ORIGIN_RECORD`: recorded
/// only once the module system is up, after which no input of
/// `computeFormat` changes for a class), else
/// [`stack_trace_element_computed_format`], recorded. `hashed` holds
/// `java.base`'s recorded hash names, asked on the first class that needs
/// them (`None` until then). May run Java (`getClassLoader0`).
fn class_computed_format(
    scope: &mut NativeHandleScope<'_>,
    class_id: ClassId,
    mirror: ObjectRef,
    module_system_inited: bool,
    hashed: &mut Option<Option<Vec<String>>>,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let row = if module_system_inited {
        frame_origin_row(&**scope, class_id)
    } else {
        None
    };
    let recorded = row.as_ref().and_then(|row| {
        let f = row
            .computed_format
            .load(std::sync::atomic::Ordering::Relaxed);
        (f != COMPUTED_FORMAT_UNKNOWN).then_some(f)
    });
    if let Some(f) = recorded {
        return Ok(f);
    }
    let names: Option<&[String]> = hashed
        .get_or_insert_with(|| {
            if module_system_inited {
                scope.module_recorded_hash_names("java.base")
            } else {
                None
            }
        })
        .as_deref();
    let f =
        stack_trace_element_computed_format(scope, class_id, mirror, module_system_inited, names)?;
    if let Some(row) = &row {
        row.computed_format
            .store(f, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(f)
}

/// The `format` byte `StackTraceElement.computeFormat()` stores for an
/// element whose `declaringClassObject` is `mirror` (class `class_id`):
/// `BUILTIN_CLASS_LOADER` when `getClassLoader0()` answers a
/// `jdk.internal.loader.BuiltinClassLoader`, `JDK_NON_UPGRADEABLE_MODULE`
/// when `isHashedInJavaBase(getModule())` holds ([`computed_format_bits`]).
fn stack_trace_element_computed_format(
    scope: &mut NativeHandleScope<'_>,
    class_id: ClassId,
    mirror: ObjectRef,
    module_system_inited: bool,
    hashed: Option<&[String]>,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let loader =
        scope.invoke_virtual(mirror, "getClassLoader0", "()Ljava/lang/ClassLoader;", &[])?;
    let builtin_loader = match loader {
        Some(Value::Object(Some(loader))) => {
            let loader_class = scope.class_id_of_object(loader);
            match scope
                .class_assignable_to_name(loader_class, "jdk/internal/loader/BuiltinClassLoader")
            {
                Some(answer) => answer,
                // A context that cannot judge: the VM's own loader record
                // (1 platform, 2 application).
                None => matches!(scope.loader_id_of_class(class_id), 1 | 2),
            }
        }
        _ => false,
    };
    let identity = scope.module_identity_of_class(class_id);
    Ok(computed_format_bits(
        builtin_loader,
        module_system_inited,
        &identity,
        hashed,
    ))
}

/// The pure half of [`stack_trace_element_computed_format`]:
/// `isHashedInJavaBase(m)` is `!VM.isModuleSystemInited() ||
/// (ModuleLayer.boot() == m.getLayer() && HashedModules.contains(m))`, where
/// a boot-layer module is [`cratonvm_native_api::ClassModuleIdentity::Named`]
/// (a non-boot layer's is `Layer`; the unnamed module has no layer) and
/// `HashedModules` is `java.base`'s recorded hash names plus `java.base`, or
/// empty when `java.base` records none.
fn computed_format_bits(
    builtin_loader: bool,
    module_system_inited: bool,
    module: &cratonvm_native_api::ClassModuleIdentity,
    hashed: Option<&[String]>,
) -> i32 {
    let mut bits = 0;
    if builtin_loader {
        bits |= STE_FORMAT_BUILTIN_CLASS_LOADER;
    }
    let hashed_in_java_base = !module_system_inited
        || match (module, hashed) {
            (cratonvm_native_api::ClassModuleIdentity::Named(name), Some(names)) => {
                name == "java.base" || names.iter().any(|n| n == name)
            }
            _ => false,
        };
    if hashed_in_java_base {
        bits |= STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE;
    }
    bits
}

/// The fields [`native_stack_trace_element_to_string_jdk`] reads, in one
/// batch: everything the JDK's `StackTraceElement.toString()` reads.
const STE_TO_STRING_FIELDS: [&str; 8] = [
    "declaringClass",
    "methodName",
    "fileName",
    "lineNumber",
    "moduleName",
    "moduleVersion",
    "classLoaderName",
    "format",
];

/// `StackTraceElement.toString()`'s text from the element's stored fields,
/// as UTF-16 units (the pure half of
/// [`native_stack_trace_element_to_string_jdk`]). The JDK body (JDK 25,
/// `length()` then `appendTo`):
///
/// * `classLoaderName` and `/`, unless `dropClassLoaderName()`
///   (`format & BUILTIN_CLASS_LOADER`) or the name is null or empty;
/// * `moduleName` when non-null and non-empty, then `@moduleVersion` unless
///   `dropModuleVersion()` (`format & JDK_NON_UPGRADEABLE_MODULE`) or the
///   version is null or empty;
/// * `/` when anything above was written (so a named loader of an unnamed
///   module prints `loader//`);
/// * `declaringClass.methodName(`, then `Native Method` (`lineNumber == -2`),
///   `fileName:lineNumber` (a file and a line `>= 0`), `fileName`, or
///   `Unknown Source`, then `)`.
///
/// `format` is read as stored: an element built by the public constructor
/// has `format == 0` and prints every non-empty field.
#[allow(clippy::too_many_arguments)]
fn stack_trace_element_text_units(
    declaring_class: &[u16],
    method_name: &[u16],
    file_name: Option<&[u16]>,
    line_number: i32,
    module_name: Option<&[u16]>,
    module_version: Option<&[u16]>,
    class_loader_name: Option<&[u16]>,
    format: i32,
) -> Vec<u16> {
    fn push_ascii(out: &mut Vec<u16>, s: &str) {
        out.extend(s.bytes().map(u16::from));
    }
    let drop_loader = (format & STE_FORMAT_BUILTIN_CLASS_LOADER) == STE_FORMAT_BUILTIN_CLASS_LOADER;
    let drop_version =
        (format & STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE) == STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE;
    let mut out: Vec<u16> = Vec::with_capacity(
        declaring_class.len() + method_name.len() + file_name.map_or(16, |f| f.len() + 12) + 32,
    );
    if !drop_loader {
        if let Some(loader) = class_loader_name.filter(|l| !l.is_empty()) {
            out.extend_from_slice(loader);
            out.push(u16::from(b'/'));
        }
    }
    if let Some(module) = module_name.filter(|m| !m.is_empty()) {
        out.extend_from_slice(module);
        if !drop_version {
            if let Some(version) = module_version.filter(|v| !v.is_empty()) {
                out.push(u16::from(b'@'));
                out.extend_from_slice(version);
            }
        }
    }
    if !out.is_empty() {
        out.push(u16::from(b'/'));
    }
    out.extend_from_slice(declaring_class);
    out.push(u16::from(b'.'));
    out.extend_from_slice(method_name);
    out.push(u16::from(b'('));
    if line_number == -2 {
        push_ascii(&mut out, "Native Method");
    } else if let Some(file) = file_name {
        out.extend_from_slice(file);
        if line_number >= 0 {
            out.push(u16::from(b':'));
            push_ascii(&mut out, &line_number.to_string());
        }
    } else {
        push_ascii(&mut out, "Unknown Source");
    }
    out.push(u16::from(b')'));
    out
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on the JDK's
/// `StackTraceElement.toString()Ljava/lang/String;`
/// (`CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC`, `--real-jdk` over a
/// real JDK only, default off; interpreter round i3 wave 76, lane L2,
/// `i75-L2-proposal-jdk-only-nojit-print-stack-trace-runs-the-text-path-in-java`).
/// `StackTraceElement` is `final` and `toString()` reads only its own eight
/// fields, so no override is bypassed; the text is
/// [`stack_trace_element_text_units`], a FRESH String as `sb.toString()`
/// returns (not interned), built from the fields' UTF-16 units so nothing is
/// lost. Nothing is cached per element.
///
/// A null `declaringClass` or `methodName` (only an element nobody filled, or
/// one deserialised from a crafted stream: the constructors refuse both)
/// throws the `NullPointerException` the JDK body's
/// `declaringClass.length()` / `methodName.length()` throws, with HotSpot's
/// helpful message for that receiver.
pub fn native_stack_trace_element_to_string_jdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // An instance method: the call site has already refused a null
        // receiver.
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut values = [Value::Object(None); STE_TO_STRING_FIELDS.len()];
    ctx.get_fields_by_name(this, &STE_TO_STRING_FIELDS, &mut values);
    let units = |ctx: &dyn NativeContext, v: Value| -> Option<Vec<u16>> {
        match v {
            Value::Object(Some(s)) => ctx.read_string_units(s),
            _ => None,
        }
    };
    let Some(declaring_class) = units(&*ctx, values[0]) else {
        return Err(throwable_refusal(
            ctx,
            "java/lang/NullPointerException",
            "Cannot invoke \"String.length()\" because \"this.declaringClass\" is null",
            None,
        ));
    };
    let Some(method_name) = units(&*ctx, values[1]) else {
        return Err(throwable_refusal(
            ctx,
            "java/lang/NullPointerException",
            "Cannot invoke \"String.length()\" because \"this.methodName\" is null",
            None,
        ));
    };
    let file_name = units(&*ctx, values[2]);
    let line_number = match values[3] {
        Value::Int(n) => n,
        _ => 0,
    };
    let module_name = units(&*ctx, values[4]);
    let module_version = units(&*ctx, values[5]);
    let class_loader_name = units(&*ctx, values[6]);
    let format = match values[7] {
        Value::Int(f) => f,
        _ => 0,
    };
    let text = stack_trace_element_text_units(
        &declaring_class,
        &method_name,
        file_name.as_deref(),
        line_number,
        module_name.as_deref(),
        module_version.as_deref(),
        class_loader_name.as_deref(),
        format,
    );
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub fn native_throwable_get_stack_trace_depth(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Length only: no copy of the trace and no line resolution (see
    // `NativeExceptionAccess::throwable_stack_trace_depth`).
    // Cast: bounded by `MaxJavaStackTraceDepth` or the frame count; fits i32.
    let depth = ctx
        .throwable_stack_trace_depth(this)
        .map_or(0, |len| len.min(i32::MAX as usize) as i32);
    Ok(Some(Value::Int(depth)))
}

/// getStackTraceElement(int index) — return a StackTraceElement for the given frame index.
///
/// Creates a StackTraceElement object with declaringClass, methodName, fileName, lineNumber.
pub fn native_throwable_get_stack_trace_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("getStackTraceElement on null".to_string()),
            }
            .into());
        }
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };

    // Clone the trace entry data to release the immutable borrow on ctx
    // before we need mutable access for object allocation.
    // Stored trace is outermost-first; `getStackTraceElement(index)` is
    // innermost-first (index 0 = throw site), so map to the reversed index.
    // One entry copied and line-resolved, not the whole trace per call: a
    // `getOurStackTrace()` looping this over every index was quadratic.
    let entry_data = match (
        usize::try_from(index),
        ctx.throwable_stack_trace_depth(this),
    ) {
        (Ok(i), Some(len)) if i < len => ctx.throwable_stack_trace_entry(this, len - 1 - i),
        _ => None,
    };

    // Helper: build a StackTraceElement with 4 fields.
    //
    // audit-2026-05-16: the STE class id was hard-coded to
    // `ClassId::new(0)` (java/lang/Object's id), which caused
    // `getClass()` on the returned STE to surface Object instead of
    // StackTraceElement. We now resolve & cache the real class id via
    // `cached_ste_class_id`, falling back to id 0 only if the class
    // hasn't been loaded yet (preserves previous behaviour on early
    // boot).
    let build_ste = |ctx: &mut dyn NativeContext,
                     exact_class: Option<ClassId>,
                     class_slashed: &str,
                     class_dotted: &str,
                     method_name: &str,
                     file_name: Option<&str>,
                     line: i32|
     -> ObjectRef {
        let ste_cid = cached_ste_class_id(ctx);
        // Allocate with the real instance-field count so the named-field
        // writes in `fill_stack_trace_element` land in valid slots.
        let n = ctx.class_num_total_fields(ste_cid).max(4);
        let ste_obj = ctx.alloc_object(ste_cid, n);
        // `fill_stack_trace_element` allocates (strings, a mirror) and roots
        // only its own copy; the address returned must be read back through a
        // root, not the pre-collection `ste_obj`.
        let mut scope = NativeHandleScope::new(ctx);
        let ste_h = scope.root(ste_obj);
        fill_captured_stack_trace_element(
            &mut *scope,
            ste_obj,
            exact_class,
            class_slashed,
            class_dotted,
            method_name,
            file_name,
            line,
        );
        scope.get(&ste_h)
    };

    match entry_data {
        Some(ste) => {
            // Use the shared `dotted_class_name` cache so repeat traces
            // that hit the same class (e.g. recursive frames) reuse the
            // existing `Arc<str>` instead of re-running `replace('/', ".")`.
            let dotted = match ctx.class_id_by_name(&ste.class_name) {
                Some(cid) => {
                    crate::lang_class::dotted_class_name(ctx.vm_identity(), cid, &ste.class_name)
                }
                // A hidden class is not found by name; keep its `/0x…`
                // suffix as HotSpot's external name does (wave 35).
                None => {
                    std::sync::Arc::from(crate::lang_class::dotted_binary_name(&ste.class_name))
                }
            };
            let obj = build_ste(
                ctx,
                ste.class_id,
                &ste.class_name,
                &dotted,
                &ste.method_name,
                ste.source_file.as_deref(),
                ste.line_number,
            );
            Ok(Some(Value::Object(Some(obj))))
        }
        None => {
            let obj = build_ste(ctx, None, "<unknown>", "<unknown>", "<unknown>", None, -1);
            Ok(Some(Value::Object(Some(obj))))
        }
    }
}

/// The JEP 358 text `NullPointerException.getMessage()` answers from the
/// transient `extendedMessage` when `detailMessage` is null: `Some` only for an
/// instance of `java.lang.NullPointerException` (or a subclass) of a class
/// library with the JDK's `extendedMessage` / `extendedMessageState` fields,
/// whose state is 2 ("computed") and whose `extendedMessage` holds a `String`.
///
/// Interpreter round i4 wave 99, lane L9
/// (`i98-L9-compatible-implicit-npe-text-is-a-detail-message`): the VM builds
/// an implicit NPE whose text it knows at the throw HotSpot's way -- null
/// `detailMessage`, the text in `extendedMessage`, state 2
/// (`exceptions::store_npe_extended_message`) -- so a serialized copy answers
/// `getMessage() == null` as on HotSpot. The `--compatible` bridges in front of
/// the JDK's `getMessage()` override (this file's
/// [`native_throwable_get_message`], `lib.rs`'s `native_exception_get_message`)
/// and their field-read fallbacks answer it here, as the JDK's override does.
/// State 1 (a `--real-jdk` deferred message, the JDK computing it on demand)
/// is not answered: the JDK's own bytecode handles it there. No allocation.
pub fn npe_computed_extended_message(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ObjectRef> {
    let npe = ctx.class_id_by_name("java/lang/NullPointerException")?;
    let class_id = ctx.class_id_of_object(this);
    if class_id != npe && !ctx.is_subclass(class_id, npe) {
        return None;
    }
    let state_slot = ctx.resolve_field_index_by_class_id(npe, "extendedMessageState")?;
    let extended_slot = ctx.resolve_field_index_by_class_id(npe, "extendedMessage")?;
    let slots = ctx.object_num_fields(this);
    if state_slot >= slots || extended_slot >= slots {
        return None;
    }
    if !matches!(ctx.get_field(this, state_slot), Value::Int(2)) {
        return None;
    }
    match ctx.get_field(this, extended_slot) {
        Value::Object(Some(s))
            if ctx.class_name_arc_of_id(ctx.class_id_of_object(s)).as_deref()
                == Some("java/lang/String") =>
        {
            Some(s)
        }
        _ => None,
    }
}

/// Run `body`, a native standing in for APPLICATION or library bytecode
/// (`class_name.method_name descriptor`), and when it gives up with a
/// `NullPointerException` of its OWN wording -- a `RuntimeError::NullPointerException`
/// returned as an internal error, never a Java throwable a callee raised --
/// run the shadowed bytecode on the same arguments instead.
///
/// Interpreter round i4 wave 99, lane L9
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// those shadows raised NPEs whose texts no HotSpot run prints
/// (`ConditionAndOr.left`, `Module.loadClass: name must not be null`, ...)
/// and, for a null the library's bytecode tolerates (a null `SessionLocal`
/// handed down, a short-circuited operand), an NPE HotSpot does not raise at
/// all. The bytecode answers both: its own JEP 358 text or `requireNonNull`
/// message, or no exception. A shadow that gives up after running Java has
/// that Java run again by the bytecode (a library state none of these
/// reaches in practice: a field its constructor always sets, a `getValue`
/// that never answers null).
///
/// The object arguments are pinned across `body` (it may run Java, and so
/// collect) -- up to 16 of them; with more, nothing is re-run. Without the
/// class bytes (no method to run) the shadow's own error stands.
pub fn own_npe_runs_shadowed_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    shadow_refusal_runs_bytecode(
        ctx,
        args,
        body,
        class_name,
        method_name,
        descriptor,
        |e| matches!(e, cratonvm_types::error::RuntimeError::NullPointerException { .. }),
    )
}

/// [`own_npe_runs_shadowed_bytecode`] for a shadow whose OWN refusals are
/// index refusals too: an `ArrayIndexOutOfBoundsException`,
/// `IndexOutOfBoundsException` or `StringIndexOutOfBoundsException` (or an
/// NPE) returned as an internal error re-runs the shadowed bytecode, which
/// answers with the library's own check (its exception type and text, or
/// the JVM's `Index i out of bounds for length n` at the access) and its own
/// partial effects, as HotSpot runs it.
///
/// Interpreter round i5 wave 103, lane L7
/// (`i100-L7-native-shadow-npe-texts`, the BouncyCastle residue): those
/// shadows refused with `ArrayIndexOutOfBoundsException(int)`'s "Array index
/// out of range: n", which only the library bytecode that itself calls that
/// constructor prints. Contract for a body wrapped this way: it refuses
/// BEFORE it writes anything the bytecode would see (a field, an array
/// element) and before it runs Java, so the re-run starts from the state the
/// call started from.
pub fn own_refusal_runs_shadowed_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;
    shadow_refusal_runs_bytecode(ctx, args, body, class_name, method_name, descriptor, |e| {
        matches!(
            e,
            RuntimeError::NullPointerException { .. }
                | RuntimeError::ArrayIndexOutOfBoundsException { .. }
                | RuntimeError::IndexOutOfBoundsException { .. }
                | RuntimeError::StringIndexOutOfBoundsException { .. }
        )
    })
}

/// `CRATONVM_DBG_SHADOW_RERUN` (interpreter round i5 wave 106, lane L7,
/// `i100-L7-native-shadow-npe-texts`): one stderr line each time a native
/// shadow of library bytecode hands a call back to that bytecode
/// (`[shadow-rerun] <class>.<name><desc> after <why>`), the evidence that a
/// shadow-shape probe row reached the registered native at all and then the
/// bytecode. Called by [`shadow_refusal_runs_bytecode`] (H2, JBoss, Lucene and
/// the BouncyCastle `bc_register_rerun!` sites), the ANTLR null cases and the
/// CBC top-of-call cases. Cold: only on a hand-back.
pub fn note_shadow_rerun(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    why: impl FnOnce() -> String,
) {
    if false {
        eprintln!(
            "[shadow-rerun] {class_name}.{method_name}{descriptor} after {}",
            why()
        );
    }
}

/// A native shadow of library bytecode that never sees a null object
/// argument: when any argument is `Value::Object(None)`, the shadowed
/// bytecode runs instead of `body` and its answer stands (the JEP 358 text
/// naming the library's own parameter or field, the library's own check, or
/// no exception at all where the bytecode never touches the argument), as on
/// HotSpot. `body` runs only when no argument is null, or when that bytecode
/// cannot be run at all (an `InternalError`: no class bytes, no such method),
/// so a null never reaches `obj_arg`'s "null object argument" text and no
/// partial effect is repeated (nothing ran before the hand-back).
///
/// Interpreter round i5 wave 107, lane L7
/// (`i106-L7-bc-plain-shadows-answer-null-arrays-with-their-own-npe`): the
/// plain BouncyCastle registrations (`bc_register_null_rerun!`). The receiver
/// of an instance method is never null here (the invoke refused it before);
/// a static shadow's first argument may be. Cost: one scan of `args` per call.
pub fn null_arg_runs_shadowed_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    use cratonvm_types::error::MethodCallFailed;
    let Some(at) = args.iter().position(|a| matches!(a, Value::Object(None))) else {
        return body(ctx, args);
    };
    note_shadow_rerun(class_name, method_name, descriptor, || {
        format!("a null argument (slot {at})")
    });
    match ctx.invoke_special_bytecode_only(class_name, method_name, descriptor, args) {
        Err(MethodCallFailed::InternalError(_)) => body(ctx, args),
        other => other,
    }
}

/// The native callback that runs `$body` through
/// [`null_arg_runs_shadowed_bytecode`]. `$body` is a function path or a
/// NON-capturing closure (coerced to a `NativeCallback`); `$class`, `$name`
/// and `$desc` are constants (a literal, a `const`, or a `concat!` of them),
/// never a `let` binding or a loop variable, so the closure captures nothing.
macro_rules! bytecode_on_null_arg {
    ($body:expr, $class:expr, $name:expr, $desc:expr) => {
        |ctx, args| {
            let body: ::cratonvm_native_api::NativeCallback = $body;
            $crate::lang_misc::null_arg_runs_shadowed_bytecode(
                ctx, args, body, $class, $name, $desc,
            )
        }
    };
}
pub(crate) use bytecode_on_null_arg;

/// The shared body of [`own_npe_runs_shadowed_bytecode`] and
/// [`own_refusal_runs_shadowed_bytecode`]: `reruns` picks the internal
/// `RuntimeError`s that send the call to the bytecode.
fn shadow_refusal_runs_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    reruns: fn(&cratonvm_types::error::RuntimeError) -> bool,
) -> MethodCallResult {
    use cratonvm_types::error::{MethodCallFailed, VmError};
    const MAX_PINNED: usize = 16;
    let mut pins = [usize::MAX; MAX_PINNED];
    let mut pinned = 0usize;
    let mut first_pin = None;
    let rerunnable = args
        .iter()
        .filter(|a| matches!(a, Value::Object(Some(_))))
        .count()
        <= MAX_PINNED;
    if rerunnable {
        for a in args {
            if let Value::Object(Some(o)) = a {
                let pin = ctx.pin_native_root(*o);
                first_pin.get_or_insert(pin);
                pins[pinned] = pin;
                pinned += 1;
            }
        }
    }
    let result = body(ctx, args);
    let own_npe = matches!(
        &result,
        Err(MethodCallFailed::InternalError(VmError::Runtime(e))) if reruns(e)
    );
    if !own_npe || !rerunnable {
        if let Some(base) = first_pin {
            ctx.unpin_native_roots(base);
        }
        return result;
    }
    let mut fresh = Vec::with_capacity(args.len());
    let mut k = 0usize;
    for a in args {
        match a {
            Value::Object(Some(o)) => {
                fresh.push(Value::Object(Some(ctx.read_native_pin(pins[k], *o))));
                k += 1;
            }
            other => fresh.push(*other),
        }
    }
    if let Some(base) = first_pin {
        ctx.unpin_native_roots(base);
    }
    if let Err(MethodCallFailed::InternalError(VmError::Runtime(e))) = &result {
        note_shadow_rerun(class_name, method_name, descriptor, || format!("{e:?}"));
    }
    match ctx.invoke_special_bytecode_only(class_name, method_name, descriptor, &fresh) {
        Err(MethodCallFailed::InternalError(_)) => result,
        other => other,
    }
}

/// The native callback registered for `$class.$name $desc` that runs `$body`
/// through [`own_npe_runs_shadowed_bytecode`]: a non-capturing closure, so it
/// coerces to a `NativeCallback`. `$class`, `$name` and `$desc` must be
/// constants (a literal or a `const`), never a loop variable.
macro_rules! bytecode_on_own_npe {
    ($body:path, $class:expr, $name:expr, $desc:expr) => {
        |ctx, args| {
            $crate::lang_misc::own_npe_runs_shadowed_bytecode(ctx, args, $body, $class, $name, $desc)
        }
    };
}
pub(crate) use bytecode_on_own_npe;

/// [`bytecode_on_own_npe`] through [`own_refusal_runs_shadowed_bytecode`]:
/// an index refusal of the shadow's own re-runs the bytecode too. `$body` is
/// a function path or a NON-capturing closure (it is coerced to a
/// `NativeCallback`); `$class`, `$name`, `$desc` are constants.
macro_rules! bytecode_on_own_refusal {
    ($body:expr, $class:expr, $name:expr, $desc:expr) => {
        |ctx, args| {
            let body: ::cratonvm_native_api::NativeCallback = $body;
            $crate::lang_misc::own_refusal_runs_shadowed_bytecode(
                ctx, args, body, $class, $name, $desc,
            )
        }
    };
}
pub(crate) use bytecode_on_own_refusal;

/// [`own_refusal_runs_shadowed_bytecode`] for a shadow whose own
/// `EOFException` re-runs the bytecode too (interpreter round i5 wave 104,
/// lane L7, `i100-L7-native-shadow-npe-texts`, the Lucene residue): the
/// library's EOF text differs by version (`new EOFException()`, a text naming
/// the position, ...), so the bytecode answers it. Same contract: the body
/// refuses before it writes anything the bytecode reads (or writes only what
/// the bytecode then writes again identically) and before it runs Java.
pub fn own_refusal_or_eof_runs_shadowed_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;
    shadow_refusal_runs_bytecode(ctx, args, body, class_name, method_name, descriptor, |e| {
        matches!(
            e,
            RuntimeError::NullPointerException { .. }
                | RuntimeError::ArrayIndexOutOfBoundsException { .. }
                | RuntimeError::IndexOutOfBoundsException { .. }
                | RuntimeError::StringIndexOutOfBoundsException { .. }
                | RuntimeError::EOFException { .. }
        )
    })
}

/// [`bytecode_on_own_refusal`] through
/// [`own_refusal_or_eof_runs_shadowed_bytecode`].
macro_rules! bytecode_on_own_refusal_or_eof {
    ($body:expr, $class:expr, $name:expr, $desc:expr) => {
        |ctx, args| {
            let body: ::cratonvm_native_api::NativeCallback = $body;
            $crate::lang_misc::own_refusal_or_eof_runs_shadowed_bytecode(
                ctx, args, body, $class, $name, $desc,
            )
        }
    };
}
pub(crate) use bytecode_on_own_refusal_or_eof;

/// `Throwable.detailMessage` of `this` read by name, or -- when it is null --
/// the computed JEP 358 text of an implicit NPE
/// ([`npe_computed_extended_message`]): what the JDK's `getMessage()` answers
/// for the real-JDK layout. `Value::Object(None)` when neither holds one.
pub fn throwable_message_or_npe_extended(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Value {
    match throwable_field_get(ctx, this, "detailMessage") {
        v @ Value::Object(Some(_)) => v,
        other => match npe_computed_extended_message(ctx, this) {
            Some(s) => Value::Object(Some(s)),
            None => other,
        },
    }
}

pub fn native_throwable_get_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Interpreter round i4 wave 99, lane L9: an implicit NPE's JEP 358 text
    // is in `extendedMessage`, not `detailMessage` (`--compatible` runs this
    // bridge in front of the JDK's `NullPointerException.getMessage()`).
    if matches!(
        throwable_field_get(ctx, this, "detailMessage"),
        Value::Object(None)
    ) {
        if let Some(text) = npe_computed_extended_message(ctx, this) {
            return Ok(Some(Value::Object(Some(text))));
        }
    }
    // Real-JDK Throwable layout has `detailMessage` at slot 1 (after
    // `backtrace` at slot 0); synthetic-stub layout puts it at slot 0
    // (unnamed `_f0`). Prefer the field-name lookup so the real-JDK
    // bytecode (which writes via `putfield detailMessage`) and our
    // native init helpers agree.
    //
    // CRITICAL: the raw slot-0 fallback is ONLY valid for the synthetic-stub
    // layout. In the real-JDK layout slot 0 is `backtrace`, which
    // `capture_throwable_trace` populates with a self-reference (the
    // Throwable itself) as a non-null marker. Blindly falling back to slot 0
    // for a real-JDK Throwable therefore returns the *exception object* as
    // the "message" — a caller doing `getMessage().contains(...)` then
    // dispatches `String.contains` against the exception's class and dies
    // with `NoSuchMethodError`. Gate the fallback on the class genuinely
    // lacking a named `detailMessage` field.
    let has_named_detail_message = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .and_then(|cn| ctx.resolve_field_index(&cn, "detailMessage"))
        .is_some();
    let by_name = throwable_field_get(ctx, this, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        // `detailMessage` resolved by name but is null/absent: that is a
        // legitimately message-less exception — return null, do NOT read
        // slot 0 (it is `backtrace` in the real-JDK layout).
        _ if has_named_detail_message => Value::Object(None),
        // Synthetic-stub layout: no named `detailMessage` field at all; the
        // canonical slot really is index 0 — BUT only when it actually holds a
        // `java/lang/String`. `capture_throwable_trace` parks the Throwable
        // itself in slot 0 (the `backtrace` non-null marker) for real-JDK-layout
        // exceptions whose inherited `detailMessage` our `resolve_field_index`
        // failed to find by name (it does not walk to `Throwable`). Returning
        // that self-reference made a no-arg `new IllegalStateException()` report
        // `getMessage() == "Exception"` instead of `null`, and
        // `getMessage().contains(...)` would `NoSuchMethodError` on the
        // exception class. Gate the fallback on the slot actually being a String.
        _ => match ctx.get_field(this, 0) {
            v @ Value::Object(Some(o))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(o))
                    .as_deref()
                    == Some("java/lang/String") =>
            {
                v
            }
            _ => Value::Object(None),
        },
    };
    // Return the field value directly. The previous `read_string` validation
    // dropped legitimate JDK String references whose internal layout
    // `read_java_string` couldn't parse during early boot — for the common
    // `Throwable(String)` ctor the field holds a real `java/lang/String`
    // and the caller treats the returned reference as one regardless.
    match detail {
        Value::Object(obj_opt) => Ok(Some(Value::Object(obj_opt))),
        _ => Ok(Some(Value::Object(None))),
    }
}

pub fn native_throwable_get_localized_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pin = ctx.pin_native_root(this);
    let receiver = ctx.read_native_pin(pin, this);
    let result = ctx.invoke_virtual(receiver, "getMessage", "()Ljava/lang/String;", &[]);
    // gen r4w3/rooting: the fallback must see the post-`getMessage` address,
    // not a re-read of the stale `args[0]`.
    let this = ctx.read_native_pin(pin, receiver);
    ctx.unpin_native_roots(pin);

    match result {
        Ok(Some(Value::Object(obj))) => Ok(Some(Value::Object(obj))),
        // `CRATONVM_EXC_MESSAGE_PROPAGATE`: an exception thrown by an
        // overriding `getMessage` leaves `getLocalizedMessage`, as on HotSpot.
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e))
            if exc_message_propagate_enabled(&*ctx) =>
        {
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e))
        }
        Ok(Some(_)) | Ok(None) | Err(_) => {
            native_throwable_get_message(ctx, &[Value::Object(Some(this))])
        }
    }
}

/// `CRATONVM_EXC_MESSAGE_PROPAGATE` — default ON in both modes, `=0` restores
/// the swallowing. `Throwable.getLocalizedMessage()` / `toString()` let an
/// exception thrown by an overriding `getMessage` / `getLocalizedMessage`
/// propagate; these natives read the field instead, so the exception vanished
/// (`r14w1-compat-throwable-message-natives-swallow-exceptions-patch`). The
/// `printStackTrace` walkers keep their fallback. Read once per VM per thread.
fn exc_message_propagate_enabled(ctx: &dyn NativeContext) -> bool {
    thread_local! {
        static ON: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    ON.with(|c| match c.get() {
        Some((owner, on)) if owner == vm => on,
        _ => {
            let on =
                true;
            c.set(Some((vm, on)));
            on
        }
    })
}

// --- Throwable additional methods ---

/// True iff `this`'s runtime class is `java.lang.reflect.InvocationTargetException`
/// (or a subclass) — the only standard throwable whose `getCause()` aliases to a
/// `target` field. Used to gate the `target`-as-cause fallback so it does not
/// leak an unrelated `target` field from other Throwable subclasses.
fn is_invocation_target_exception(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let this_cid = ctx.class_id_of_object(this);
    match ctx.class_id_by_name("java/lang/reflect/InvocationTargetException") {
        Some(ite_cid) => this_cid == ite_cid || ctx.is_subclass(this_cid, ite_cid),
        None => false,
    }
}

/// getCause() — read the cause field.
///
/// Real-JDK Throwable has `cause` at slot 2 (after backtrace, detailMessage).
/// Our synthetic-stub layout puts it at slot 1 (unnamed `_f1`). Prefer the
/// field-name lookup so this works regardless of layout.
///
/// `InvocationTargetException` (and similar wrappers) override `getCause()`
/// in bytecode to return their own field (`target`). When the dispatch path
/// routes here anyway — e.g. via the Throwable-base hierarchy walk after a
/// stale invoke-cache miss — we mimic the override by reading `target` when
/// the synthesized cause field is null. The check is field-name-driven so
/// it stays layout-agnostic.
///
/// JDK-spec sentinel: real-JDK `Throwable` declares `private Throwable
/// cause = this;` — a self-reference means "no cause set yet" — and
/// `getCause()` returns `null` in that case. Real-JDK bytecode running
/// through our interpreter (and any path that mirrors that initializer)
/// can therefore land here with `cause == this`. Returning `this` would
/// drive Spring Boot's `getExitCodeFromExitCodeGeneratorException`
/// recursion into a `StackOverflowError`. Map the sentinel to `null` per
/// the JDK contract.
pub fn native_throwable_get_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let by_name_cause = read_throwable_field(ctx, this, "cause");
    if let Value::Object(Some(cause_obj)) = by_name_cause {
        // JDK sentinel: cause==this means "uninitialized cause"; report null.
        if cause_obj == this {
            return Ok(Some(Value::Object(None)));
        }
        return Ok(Some(by_name_cause));
    }
    // `InvocationTargetException.getCause()` (and `getTargetException()`) alias
    // to the dedicated `target` field, not the inherited `Throwable.cause`; this
    // native backs both. But that aliasing must be gated on the receiver ACTUALLY
    // being an ITE — a plain `Throwable` subclass that merely declares its own
    // unrelated `target` field (e.g. Spring's `InvocationRejectedException`,
    // which holds the rejected *bean* there) must NOT have `getCause()` return
    // that object. Otherwise callers walking the cause chain — JUnit's
    // `ExceptionUtils.findNestedThrowables` — `ClassCastException` casting the
    // non-Throwable target to `Throwable`. Only read `target` for a real ITE.
    let by_name_target = ctx.get_field_by_name(this, "target");
    if let Value::Object(Some(target_obj)) = by_name_target {
        if is_invocation_target_exception(ctx, this) {
            // Same self-reference guard, in case the `target` field is
            // initialized with `this` as a sentinel.
            if target_obj == this {
                return Ok(Some(Value::Object(None)));
            }
            return Ok(Some(by_name_target));
        }
    }
    // Real-JDK only: when neither `cause` nor `target` named fields hold a
    // value, the cause is genuinely null. The previous synthetic-stub
    // fallback that read `slot 1` was wrong here — slot 1 of a real-JDK
    // Throwable layout is `detailMessage` (a String), so the fallback
    // returned the message text as the cause and Spring Boot's
    // `getExitCodeFromExitCodeGeneratorException` recursion then dispatched
    // `Throwable.getCause()` on a String, surfacing as
    // `NoSuchMethodError: java/lang/String.getCause()Ljava/lang/Throwable;`.
    Ok(Some(Value::Object(None)))
}

/// Build the exception a `Throwable` state-machine check refuses with.
///
/// The JDK's own refusals carry the receiver as their cause — `throw new
/// IllegalStateException(msg, this)` / `new IllegalArgumentException(msg,
/// exception)` — and that cause is observable (`ise.getCause()` measured as
/// `java.lang.RuntimeException: m` on JDK 25). A bare
/// `RuntimeError::IllegalStateException` cannot carry one, so construct the
/// throwable properly and fall back to the message-only variant if that fails;
/// the *type* is the load-bearing half and must survive either way.
fn throwable_refusal(
    ctx: &mut dyn NativeContext,
    exc_class: &str,
    message: &str,
    cause: Option<ObjectRef>,
) -> cratonvm_types::error::MethodCallFailed {
    use cratonvm_types::error::{MethodCallFailed, RuntimeError};
    let message_only = || -> MethodCallFailed {
        match exc_class {
            "java/lang/IllegalStateException" => RuntimeError::IllegalStateException {
                message: message.to_string(),
            }
            .into(),
            "java/lang/NullPointerException" => RuntimeError::NullPointerException {
                message: Some(message.to_string()),
            }
            .into(),
            _ => RuntimeError::IllegalArgumentException {
                message: message.to_string(),
            }
            .into(),
        }
    };
    let Some(cause) = cause else {
        return message_only();
    };
    // `create_string` and the constructor both allocate, so the receiver we were
    // handed can move underneath us.
    let pin = ctx.pin_native_root(cause);
    let msg_ref = ctx.create_string(message);
    let cause = ctx.read_native_pin(pin, cause);
    let msg_pin = ctx.pin_native_root(msg_ref);
    let cause = ctx.read_native_pin(pin, cause);
    let msg_ref = ctx.read_native_pin(msg_pin, msg_ref);
    let built = ctx.new_object_initialized(
        exc_class,
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        &[Value::Object(Some(msg_ref)), Value::Object(Some(cause))],
    );
    ctx.unpin_native_roots(pin);
    match built {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => message_only(),
    }
}

/// `initCause(Throwable)` — the JDK's `Throwable.initCause` **state machine**,
/// not a bare field write.
///
/// This native SHADOWS the real `Throwable.initCause` bytecode for **every**
/// instance — registering a native on a real JDK class does not add a fallback,
/// it replaces the method — so the two refusals `initCause` is specified to
/// raise only exist if they are written here. Until 2026-08-12 neither was, and
/// `initCause` was an unconditional setter — the
/// differential probe measured `Throwable.initCauseAfterCtorThrows`,
/// `Throwable.initCauseTwiceThrows` and `Throwable.selfCauseThrows` all as
/// `no-throw` where HotSpot raises.
///
/// The javadoc, verbatim:
///
/// > `@throws IllegalStateException` if this throwable was created with
/// > `Throwable(Throwable)` or `Throwable(String,Throwable)`, or this method has
/// > already been called on this throwable.
///
/// > `@throws IllegalArgumentException` if `cause` is this throwable. (A
/// > throwable cannot be its own cause.)
///
/// **Order matters and is the JDK's**: the already-set test runs *first*, which
/// is why `new RuntimeException().initCause(itself)` is an
/// `IllegalArgumentException` (the sentinel still says "unset") rather than an
/// `IllegalStateException`. Reversing the two would produce the right kind of
/// failure with the wrong type, and a mistyped refusal sends a caller down the
/// wrong `catch` branch just as surely as a missing one.
///
/// "Already set" is `this.cause != this`: the JDK declares `private Throwable
/// cause = this;` as a sentinel for "no cause yet", so a constructor-supplied
/// cause, a previous `initCause` — *including* `initCause(null)`, which is why a
/// null check would not do — and a deserialised throwable whose `cause` is
/// genuinely null all read as set. HotSpot refuses all three. Measured
/// 2026-08-12 on `--fast-boot`: CratonVM's `cause` field already tracks HotSpot's
/// exactly (`isSelf` after `new RuntimeException("m")`, not-self after
/// `(String,Throwable)`, null after `initCause(null)`), so testing the raw field
/// is testing the same thing HotSpot tests.
pub fn native_throwable_init_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cause_val = args.get(1).cloned().unwrap_or(Value::Object(None));

    // `if (this.cause != this) throw new IllegalStateException(...)`, read raw:
    // `read_throwable_cause`'s self-reference guard is exactly what must NOT be
    // applied here, because the sentinel is the whole signal.
    //
    // Every arm that is not a verdict fails OPEN (proceed with the write). An
    // over-throw here would refuse a legitimate first `initCause` — the two
    // dead sections this same probe turned up on 2026-08-12 were both real JDK
    // code refusing state *we* had corrupted, so a refusal added on a signal we
    // cannot read is the specific way this goes wrong.
    let class_id = ctx.class_id_of_object(this);
    let declares_cause = ctx
        .resolve_field_index_by_class_id(class_id, "cause")
        .is_some();
    let already_set = match read_throwable_field(ctx, this, "cause") {
        // A live reference that is not the sentinel: a constructor-supplied
        // cause, or a previous `initCause`.
        Value::Object(Some(c)) => c != this,
        // A genuine null is "set" too — `initCause(null)` is a valid first call
        // and HotSpot refuses the second (measured). Trust the null only when
        // the receiver really has the field: `read_throwable_field`'s slot
        // fallback answers the same `Object(None)` for a class that has none.
        Value::Object(None) => declares_cause,
        // An unset slot on a LEGACY object reads back as `Int(0)` — no
        // verdict. (On a COMPACT object it reads `Object(None)` and takes the
        // arm above, as HotSpot refuses `initCause` on an
        // `allocateInstance`'d throwable; every throwable the VM builds has
        // `cause = this` seeded before its constructor runs.)
        _ => false,
    };
    if crate::nbflags().dbg_cause {
        let this_cls = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        let raw = throwable_field_get(ctx, this, "cause");
        let idx = ctx.resolve_field_index_by_class_id(class_id, "cause");
        let nf = ctx.object_num_fields(this);
        ctx.capture_throwable_stack_trace(this);
        let frames = ctx.get_throwable_stack_trace(this).unwrap_or_default();
        let top: Vec<String> = frames
            .iter()
            .take(8)
            .map(|f| format!("{}.{}:{}", f.class_name, f.method_name, f.line_number))
            .collect();
        eprintln!(
            "CAUSE_DBG_INIT this={this_cls} hash={} raw={raw:?} is_self={} declares={declares_cause} idx={idx:?} nfields={nf} already_set={already_set} arg={:?} frames=[{}]",
            ctx.identity_hash_code(this),
            matches!(raw, Value::Object(Some(c)) if c == this),
            cause_val,
            top.join(" <- ")
        );
    }
    if already_set {
        // `"Can't overwrite cause with " + Objects.toString(cause, "a null")`.
        // Rendering the argument re-enters Java (`toString()`), so the receiver
        // has to survive a collection across it.
        let (this, rendered) = match cause_val {
            Value::Object(Some(c)) => {
                let pin = ctx.pin_native_root(this);
                let cause_pin = ctx.pin_native_root(c);
                // `Objects.toString` dispatches the argument's OWN `toString()`,
                // which a Throwable subclass may override; only fall back to
                // this file's `Throwable.toString()` reconstruction if that
                // dispatch cannot answer.
                let text = match ctx.invoke_virtual(c, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                    _ => None,
                };
                let text = match text {
                    Some(t) => t,
                    None => {
                        let c = ctx.read_native_pin(cause_pin, c);
                        throwable_to_string_text(ctx, c).1
                    }
                };
                let this = ctx.read_native_pin(pin, this);
                ctx.unpin_native_roots(pin);
                (this, text)
            }
            _ => (this, "a null".to_string()),
        };
        return Err(throwable_refusal(
            ctx,
            "java/lang/IllegalStateException",
            &format!("Can't overwrite cause with {rendered}"),
            Some(this),
        ));
    }
    if let Value::Object(Some(c)) = cause_val {
        if c == this {
            return Err(throwable_refusal(
                ctx,
                "java/lang/IllegalArgumentException",
                "Self-causation not permitted",
                Some(this),
            ));
        }
    }
    write_throwable_cause(ctx, this, cause_val);
    Ok(Some(Value::Object(Some(this))))
}

/// toString() — build "ClassName: message" or just "ClassName"
pub fn native_throwable_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };

    let result = if exc_message_propagate_enabled(&*ctx) {
        throwable_to_string_text_propagating(ctx, this)?.1
    } else {
        throwable_to_string_text(ctx, this).1
    };
    // Round 13 wave 9 (lane bigdec2): fresh, not interned into the never-pruned
    // pool (one String per distinct message); `CRATONVM_COMPUTED_STRINGS_UNINTERNED`.
    let str_obj = crate::lang_math::fresh_computed_string(ctx, &result);
    Ok(Some(Value::Object(Some(str_obj))))
}

fn throwable_class_name(ctx: &mut dyn NativeContext, t: ObjectRef) -> String {
    // Round 15 wave 2 (lane trace2): `Throwable.toString()` is
    // `getClass().getName()`, which keeps a hidden class's `/0x…`.
    trace_binary_name(
        ctx.stack_trace_fill_switches().binary_names,
        &ctx.class_name_of_id(ctx.class_id_of_object(t))
            .unwrap_or_else(|| "java/lang/Throwable".to_string()),
    )
}

/// Round 15 wave 2 (lane trace2; `r15w1-trace-ste-class-name-fallbacks-FIXED-20260930.md`):
/// the external name of a class named by its internal name alone -- a
/// captured frame's class that the by-name lookup did not find, every frame of
/// the `printStackTrace` text, a throwable's own class -- as `Class.getName()`
/// and `native_throwable_get_stack_trace_element` give it: a hidden class
/// keeps its `/0x…` suffix (`lang_class::dotted_binary_name`), where
/// `replace('/', ".")` printed `p.Foo$$Lambda.0x…` from two `StackTraceElement`
/// builders and the text while the third said `p.Foo$$Lambda/0x…`.
/// `CRATONVM_STACK_TRACE_BINARY_NAMES=0` (`binary_names == false`, the VM's
/// `StackTraceFillSwitches::binary_names`, read once per VM since interpreter
/// round i3 wave 77, lane L2, where it was an env read per hidden-class
/// frame) restores the replace for a name that has such a suffix.
fn trace_binary_name(binary_names: bool, internal: &str) -> String {
    let binary = crate::lang_class::dotted_binary_name(internal);
    if binary.contains('/') && !binary_names {
        return internal.replace('/', ".");
    }
    binary
}

/// Round 15 wave 3 (lane trace3): the task the served `java.lang.Thread.run()`
/// (`lib.rs`, both registrations) runs for the real-JDK-layout thread `this`:
/// the `target` field `java.lang.Thread` itself declares (JDK 17), else its
/// `holder`'s `task` (JDK 21+). By name (`get_field_by_name`) the MOST-DERIVED
/// field of that name answers, so a `Thread` subclass declaring its own
/// `target` or `holder` (`class Retry extends Thread { final Runnable target;
/// ... }`) had its OWN field run -- or nothing -- where HotSpot runs the task
/// handed to `super(task)`. The slots are resolved from `java.lang.Thread` up
/// the object's own superclass chain (a class-name lookup can miss a
/// bootstrap alias). A `java.lang.Thread` instance itself, a chain that does
/// not reach it, or `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0` (read only
/// for a subclass) keeps the by-name read. `vm_exec::thread_run_bottom_frame`
/// reads the task the same way.
pub fn served_thread_run_task(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    // The read before round 15 wave 3.
    fn by_name(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
        let mut task = ctx.get_field_by_name(this, "target");
        if matches!(task, Value::Object(None)) {
            if let Value::Object(Some(holder)) = ctx.get_field_by_name(this, "holder") {
                task = ctx.get_field_by_name(holder, "task");
            }
        }
        task
    }
    // Round 15 wave 3 (lane trace4): the walk is shared with the other
    // `Thread` natives (`thread_declared_field`). Round 15 wave 4 (lane
    // threadvm; TR4W3-1): through `thread_declared_field` itself, which the
    // per-VM layout memo answers without the class-manager lock.
    let slot_of = |field: ThreadFieldSlot| match field {
        ThreadFieldSlot::Declared(slot) => Some(slot),
        ThreadFieldSlot::ByName | ThreadFieldSlot::Absent => None,
    };
    let target = thread_declared_field(ctx, this, "target");
    if target == ThreadFieldSlot::ByName {
        return by_name(ctx, this);
    }
    let holder = thread_declared_field(ctx, this, "holder");
    if holder == ThreadFieldSlot::ByName
        || (target == ThreadFieldSlot::Absent && holder == ThreadFieldSlot::Absent)
    {
        return by_name(ctx, this);
    }
    let (target, holder) = (slot_of(target), slot_of(holder));
    if let Some(slot) = target {
        let task = ctx.get_field(this, slot);
        if !matches!(task, Value::Object(None)) {
            return task;
        }
    }
    if let Some(slot) = holder {
        if let Value::Object(Some(holder)) = ctx.get_field(this, slot) {
            // `Thread$FieldHolder` is a final JDK class: its `task` is exact.
            return ctx.get_field_by_name(holder, "task");
        }
    }
    Value::Object(None)
}

/// Round 15 wave 3 (lane trace4; TR2W2-2): the served `java.lang.Thread.run()`
/// (`lib.rs`, both registrations) runs its task, `task.run()`, recording the
/// call for the stack captures inside it
/// (`NativeExceptionAccess::enter_served_thread_run`): they list HotSpot's
/// `java.lang.Thread.run` frame right below the first frame the task pushes.
/// The record ends on every exit path; the task's exception propagates.
pub fn served_thread_run_call(
    ctx: &mut dyn NativeContext,
    task: ObjectRef,
) -> MethodCallResult {
    let mark = ctx.enter_served_thread_run();
    let ran = ctx.invoke_virtual(task, "run", "()V", &[]);
    // Round 15 wave 6 (lane trace6): a yield unwinds through here and the
    // task's frames remount WITHOUT this call, so its record must say so.
    if matches!(
        &ran,
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield { .. }
        ))
    ) {
        ctx.leave_served_thread_run_yielded(mark);
    } else {
        ctx.leave_served_thread_run(mark);
    }
    ran
}

/// `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS` (default on): the `Thread`
/// natives address the fields `java.lang.Thread` declares, not a subclass's
/// field of the same name. `0` restores the by-name reads and writes.
pub fn thread_declared_fields_on() -> bool {
    true
}

/// `java.lang.Thread`'s class id when it is a PROPER superclass of `this`'s
/// class (a `Thread` subclass instance), else `None`: a `java.lang.Thread`
/// itself, whose by-name field is its own, or an object whose chain does not
/// reach it. Found on the object's own superclass chain, as
/// [`served_thread_run_task`] needs (a class-name lookup can miss a bootstrap
/// alias). Round 15 wave 3 (lane trace3's walk, shared by lane trace4).
fn thread_class_above_object(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ClassId> {
    let class_id = ctx.class_id_of_object(this);
    if ctx.class_name_arc_of_id(class_id).as_deref() == Some("java/lang/Thread") {
        return None;
    }
    let mut id = ctx.superclass_of(class_id);
    for _ in 0..64 {
        let current = id?;
        if ctx.class_name_arc_of_id(current).as_deref() == Some("java/lang/Thread") {
            return Some(current);
        }
        id = ctx.superclass_of(current);
    }
    None
}

/// Where the field `name` that `java.lang.Thread` declares lives on a thread
/// ([`thread_declared_field`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadFieldSlot {
    /// Address it by name: `this` is a `java.lang.Thread` itself (its
    /// by-name field is its own), no `Thread` at all, or the switch is off.
    ByName,
    /// `java.lang.Thread`'s own slot, on a subclass instance.
    Declared(usize),
    /// A subclass instance, and `java.lang.Thread` declares no such field
    /// (JDK 21+ has no `target`): the `Thread` code never touches a field of
    /// that name, so a subclass's own is neither read nor written.
    Absent,
}

/// Round 15 wave 3 (lane trace4;
/// `r15w3-trace3-thread-fields-read-by-name-FIXED-20260930.md`): the field `name`
/// that `java.lang.Thread` declares, for the thread `this`. By name
/// (`get_field_by_name` / `set_field_by_name`) the MOST-DERIVED field of that
/// name answers, so `class P extends Thread { Object holder; }` had its own
/// `holder` read by `getPriority` / `isDaemon` / `getThreadGroup`, and
/// written with the `Thread$FieldHolder` (and a `target` with the task) by the
/// served `Thread.<init>`, where HotSpot leaves it `null`. The switch
/// ([`thread_declared_fields_on`]) is read only for a subclass instance.
///
/// Round 15 wave 4 (lane threadvm; TR4W3-1): the VM's per-VM `Thread` layout
/// memo (`NativeHeapAccess::thread_declared_field_slot`) answers first, with
/// the same three answers and no lock; `None` from it (a mock, the memo off
/// by `CRATONVM_THREAD_LAYOUT_CACHE=0`, a name it does not keep) walks here.
fn thread_declared_field(ctx: &dyn NativeContext, this: ObjectRef, name: &str) -> ThreadFieldSlot {
    use cratonvm_native_api::registry::ThreadDeclaredField;
    if let Some(answer) = ctx.thread_declared_field_slot(this, name) {
        return match answer {
            ThreadDeclaredField::ByName => ThreadFieldSlot::ByName,
            ThreadDeclaredField::Declared(slot) => ThreadFieldSlot::Declared(slot),
            ThreadDeclaredField::Absent => ThreadFieldSlot::Absent,
        };
    }
    let Some(thread) = thread_class_above_object(ctx, this) else {
        return ThreadFieldSlot::ByName;
    };
    if !thread_declared_fields_on() {
        return ThreadFieldSlot::ByName;
    }
    match ctx.resolve_field_index_by_class_id(thread, name) {
        Some(slot) => ThreadFieldSlot::Declared(slot),
        None => ThreadFieldSlot::Absent,
    }
}

/// The value of the field `name` that `java.lang.Thread` declares, on the
/// thread `this` ([`thread_declared_field`]; `null` when it declares none);
/// by name for a `java.lang.Thread` itself, or with the switch off.
pub fn thread_field(ctx: &dyn NativeContext, this: ObjectRef, name: &str) -> Value {
    match thread_declared_field(ctx, this, name) {
        ThreadFieldSlot::Declared(slot) => ctx.get_field(this, slot),
        ThreadFieldSlot::ByName => ctx.get_field_by_name(this, name),
        ThreadFieldSlot::Absent => Value::Object(None),
    }
}

/// Write the field `name` that `java.lang.Thread` declares, on the thread
/// `this` ([`thread_declared_field`]; nothing when it declares none); by name
/// for a `java.lang.Thread` itself, or with the switch off.
pub fn set_thread_field(ctx: &dyn NativeContext, this: ObjectRef, name: &str, value: Value) {
    match thread_declared_field(ctx, this, name) {
        ThreadFieldSlot::Declared(slot) => ctx.set_field(this, slot, value),
        ThreadFieldSlot::ByName => ctx.set_field_by_name(this, name, value),
        ThreadFieldSlot::Absent => {}
    }
}

/// Round 15 wave 5 (lane threadvm2;
/// `r15w4-threadvm-pool-workers-registered-daemon-FIXED-20260930.md`): whether the
/// `java.util.concurrent.ThreadPoolExecutor$Worker` `worker` belongs to this
/// VM's own async pool (`async_worker_pool`, behind
/// `spawn_runnable_on_real_thread`). That pool stands in for
/// `ForkJoinPool.commonPool()`, whose workers HotSpot makes daemon, but its
/// workers come from `Executors.defaultThreadFactory()` and are not: the VM's
/// `thread_start` registers them daemon, so a `runAsync` task does not hold
/// the exit (for the pool's 10 s keep-alive, or for ever if it blocks).
/// `Worker` is a final inner class, so its `this$0` is exact; no pool yet
/// means no such worker.
pub fn is_vm_async_pool_worker(ctx: &mut dyn NativeContext, worker: ObjectRef) -> bool {
    let Some(pool) = crate::async_worker_pool_existing(ctx) else {
        return false;
    };
    matches!(ctx.get_field_by_name(worker, "this$0"), Value::Object(Some(owner)) if owner == pool)
}

fn throwable_detail_message_text(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<String> {
    let has_named_detail_message = ctx
        .class_name_of_id(ctx.class_id_of_object(t))
        .and_then(|cn| ctx.resolve_field_index(&cn, "detailMessage"))
        .is_some();
    // An implicit NPE's JEP 358 text sits in `extendedMessage` (wave 99 L9).
    let detail = match throwable_message_or_npe_extended(ctx, t) {
        v @ Value::Object(Some(_)) => v,
        _ if has_named_detail_message => Value::Object(None),
        _ => match ctx.get_field(t, 0) {
            v @ Value::Object(Some(o))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(o))
                    .as_deref()
                    == Some("java/lang/String") =>
            {
                v
            }
            _ => Value::Object(None),
        },
    };
    match detail {
        Value::Object(Some(sr)) => ctx.read_string(sr),
        _ => None,
    }
}

/// Return a Throwable.toString()-compatible text line plus the post-GC receiver
/// reference. HotSpot's `Throwable.toString()` calls virtual
/// `getLocalizedMessage()`, not a raw `detailMessage` field read; custom
/// subclasses such as Spring's `JmsException` depend on that virtual message to
/// include linked exception details.
fn throwable_to_string_text(ctx: &mut dyn NativeContext, t: ObjectRef) -> (ObjectRef, String) {
    let pin = ctx.pin_native_root(t);
    let receiver = ctx.read_native_pin(pin, t);
    let localized =
        ctx.invoke_virtual(receiver, "getLocalizedMessage", "()Ljava/lang/String;", &[]);
    let current = ctx.read_native_pin(pin, receiver);
    let message = match localized {
        Ok(Some(Value::Object(Some(msg)))) => ctx.read_string(msg),
        Ok(Some(Value::Object(None))) | Ok(None) => None,
        _ => throwable_detail_message_text(ctx, current),
    };
    ctx.unpin_native_roots(pin);

    let class_name = throwable_class_name(ctx, current);
    let text = match message {
        Some(m) => format!("{class_name}: {m}"),
        None => class_name,
    };
    (current, text)
}

/// [`throwable_to_string_text`] for `Throwable.toString()` itself
/// (`CRATONVM_EXC_MESSAGE_PROPAGATE`): a Java exception thrown by the virtual
/// `getLocalizedMessage()` propagates, as on HotSpot; every other failure
/// keeps the field-read fallback. The `printStackTrace` walkers keep the
/// swallowing helper (a header must still print).
fn throwable_to_string_text_propagating(
    ctx: &mut dyn NativeContext,
    t: ObjectRef,
) -> Result<(ObjectRef, String), cratonvm_types::error::MethodCallFailed> {
    let pin = ctx.pin_native_root(t);
    let receiver = ctx.read_native_pin(pin, t);
    let localized =
        ctx.invoke_virtual(receiver, "getLocalizedMessage", "()Ljava/lang/String;", &[]);
    let current = ctx.read_native_pin(pin, receiver);
    ctx.unpin_native_roots(pin);
    let message = match localized {
        Ok(Some(Value::Object(Some(msg)))) => ctx.read_string(msg),
        Ok(Some(Value::Object(None))) | Ok(None) => None,
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) => {
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e));        }
        _ => throwable_detail_message_text(ctx, current),
    };
    let class_name = throwable_class_name(ctx, current);
    let text = match message {
        Some(m) => format!("{class_name}: {m}"),
        None => class_name,
    };
    Ok((current, text))
}

/// The cause of `t` as `printStackTrace` sees it: the result of the VIRTUAL
/// `getCause()`, with the JDK `cause = this` "uninitialized" sentinel and a
/// self-reference both reported as `None`.
///
/// **Dispatching rather than reading the field is the point.** `Throwable`'s
/// own `printStackTrace` calls `getCause()`, and several subclasses override it
/// to answer a field of their own -- `InvocationTargetException.getCause()`
/// returns `target`, and this VM's registrar has an entry saying exactly that
/// a few hundred lines from here. A raw `cause` read misses every one of them,
/// so a wrapped exception printed its header and nothing else:
///
/// ```text
///   new InvocationTargetException(new IOException("io"), "wrapped")
///     HotSpot   java.lang.reflect.InvocationTargetException: wrapped
///               Caused by: java.io.IOException: io
///     was       java.lang.reflect.InvocationTargetException: wrapped
/// ```
///
/// MEASURED by `ThrowableFamilySweep.java`. This is the same
/// correction [`throwable_to_string_text`] already carries for the message half
/// -- it dispatches `getLocalizedMessage()` for the same reason -- so the two
/// halves of a printed header now agree about whose implementation decides.
/// The field read stays as the fallback for a receiver whose `getCause` cannot
/// be dispatched (a partially-built VM-minted throwable on the boot path).
fn throwable_cause(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<ObjectRef> {
    let pin = ctx.pin_native_root(t);
    let receiver = ctx.read_native_pin(pin, t);
    let dispatched = ctx.invoke_virtual(receiver, "getCause", "()Ljava/lang/Throwable;", &[]);
    let t = ctx.read_native_pin(pin, receiver);
    ctx.unpin_native_roots(pin);
    let by_name = match dispatched {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        Ok(Some(Value::Object(None))) | Ok(None) => Value::Object(None),
        // Dispatch failed outright, or answered a non-reference: fall back to
        // the raw field, which is what this function did before it dispatched.
        Err(_) | Ok(Some(_)) => throwable_field_get(ctx, t, "cause"),
    };
    if let Value::Object(Some(c)) = by_name {
        if c == t {
            return None;
        }
        if crate::nbflags().dbg_cause {
            let t_cls = ctx
                .class_name_of_id(ctx.class_id_of_object(t))
                .unwrap_or_default();
            let c_cls = ctx
                .class_name_of_id(ctx.class_id_of_object(c))
                .unwrap_or_default();
            eprintln!(
                "CAUSE_DBG_READ this={t_cls} hash={} ptr={:?} cause={c_cls} cause_hash={} cause_ptr={:?}",
                ctx.identity_hash_code(t),
                t.as_ptr(),
                ctx.identity_hash_code(c),
                c.as_ptr()
            );
        }
        return Some(c);
    }
    None
}

/// `CRATONVM_THROWABLE_NATIVE_TRACE_FIELD` -- default ON (round 13 wave 12,
/// lane misc12, both modes; a correctness fix). The native `printStackTrace`
/// overloads print a trace `setStackTrace` stored, `setStackTrace` honours the
/// immutable-stack protocol, an explicit null stream after boot throws
/// `NullPointerException`, and a captured frame's location follows
/// `StackTraceElement.toString()` ("Native Method", "Unknown Source"). `0`
/// restores the capture-store-only natives. Read per call: printing a trace
/// and setting one are not hot.
fn throwable_native_trace_field() -> bool {
    true
}

/// `CRATONVM_THROWABLE_PRINT_CAUSE_LOOP` -- default ON (round 13 wave 13, lane
/// trace3, both modes; a correctness fix). The native `printStackTrace` walks
/// the `Caused by:` chain in a loop, so a chain of any length prints whole;
/// `0` restores the recursion that stopped after 32 nested levels. Read once
/// per top-level or `Suppressed:` walk: printing is not hot.
fn throwable_print_cause_loop() -> bool {
    true
}

/// Whether the native `printStackTrace` walkers take each header line from the
/// throwable's VIRTUAL `toString()`, as `Throwable.printStackTrace` does
/// (`s.println(this)` / `prefix + caption + this`). Default ON (nio21 wave 12
/// lane L5); `CRATONVM_THROWABLE_PRINT_VIRTUAL_TOSTRING=0` restores the
/// `ClassName: getLocalizedMessage()` rebuild.
///
/// The rebuild ignored an overriding `toString()`. Measured on the w8 host
/// battery, `--compatible` `Nio21UncaughtMain tostring`: HotSpot prints
/// `Exception in thread "main" ` and then `JavaThread::exit`'s
/// `Exception: java.lang.IllegalStateException thrown from the
/// UncaughtExceptionHandler` line, because the override throws out of
/// `printStackTrace`. This VM printed a fabricated `Nio21UncaughtMain$BadToString:
/// bad` header and the frames. `--real-jdk` runs the JDK bytecode and was already
/// right.
fn throwable_print_virtual_to_string() -> bool {
    true
}

/// One `printStackTrace` header for `t`: the virtual `toString()`'s answer
/// (`"null"` for a null result, as `PrintStream.println(Object)` prints it), or
/// the Java exception the override threw, which the caller propagates as
/// `printStackTrace` does. When the call does not answer (a failed dispatch, a
/// boot-path throwable, the kill switch), the header is rebuilt from
/// [`throwable_to_string_text`] as before. Returns `t`'s current address
/// (the call runs Java and can collect).
///
/// Never called from `native_throwable_to_string`: that native is what the
/// virtual call lands on for a throwable that does not override `toString()`,
/// so calling it from there would recurse.
fn throwable_header_text(
    ctx: &mut dyn NativeContext,
    t: ObjectRef,
) -> (
    ObjectRef,
    Result<String, cratonvm_types::error::MethodCallFailed>,
) {
    let mut t = t;
    if throwable_print_virtual_to_string() {
        let pin = ctx.pin_native_root(t);
        let receiver = ctx.read_native_pin(pin, t);
        let answered = ctx.invoke_virtual(receiver, "toString", "()Ljava/lang/String;", &[]);
        let current = ctx.read_native_pin(pin, receiver);
        ctx.unpin_native_roots(pin);
        match answered {
            Ok(Some(Value::Object(Some(s)))) => {
                if let Some(text) = ctx.read_string(s) {
                    return (current, Ok(text));
                }
            }
            Ok(Some(Value::Object(None))) => return (current, Ok("null".to_string())),
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) => {
                return (
                    current,
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)),
                );
            }
            _ => {}
        }
        t = current;
    }
    let (current, text) = throwable_to_string_text(ctx, t);
    (current, Ok(text))
}

/// The line number a native frame carries (`stackwalker::LINE_NUMBER_NATIVE`,
/// HotSpot's and `StackTraceElement.isNativeMethod()`'s -2).
const LINE_NUMBER_NATIVE_METHOD: i32 = -2;

/// `Throwable.UNASSIGNED_STACK`, when the real class supplies it (a
/// synthetic-JDK image has no such static, and it is null before
/// `Throwable.<clinit>`).
fn unassigned_stack_sentinel(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let cid = ctx.class_id_by_name("java/lang/Throwable")?;
    let idx = ctx.static_field_index_by_name(cid, "UNASSIGNED_STACK")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(u)) => Some(u),
        _ => None,
    }
}

/// The `stackTrace` array `setStackTrace` stored (or `getOurStackTrace`
/// cached), or `None` while the field is null or still the sentinel.
///
/// A DELIBERATELY EMPTY array is a set array. The guard used to be
/// `array_length(set_arr) > 0`, which cannot tell `setStackTrace(new
/// StackTraceElement[0])` from the never-assigned state -- so clearing a
/// stack trace silently did nothing and `getStackTrace().length` answered 2
/// where HotSpot answers 0 (MEASURED: `probes/IoSystemSweep.java`, both
/// modes). The JDK's own discriminator is IDENTITY, not length: `Throwable`
/// initialises the field to the private static sentinel `UNASSIGNED_STACK`,
/// itself a zero-length array, and `setStackTrace` stores a DIFFERENT
/// zero-length array. Compare against that sentinel when the real class
/// supplies it, and fall back to the old length test when it does not.
fn explicitly_set_stack_trace(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<ObjectRef> {
    let Value::Object(Some(set_arr)) = throwable_field_get(ctx, t, "stackTrace") else {
        return None;
    };
    let was_set = match unassigned_stack_sentinel(ctx) {
        // The real class library is present: identity decides, so an
        // explicitly-set empty array is honoured.
        Some(unassigned) => unassigned != set_arr,
        // No sentinel to compare against — keep the pre-2026-08-27 rule.
        None => ctx.array_length(set_arr) > 0,
    };
    was_set.then_some(set_arr)
}

/// `Throwable(String, Throwable, boolean, false)` leaves `stackTrace == null
/// && backtrace == null`, and the JDK's `setStackTrace` is then a no-op after
/// its validation. Only on the real layout (the sentinel exists), and only
/// when the VM holds no captured frames for `t`, so a throwable some VM path
/// filled without the `backtrace` marker keeps accepting a set trace.
fn stack_trace_is_immutable(ctx: &mut dyn NativeContext, t: ObjectRef) -> bool {
    unassigned_stack_sentinel(ctx).is_some()
        && !matches!(
            throwable_field_get(ctx, t, "stackTrace"),
            Value::Object(Some(_))
        )
        && !matches!(
            throwable_field_get(ctx, t, "backtrace"),
            Value::Object(Some(_))
        )
        && ctx.throwable_stack_trace_depth(t).unwrap_or(0) == 0
}

/// One line per element of a set `StackTraceElement[]`, each the element's
/// own `toString()` -- exactly the text `Throwable.printStackTrace` prints
/// after `"\tat "`. The calls run bytecode, so `arr` is pinned across them.
fn set_stack_trace_frame_text(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<String> {
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let arr = ctx.read_native_pin(pin, arr);
        let text = match ctx.get_array_element(arr, i) {
            Value::Object(Some(elem)) => {
                match ctx.invoke_virtual(elem, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                    _ => None,                }            }
            _ => None,
        };
        // `String.valueOf(null)`: `setStackTrace` refuses null elements, so
        // only a failed `toString` lands here.
        out.push(text.unwrap_or_else(|| "null".to_string()));
    }
    ctx.unpin_native_roots(pin);
    out
}

/// The object whose captured trace a read of `t` names: what `t`'s own
/// `backtrace` holds when it is an object, else `t`. The JDK's
/// `getOurStackTrace()` reads through `backtrace`
/// (`StackTraceElement.of(backtrace, depth)`, the `--real-jdk` path), and
/// HotSpot's `Object.clone()` of a `Cloneable` exception copies that field, so
/// the clone shares the original's trace; the registry holds no row under the
/// clone's own address. With `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1` it is
/// the throwable-owned holder, which the VM's readers decode. Interpreter
/// round i3 wave 82, lane L2 (`--compatible`'s `getStackTrace()` and
/// `printStackTrace()` natives).
fn throwable_trace_source(ctx: &mut dyn NativeContext, t: ObjectRef) -> ObjectRef {
    match read_throwable_field(ctx, t, "backtrace") {
        Value::Object(Some(backtrace)) => backtrace,
        _ => t,
    }
}

/// Format the captured stack-trace frames for `t`, innermost first, without
/// indentation. Keeping the raw rendered frame text lets the print routine
/// apply HotSpot's common-suffix elision consistently to causes and suppressed
/// exceptions.
///
/// A trace `setStackTrace` stored wins over the captured frames, as it does in
/// `Throwable.getOurStackTrace()` (round 13 wave 12, lane misc12,
/// [`throwable_native_trace_field`]): JUnit's pruned traces, AssertJ's
/// filtered ones and every synthetic re-throw print through this.
fn throwable_frame_text(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<String> {
    if throwable_native_trace_field() {
        if let Some(arr) = explicitly_set_stack_trace(ctx, t) {
            return set_stack_trace_frame_text(ctx, arr);
        }
    }
    let binary_names = ctx.stack_trace_fill_switches().binary_names;
    let source = throwable_trace_source(ctx, t);
    let frames: Vec<(String, String, Option<String>, i32, Option<ClassId>)> = ctx
        .get_throwable_stack_trace(source)
        .map(|tr| {
            // stored trace is outermost-first; printStackTrace prints the
            // throw site first, so reverse to innermost-first.
            tr.iter()
                .rev()
                .map(|e| {
                    (
                        // Round 15 wave 2 (lane trace2): a hidden class keeps
                        // its `/0x…`, as `getStackTrace()` names it.
                        trace_binary_name(binary_names, &e.class_name),
                        e.method_name.to_string(),
                        e.source_file.as_ref().map(|f| f.to_string()),
                        e.line_number,
                        e.class_id,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // `StackTraceElement.toString()`'s location rules (round 13 wave 12):
    // a native frame (line -2) is "Native Method", and a frame with no file
    // is "Unknown Source" whatever its line. This printed "Thread.java" and
    // "Unknown Source:12" for those two.
    let jdk_location = throwable_native_trace_field();
    // Round 13 wave 13 (lane trace3): and its `loader/module@version/` prefix
    // (`java.base/java.lang.Thread.run(...)`), once per class. `t` is not read
    // again here, so the mirror reads may allocate.
    let with_prefix = stack_trace_module_prefix(&*ctx);
    let mut prefixes: Vec<(ClassId, String)> = Vec::new();
    let frames: Vec<(String, String, Option<String>, i32)> = frames
        .into_iter()
        .map(|(cls, meth, file, line, class_id)| {
            let prefix = match class_id {
                Some(cid) if with_prefix => {
                    match prefixes.iter().find(|(known, _)| *known == cid) {
                        Some((_, p)) => p.clone(),
                        None => {
                            let (module, version) = frame_module(&*ctx, cid);
                            let loader =
                                frame_loader_name(ctx, cid).and_then(|s| ctx.read_string(s));
                            let p = stack_frame_prefix(
                                loader.as_deref(),
                                module.as_deref(),
                                version.as_deref(),
                            );
                            prefixes.push((cid, p.clone()));
                            p
                        }
                    }
                }
                _ => String::new(),
            };
            (format!("{prefix}{cls}"), meth, file, line)
        })
        .collect();
    frames
        .into_iter()
        .map(|(cls, meth, file, line)| {
            let loc = match (file.as_deref(), line) {
                (_, LINE_NUMBER_NATIVE_METHOD) if jdk_location => "Native Method".to_string(),
                (None, _) if jdk_location => "Unknown Source".to_string(),
                // `lineNumber >= 0` in `toString()`: a line-0 frame (emitted
                // by some compilers) prints `F.java:0` (round 13 wave 13).
                (Some(f), n) if jdk_location && n >= 0 => format!("{f}:{n}"),
                (Some(f), n) if n > 0 => format!("{f}:{n}"),
                (Some(f), _) => f.to_string(),
                (None, n) if n > 0 => format!("Unknown Source:{n}"),
                _ => "Unknown Source".to_string(),
            };
            format!("{cls}.{meth}({loc})")
        })
        .collect()
}

/// Return the real suppressed-throwable elements.
///
/// This used to say "the JDK sentinel is a List, while CratonVM's native
/// `addSuppressed` replaces it with a Throwable array; only the latter
/// represents user-visible suppressed exceptions". That array was the D7 defect
/// -- see [`store_suppressed`] -- and `addSuppressed` now writes the declared
/// `java.util.List` wherever one exists.
fn throwable_suppressed(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<ObjectRef> {
    // BOTH shapes, through the one reader. This body accepted the array shape
    // only and answered "none" for anything else, so once `addSuppressed`
    // started storing the declared `java.util.List` a printed trace would have
    // lost every `Suppressed:` line. See [`suppressed_elements`].
    suppressed_elements(ctx, t)
}

/// Emit a single line: record it for in-VM consumers and write it to the
/// host process's stderr fd (2) so `printStackTrace` actually shows up
/// when the JVM runs an embedded program. The record_printed_line call
/// keeps existing tests that scan `thread.printed_lines` working.
fn emit_stack_line(ctx: &mut dyn NativeContext, line: String, fd: u32) {
    ctx.record_printed_line(line.clone());
    // `printStackTrace` goes through `println` in the JDK, so it takes the
    // captured separator like every other writer -- and this was the one copy
    // of the fallback with no empty-string filter, so a `line.separator` set
    // to "" ran every stack frame together. See
    // `NativeContext::captured_line_separator`.
    let sep = ctx.captured_line_separator();
    let _ = ctx.fd_table().write_string(fd, &line);
    let _ = ctx.fd_table().write_string(fd, &sep);
}

/// Resolve the host fd that a `printStackTrace(PrintStream/PrintWriter)` target
/// should write to. The no-arg `printStackTrace()` goes to `System.err` (fd 2).
/// When an explicit stream is passed we honour it: if it is the `System.out`
/// static we route to stdout (fd 1) — `t.printStackTrace(System.out)` must land
/// on stdout, not stderr — otherwise we keep the stderr sink (the common
/// `printStackTrace(System.err)` case and any other stream we can't map).
fn print_stream_target_fd(ctx: &mut dyn NativeContext, stream: Option<ObjectRef>) -> u32 {
    let stream = match stream {
        Some(s) => s,
        None => return 2,
    };
    if let Some(sys) = ctx.class_id_by_name("java/lang/System") {
        if let Some(idx) = ctx.static_field_index_by_name(sys, "out") {
            if let Value::Object(Some(out)) = ctx.get_static_field(sys, idx) {
                if out == stream {
                    return 1;
                }
            }
        }
    }
    2
}

/// Has `System.err` been assigned? Before `initPhase1` it is null, and a
/// catch handler's `printStackTrace(System.err)` then passes a null stream
/// that must not throw.
fn system_err_is_set(ctx: &mut dyn NativeContext) -> bool {
    let Some(sys) = ctx.class_id_by_name("java/lang/System") else {
        return false;
    };
    let Some(idx) = ctx.static_field_index_by_name(sys, "err") else {
        return false;
    };
    matches!(ctx.get_static_field(sys, idx), Value::Object(Some(_)))}

/// True iff `stream` is the `System.out` or `System.err` static — the two
/// streams the fd fast path can reach directly. Any other non-null stream is a
/// user-provided sink (e.g. a `ByteArrayOutputStream`/`StringWriter`-backed
/// `PrintWriter`) that must be driven through its own `println`.
fn is_system_out_or_err(ctx: &mut dyn NativeContext, stream: ObjectRef) -> bool {
    if let Some(sys) = ctx.class_id_by_name("java/lang/System") {
        for name in ["out", "err"] {
            if let Some(idx) = ctx.static_field_index_by_name(sys, name) {
                if let Value::Object(Some(std)) = ctx.get_static_field(sys, idx) {
                    if std == stream {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// printStackTrace() / printStackTrace(PrintStream) / printStackTrace(PrintWriter).
///
/// Walks the full cause chain (with a cycle guard) and prints each
/// throwable's "ClassName: message" header followed by its captured
/// stack frames as "\tat ..." lines. Output goes both to the recorded-
/// line buffer (so tests scanning `thread.printed_lines` keep working)
/// and to the host process's stderr fd so users actually see the trace
/// when WildFly / Keycloak / etc. dump exceptions during boot.
pub fn native_throwable_print_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // No-arg overload: real JDK writes to `System.err`. Same trap as the
    // explicit-stream overload (see its doc comment): if `System.err` has
    // been redirected to a tee/capture stream (`System.setErr`,
    // `OutputCaptureExtension`), a raw fd-2 write bypasses that stream's
    // Java-level buffer entirely. Resolve the CURRENT `System.err` value and
    // route through its own `println` when it's wrapped.
    let err_stream = ctx.class_id_by_name("java/lang/System").and_then(|sys| {
        ctx.static_field_index_by_name(sys, "err").and_then(|idx| {
            match ctx.get_static_field(sys, idx) {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            }
        })
    });
    match err_stream {
        Some(s) if matches!(ctx.get_field_by_name(s, "out"), Value::Object(Some(_))) => {
            print_throwable_chain_to_stream_obj(ctx, this, s)?;
        }
        _ => print_throwable_chain_to_fd(ctx, this, 2)?,
    }
    Ok(None)
}

/// Collect the full printStackTrace text (header + captured frames for the
/// throwable and its cause chain) as a list of lines, WITHOUT emitting them.
/// Shared by the fd sink and the stream-object sink so both produce identical
/// text. The header path pins each throwable while it asks the receiver for its
/// virtual localized message, matching `Throwable.toString()` semantics.
#[cfg_attr(not(test), allow(dead_code))]
fn collect_throwable_chain_lines(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<String> {
    collect_throwable_chain_lines_checked(ctx, this).0
}

/// [`collect_throwable_chain_lines`], plus the Java exception a header's
/// `toString()` threw (nio21 wave 12 lane L5). The walk stops there, as
/// `printStackTrace` does: the lines before it were already printed, and the
/// exception leaves `printStackTrace`.
fn collect_throwable_chain_lines_checked(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> (Vec<String>, Option<cratonvm_types::error::MethodCallFailed>) {
    // gen r4w3/rooting: `seen` holds (pin handle, last-known address) pairs.
    // The walk runs `getLocalizedMessage` / `getCause` / List `get` bytecode
    // between identity comparisons, so a bare `Vec<ObjectRef>` would compare
    // against vacated from-space addresses. Every entry stays pinned until
    // `collect_throwable_chain_lines` releases its base pin.
    fn seen_contains(
        ctx: &dyn NativeContext,
        seen: &[(usize, ObjectRef)],
        throwable: ObjectRef,
    ) -> bool {
        seen.iter()
            .any(|&(handle, old)| ctx.read_native_pin(handle, old) == throwable)
    }

    fn append_throwable(
        ctx: &mut dyn NativeContext,
        lines: &mut Vec<String>,
        seen: &mut Vec<(usize, ObjectRef)>,
        throwable: ObjectRef,
        caption: &str,
        prefix: &str,
        enclosing_frames: &[String],
        depth: usize,
        first_error: &mut Option<cratonvm_types::error::MethodCallFailed>,
    ) {
        if depth > 32 || first_error.is_some() {
            return;
        }
        // Round 13 wave 13 (lane trace3): the `Caused by:` link is walked by
        // this loop rather than by recursion, so only `Suppressed:` nesting
        // counts against the Rust-stack bound above and a cause chain prints
        // whole, as `Throwable.printStackTrace` does (it printed 33 levels and
        // then nothing, `r13w12-misc12-native-printer-stops-after-32-causes-FIXED-20260929.md`).
        let cause_loop = throwable_print_cause_loop();
        let mut throwable = throwable;
        let mut caption = caption;
        let mut enclosing_frames: Vec<String> = enclosing_frames.to_vec();
        loop {
            // A throwable already printed in this trace is named, not repeated
            // or silently dropped: `Throwable.printEnclosedStackTrace` prints
            // `<prefix><caption>[CIRCULAR REFERENCE: <toString>]` for it
            // (interpreter round i1 wave 23, lane L7; this returned without a
            // line, so a cause / suppressed cycle lost its last line;
            // `L7W23ErrorShapes.java` `cycle`).
            let already_seen = seen_contains(&*ctx, seen, throwable);
            let (current, header) = match throwable_header_text(ctx, throwable) {
                (current, Ok(header)) => (current, header),
                (_, Err(e)) => {
                    *first_error = Some(e);
                    return;
                }
            };
            if already_seen || seen_contains(&*ctx, seen, current) {
                lines.push(format!("{prefix}{caption}[CIRCULAR REFERENCE: {header}]"));
                return;
            }
            // gen r4w3/rooting: pin the receiver; it is re-read below after the
            // suppressed walk (recursive, GC-capable) and before `getCause`.
            let self_pin = ctx.pin_native_root(current);
            seen.push((self_pin, current));
            let frames = throwable_frame_text(ctx, current);
            // A set trace is rendered through each element's `toString()`, which
            // runs bytecode and can move the receiver (round 13 wave 12).
            let current = ctx.read_native_pin(self_pin, current);
            let common = frames
                .iter()
                .rev()
                .zip(enclosing_frames.iter().rev())
                .take_while(|(frame, enclosing)| frame == enclosing)
                .count();
            lines.push(format!("{prefix}{caption}{header}"));
            for frame in &frames[..frames.len().saturating_sub(common)] {
                lines.push(format!("{prefix}\tat {frame}"));
            }
            if common > 0 {
                lines.push(format!("{prefix}\t... {common} more"));
            }

            // gen r4w3/rooting: each suppressed element is pinned before the
            // first recursive call (which can move the rest) and re-read per
            // iteration.
            let suppressed_list = throwable_suppressed(ctx, current);
            let mut suppressed_pins: Vec<(usize, ObjectRef)> =
                Vec::with_capacity(suppressed_list.len());
            for s in suppressed_list {
                suppressed_pins.push((ctx.pin_native_root(s), s));
            }
            for (handle, old) in suppressed_pins {
                let suppressed = ctx.read_native_pin(handle, old);
                append_throwable(
                    ctx,
                    lines,
                    seen,
                    suppressed,
                    "Suppressed: ",
                    &format!("{prefix}\t"),
                    &frames,
                    depth + 1,
                    first_error,
                );
            }
            if first_error.is_some() {
                return;
            }
            let current = ctx.read_native_pin(self_pin, current);
            let Some(cause) = throwable_cause(ctx, current) else {
                return;
            };
            if !cause_loop {
                append_throwable(
                    ctx,
                    lines,
                    seen,
                    cause,
                    "Caused by: ",
                    prefix,
                    &frames,
                    depth + 1,
                    first_error,
                );
                return;
            }
            throwable = cause;
            caption = "Caused by: ";
            enclosing_frames = frames;
        }
    }

    let mut lines = Vec::new();
    let mut seen = Vec::new();
    // gen r4w3/rooting: every pin the walk takes sits above `base`; one
    // truncation releases them all.
    let mut first_error = None;
    let base = ctx.pin_native_root(this);
    append_throwable(
        ctx,
        &mut lines,
        &mut seen,
        this,
        "",
        "",
        &[],
        0,
        &mut first_error,
    );
    ctx.unpin_native_roots(base);
    (lines, first_error)
}

/// Shared body for the fd sink of all `printStackTrace` overloads: writes the
/// header + captured frames for the throwable and its cause chain to `fd`.
fn print_throwable_chain_to_fd(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    fd: u32,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let (lines, error) = collect_throwable_chain_lines_checked(ctx, this);
    for line in lines {
        emit_stack_line(ctx, line, fd);
    }
    error.map_or(Ok(()), Err)
}

/// Stream-object sink for `printStackTrace(PrintStream)` / `(PrintWriter)` when
/// the target is a user-provided stream (NOT System.out/err, which use the fd
/// fast path). The native fd write cannot reach a buffer such as a
/// `ByteArrayOutputStream`-backed `PrintWriter`, so we drive the trace through
/// the stream object's own `println(String)` — exactly like HotSpot's
/// `Throwable.printStackTrace(PrintWriter)` — so the text lands wherever the
/// stream points. Both PrintStream and PrintWriter declare `println(String)V`.
fn print_throwable_chain_to_stream_obj(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    stream: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Pin the stream across the allocating create_string / re-entrant println.
    // gen r4w3/rooting: pin it BEFORE collecting the lines — that walk runs
    // `getLocalizedMessage` / `getCause` bytecode and can move `stream`.
    let pin = ctx.pin_native_root(stream);
    let (lines, error) = collect_throwable_chain_lines_checked(ctx, this);
    for line in lines {
        // Keep test consumers that scan `printed_lines` working.
        ctx.record_printed_line(line.clone());
        let s = ctx.read_native_pin(pin, stream);
        let arg = Value::Object(Some(ctx.create_string(&line)));
        let s = ctx.read_native_pin(pin, s);
        // Best-effort: ignore a secondary failure while printing a trace.
        let _ = ctx.invoke_virtual(s, "println", "(Ljava/lang/String;)V", &[arg]);
    }
    ctx.unpin_native_roots(pin);
    error.map_or(Ok(()), Err)
}

/// addSuppressed(Throwable) — append to the `suppressedExceptions` list.
///
/// ES-FAIL-FAMILY-20260710: this used to hardcode field **index 2** for
/// "suppressed storage". Index 2 is `cause` in the real-JDK Throwable
/// layout used consistently everywhere else in this file (`backtrace`=0,
/// `detailMessage`=1, `cause`=2, `stackTrace`=3, `suppressedExceptions`=4 —
/// see `write_throwable_cause`'s own doc comment). Every real `addSuppressed`
/// call therefore silently clobbered the receiver's `cause` field with a
/// freshly-allocated 1-element `Object[]` array instead of touching
/// `suppressedExceptions` — surfacing later as a corrupted `Caused by:` line
/// in `printStackTrace` (root-caused via a hardware watchpoint catching the
/// exact `set_field(this, 2, …)` write; see the known-issues doc). Fixed by
/// resolving `suppressedExceptions` **by name** instead of a hardcoded
/// index, matching the pattern `init_suppressed_sentinel` already used
/// correctly for the same field.
///
/// 2026-08-12 — the three rules the JDK states for this method, none of which
/// this native implemented (the differential probe measured
/// `Throwable.addSuppressedSelfThrows` as `no-throw` and
/// `Throwable.suppressionDisabled` as `1:0` against HotSpot's `0:0`). The
/// javadoc, verbatim:
///
/// > `@throws IllegalArgumentException` if `exception` is this throwable; a
/// > throwable cannot suppress itself.
///
/// > `@throws NullPointerException` if `exception` is `null`.
///
/// > If suppression is disabled, this method does nothing other than to
/// > validate its argument.
///
/// "other than to validate its argument" is load-bearing and measured: with
/// suppression disabled, HotSpot *still* raises `IllegalArgumentException` for
/// `addSuppressed(this)` and `NullPointerException` for `addSuppressed(null)`.
/// Both checks therefore run BEFORE the disabled test — silently returning on a
/// null argument, as this native used to, hides a caller bug in exactly the
/// place (a failed `close()`) where it is hardest to notice.
///
/// Suppression is disabled iff `suppressedExceptions == null` — that is the
/// JDK's own encoding, written by the four-arg protected constructor. Measured
/// on `--fast-boot`: CratonVM already reproduces both states exactly
/// (`Collections$EmptyList` by default, `null` after
/// `new RuntimeException(m, null, false, false)`), because the real constructor
/// bytecode runs. VM-*minted* throwables are the case that had to be closed
/// alongside this, or the new rule would drop suppressions on them — see
/// `mirror_suppressed_initialiser` in `vm/src/runtime/exceptions.rs`.
/// The suppressed throwables of `t`, in whichever of the two shapes the field
/// holds.
///
/// `Throwable.suppressedExceptions` is declared `java.util.List<Throwable>`, and
/// on an image with the real class library that is what it holds: the
/// `SUPPRESSED_SENTINEL` empty list to begin with, and a list of its own once
/// something has been suppressed. A synthetic image has no usable `List`, so
/// [`store_suppressed`] falls back to the `Object[]` these natives have always
/// written. Reading BOTH is what lets the writer choose per image rather than
/// per build -- the same self-checking shape `sb_view` uses for the two
/// `StringBuilder` layouts.
///
/// The empty sentinel needs no case of its own: its `size()` is zero.
fn suppressed_elements(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<ObjectRef> {
    let Value::Object(Some(stored)) = read_throwable_field(ctx, t, "suppressedExceptions") else {
        return Vec::new();
    };
    if ctx.heap_kind_of(stored) == cratonvm_types::ObjectKind::Array {
        return (0..ctx.array_length(stored))
            .filter_map(|i| match ctx.get_array_element(stored, i) {
                Value::Object(Some(e)) => Some(e),
                _ => None,
            })
            .collect();
    }
    // A List. `size()` and `get(int)` are ordinary bytecode and may allocate, so
    // the list reference is pinned and re-read across every call.
    let pin = ctx.pin_native_root(stored);
    let list = ctx.read_native_pin(pin, stored);
    let size = match ctx.invoke_virtual(list, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) if n > 0 => n,
        _ => {
            ctx.unpin_native_roots(pin);
            return Vec::new();
        }
    };
    // gen r4w3/rooting: every element is pinned as it is collected, because
    // the NEXT `get` call can move the ones already gathered; all are re-read
    // after the last GC point. The pins sit above `pin`, so the single
    // `unpin_native_roots(pin)` below releases them too.
    let mut pinned: Vec<(usize, ObjectRef)> = Vec::with_capacity(size as usize);
    for i in 0..size {
        let list = ctx.read_native_pin(pin, stored);
        match ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)]) {
            Ok(Some(Value::Object(Some(e)))) => pinned.push((ctx.pin_native_root(e), e)),
            _ => break,
        }
    }
    let out: Vec<ObjectRef> = pinned
        .iter()
        .map(|&(handle, old)| ctx.read_native_pin(handle, old))
        .collect();
    ctx.unpin_native_roots(pin);
    out
}

/// Store `elements` into `t.suppressedExceptions` in the shape this image can
/// hold: a real `java.util.ArrayList` when one is available, the legacy
/// `Object[]` otherwise.
///
/// **The List shape is not cosmetic.** The field's declared type is
/// `Ljava/util/List;`, and `java.io.ObjectInputStream` type-checks a
/// deserialized field value against that descriptor. With an array in there,
/// every throwable carrying a suppressed exception failed its round trip:
///
/// ```text
///   ClassCastException: cannot assign instance of [Ljava.lang.Object; to
///   field java.lang.Throwable.suppressedExceptions of type java.util.List
///   in instance of java.lang.IllegalStateException
/// ```
///
/// which is the shape of defect that stays invisible until real bytecode reads
/// what a native wrote. MEASURED by `ThrowableFamilySweep.java`.
///
/// **GC discipline.** The elements are staged into a plain `Object[]` FIRST:
/// filling it runs no bytecode, so nothing can move while it is built, and one
/// pinned array then keeps every element reachable across the `add` calls --
/// which do run bytecode and may collect. That array is also the fallback
/// value, so the failure path costs nothing extra.
fn store_suppressed(ctx: &mut dyn NativeContext, this: ObjectRef, elements: &[ObjectRef]) {
    use cratonvm_types::ClassId;
    let staged = ctx.new_ref_array(ClassId::new(0), elements.len());
    for (i, e) in elements.iter().enumerate() {
        ctx.set_array_element(staged, i, Value::Object(Some(*e)));
    }
    let base = ctx.pin_native_root(this);
    let staged_pin = ctx.pin_native_root(staged);

    if ctx.class_id_by_name("java/util/ArrayList").is_some() {
        if let Ok(Some(Value::Object(Some(list)))) =
            ctx.new_object_initialized("java/util/ArrayList", "()V", &[])
        {
            let list_pin = ctx.pin_native_root(list);
            let mut ok = true;
            for i in 0..elements.len() {
                let staged_now = ctx.read_native_pin(staged_pin, staged);
                let elem = ctx.get_array_element(staged_now, i);
                let list_now = ctx.read_native_pin(list_pin, list);
                if ctx
                    .invoke_virtual(list_now, "add", "(Ljava/lang/Object;)Z", &[elem])
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            if ok {
                let list_now = ctx.read_native_pin(list_pin, list);
                let this_now = ctx.read_native_pin(base, this);
                write_throwable_field(
                    ctx,
                    this_now,
                    "suppressedExceptions",
                    Value::Object(Some(list_now)),
                );
                ctx.unpin_native_roots(base);
                return;
            }
        }
    }
    // No usable `ArrayList` (a synthetic-JDK image), or the construction failed:
    // keep the pre-2026-08-29 array shape rather than lose the suppression.
    let this_now = ctx.read_native_pin(base, this);
    let staged_now = ctx.read_native_pin(staged_pin, staged);
    write_throwable_field(
        ctx,
        this_now,
        "suppressedExceptions",
        Value::Object(Some(staged_now)),
    );
    ctx.unpin_native_roots(base);
}

pub fn native_throwable_add_suppressed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `if (exception == this) throw new IllegalArgumentException(
    //      SELF_SUPPRESSION_MESSAGE, exception);`
    // then `Objects.requireNonNull(exception, NULL_CAUSE_MESSAGE);` — in that
    // order, so a self-argument is an IAE even though it is also non-null.
    let suppressed = match args.get(1) {
        Some(Value::Object(Some(r))) if *r == this => {
            return Err(throwable_refusal(
                ctx,
                "java/lang/IllegalArgumentException",
                "Self-suppression not permitted",
                Some(this),
            ));
        }
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // `Objects.requireNonNull` produces a message-only NPE with no
            // cause — measured: `npe.getCause()` is null on HotSpot.
            return Err(throwable_refusal(
                ctx,
                "java/lang/NullPointerException",
                "Cannot suppress a null exception.",
                None,
            ));
        }
    };
    // `if (suppressedExceptions == null) return;` — suppression disabled.
    if suppression_disabled(ctx, this) {
        return Ok(None);
    }
    // Read what is there, append, write the whole thing back in whichever shape
    // this image can hold. `suppressed_elements` reads the empty
    // SUPPRESSED_SENTINEL as zero elements, so the first suppression needs no
    // branch of its own.
    // gen r4w3/rooting: reading a List-shaped `suppressedExceptions` runs
    // `size()`/`get()` bytecode (GC points); `this` and `suppressed` are used
    // after it, so pin both and re-read.
    let this_pin = ctx.pin_native_root(this);
    let suppressed_pin = ctx.pin_native_root(suppressed);
    let mut elements = suppressed_elements(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    let suppressed = ctx.read_native_pin(suppressed_pin, suppressed);
    ctx.unpin_native_roots(this_pin);
    elements.push(suppressed);
    store_suppressed(ctx, this, &elements);
    Ok(None)
}

/// getSuppressed() — return Throwable[] from `suppressedExceptions`, or an
/// empty array if none were added. See `native_throwable_add_suppressed`'s
/// doc comment for why this reads by field NAME rather than a hardcoded
/// index.
///
/// The suppression-disabled case needs nothing extra here: the four-arg
/// constructor leaves `suppressedExceptions` **null**, which is not an array, so
/// the empty-array tail below is already the specified answer ("if suppression
/// was disabled … an empty array will be returned"). It only started reporting
/// that correctly once `addSuppressed` stopped writing an array into the null
/// field — the probe's `Throwable.suppressionDisabled` measured `1:0` because of
/// that write, not because of anything on this path.
pub fn native_throwable_get_suppressed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // A FRESH array every time. The JDK's body is
    // `suppressedExceptions.toArray(EMPTY_THROWABLE_ARRAY)`, which allocates;
    // this used to hand back the stored array itself, so a caller that wrote
    // into the array it was given edited the throwable:
    //
    //   t.addSuppressed(x); a = t.getSuppressed(); a[0] = null;
    //     t.getSuppressed()[0]   HotSpot  java.lang.IllegalStateException
    //                            was      null
    //
    // MEASURED by `ThrowableFamilySweep.java`. The empty case is
    // the one place a shared array WOULD be faithful -- HotSpot returns its
    // `EMPTY_THROWABLE_ARRAY` constant -- but nothing observable depends on
    // that identity, so one allocation path is enough.
    let elements = suppressed_elements(ctx, this);
    let element_class = ctx
        .class_id_by_name("java/lang/Throwable")
        .unwrap_or(ClassId::new(0));
    // i7-L2: `elements` are raw references held in a Rust `Vec` across the
    // array allocation, which can move them; root each and store the current
    // address.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let handles: Vec<_> = elements.into_iter().map(|e| scope.root(e)).collect();
    let arr = scope.new_ref_array(element_class, handles.len());
    for (i, h) in handles.iter().enumerate() {
        let e = scope.get(h);
        scope.set_array_element(arr, i, Value::Object(Some(e)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// getStackTrace() — return StackTraceElement[] from captured stack trace
pub fn native_throwable_get_stack_trace_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // ES-FAIL-06 (proper fix): honor an explicitly-set `stackTrace` array.
    // `RandomizedRunner.augmentStackTrace()` prepends a synthetic
    // `__randomizedtesting.SeedInfo.seed(...)` frame via `setStackTrace()`, and
    // the JDK stores the (cloned) array in the `stackTrace` field. Previously
    // this native ALWAYS re-materialised from the captured backtrace, dropping
    // that augmentation and yielding length/content mismatches (the masked
    // ES-suite failures). The JDK sentinel `UNASSIGNED_STACK` is a zero-length
    // array, so a non-empty `stackTrace` field means it was set (or cached) and
    // must be returned verbatim instead of re-deriving from the backtrace.
    // "Set" is decided by identity against `UNASSIGNED_STACK`, not by length:
    // see [`explicitly_set_stack_trace`], shared with the native printer.
    if let Some(set_arr) = explicitly_set_stack_trace(ctx, this) {
        if crate::nbflags().dbg_sttrace {
            let n = ctx.array_length(set_arr);
            for i in 0..n {
                if let Value::Object(Some(e)) = ctx.get_array_element(set_arr, i) {
                    let cn = ctx.get_field_by_name(e, "declaringClass");
                    eprintln!("[STTRACE set-array] [{i}] declaringClass={cn:?}");
                } else {
                    eprintln!("[STTRACE set-array] [{i}] = NULL ELEMENT");
                }
            }
        }
        // Interpreter round i3 wave 77, lane L2
        // (`CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`): `getStackTrace()` is
        // `getOurStackTrace().clone()` -- a NEW array every call, so a caller
        // that edits it does not edit the throwable's trace
        // (`interpreter-i76-L2-compatible-get-stack-trace-returns-the-set-array-itself-20261109-FIXED-20261110.md`).
        if ctx.compat_stack_trace_keeps_array() {
            let copy = stack_trace_array_copy(ctx, set_arr)?;
            return Ok(Some(Value::Object(Some(copy))));
        }
        return Ok(Some(Value::Object(Some(set_arr))));
    }
    if crate::nbflags().dbg_sttrace {
        eprintln!(
            "STTRACE_DBG_GET_ARRAY this={:?} hash={}",
            this.as_ptr(),
            ctx.identity_hash_code(this)
        );
    }
    // The captured trace, owned (its lines resolved) and iterated in place:
    // interpreter round i3 wave 76, lane L2 dropped the second, tuple-by-tuple
    // `Arc` copy taken "to avoid a borrow conflict" -- nothing is borrowed.
    // Keyed by the object: see `NativeExceptionAccess::get_throwable_stack_trace`.
    let source = throwable_trace_source(ctx, this);
    let trace_data = ctx.get_throwable_stack_trace(source).unwrap_or_default();
    let name_from_text = ctx.stack_trace_fill_name_from_text();
    let keeps_array = ctx.compat_stack_trace_keeps_array();

    let len = trace_data.len();
    // The array outlives one allocation per frame (the element, its three
    // strings, possibly a class mirror), so it cannot be held as an address.
    // Wave 77 (`CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`): neither can the
    // throwable the built array is then stored on, rooted before anything
    // below can collect (the class lookup can initialise a class).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = keeps_array.then(|| scope.root(this));
    let ste_cid = cached_ste_class_id(&mut *scope);
    // gcg g1b/o (`gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`,
    // door 3): the array collects once before the OutOfMemoryError, as
    // HotSpot's `Throwable.getStackTrace` (Java, `clone()`) does. Nothing but
    // the scope's (still empty) handles is held here, so the reclaim is legal.
    // With one attempt, the first `getStackTrace()` after a program caught an
    // OOME and dropped its data threw on `--compatible` G1 (vg1
    // `o1_stage_cpt_g1`: `native reference array of length 0`).
    let arr_obj = crate::util_concurrent_ext::new_ref_array_reclaiming(
        &mut *scope,
        ste_cid,
        len,
        &mut [],
        "Throwable.getStackTrace",
    )?;
    let arr_h = scope.root(arr_obj);
    let memo_on = scope.stack_trace_fill_switches().origin_memo;
    let mut origin_memo = SteOriginMemo::new(&arr_h, memo_on);
    // The element class's layout, resolved once for the array (interpreter
    // round i1 wave 49, lane L7; see `native_init_stack_trace_elements`).
    let ste_allocator = if trace_data.is_empty() {
        None
    } else {
        Some(
            crate::util_concurrent_ext::ConcurrentSyntheticAllocator::new(
                &mut *scope,
                "java/lang/StackTraceElement",
                4,
            )?,
        )
    };
    // stored trace is outermost-first; getStackTrace() wants index 0 = the
    // throw site (innermost), so fill the array reversed.
    for (i, entry) in trace_data.iter().rev().enumerate() {
        let Some(allocator) = &ste_allocator else {
            break;
        };
        // gcg g6/oo (door 4 of `gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`):
        // a refused element collects and retries before the catchable
        // `OutOfMemoryError`, as HotSpot's `StackTraceElement` allocation does.
        // Safe: what the loop uses after the call is rooted in `scope` or is
        // Rust data.
        let ste = allocator.alloc_reclaiming(&mut *scope, "java/lang/StackTraceElement")?;
        // The element itself crosses `fill_stack_trace_element`, which
        // allocates three strings and possibly a mirror.
        let ste_h = scope.root(ste);
        let cls_slashed: &str = &entry.class_name;
        let cls_dotted = captured_frame_dotted_name(&*scope, name_from_text, cls_slashed);
        origin_memo.at(i);
        fill_captured_stack_trace_element_memo(
            &mut *scope,
            ste,
            entry.class_id,
            cls_slashed,
            &cls_dotted,
            &entry.method_name,
            entry.source_file.as_deref(),
            entry.line_number,
            &mut origin_memo,
        );
        let arr = scope.get(&arr_h);
        let ste = scope.get(&ste_h);
        scope.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    let arr = scope.get(&arr_h);
    if crate::nbflags().dbg_sttrace {
        let n = scope.array_length(arr);
        for i in 0..n {
            if let Value::Object(Some(e)) = scope.get_array_element(arr, i) {
                let cn = scope.get_field_by_name(e, "declaringClass");
                eprintln!("[STTRACE materialized] [{i}] declaringClass={cn:?}");
            } else {
                eprintln!("[STTRACE materialized] [{i}] = NULL ELEMENT");
            }
        }
    }
    if let Some(this_h) = &this_h {
        if let Some(copy) = keep_built_stack_trace(&mut scope, this_h, &arr_h)? {
            return Ok(Some(Value::Object(Some(copy))));
        }
    }
    let arr = scope.get(&arr_h);
    Ok(Some(Value::Object(Some(arr))))
}

/// `stackTrace.clone()` for a `StackTraceElement[]` `src`: a new array of
/// the same length holding the same element references (a shallow copy, as
/// the JDK's array clone). Collects and retries once before the catchable
/// `OutOfMemoryError`, as the clone's allocation does; `src` crosses that
/// in the reclaim's live set.
fn stack_trace_array_copy(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let len = ctx.array_length(src);
    let mut live = [src];
    // Before the allocation: the class lookup can initialise the class.
    let ste_cid = {
        let pin = ctx.pin_native_root(live[0]);
        let cid = cached_ste_class_id(ctx);
        live[0] = ctx.read_native_pin(pin, live[0]);
        ctx.unpin_native_roots(pin);
        cid
    };
    let copy = crate::util_concurrent_ext::new_ref_array_reclaiming(
        ctx,
        ste_cid,
        len,
        &mut live,
        "Throwable.getStackTrace",
    )?;
    let src = live[0];
    for i in 0..len {
        let element = ctx.get_array_element(src, i);
        ctx.set_array_element(copy, i, element);
    }
    Ok(copy)
}

/// Interpreter round i3 wave 77, lane L2 (`CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the JDK's `getOurStackTrace()` state machine for the native
/// `getStackTrace()`. The throwable `this_h` roots had its array built from
/// the captured trace (`arr_h`); when its `stackTrace` is still unassigned
/// -- the `UNASSIGNED_STACK` sentinel, or `null` (the native constructors
/// skip the field initialiser; the JDK body tests `stackTrace == null` too)
/// -- and it has a `backtrace` (the JDK body fills only then), the built
/// array is stored in `stackTrace` and a copy is answered (`Some`). Later
/// calls take the explicitly-set branch, which copies too; `fillInStackTrace()`
/// (the JDK's body under `--compatible`) resets the field to the sentinel,
/// so a refill is seen. `None` (nothing stored; the caller answers the
/// built array, as before) for a throwable whose trace is not unassigned or
/// that has no `backtrace`, or on a JDK without the sentinel.
fn keep_built_stack_trace(
    scope: &mut NativeHandleScope<'_>,
    this_h: &cratonvm_native_api::NativeHandle,
    arr_h: &cratonvm_native_api::NativeHandle,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    let Some(unassigned) = unassigned_stack_sentinel(&mut **scope) else {
        return Ok(None);
    };
    let this = scope.get(this_h);
    let unset = match throwable_field_get(&mut **scope, this, "stackTrace") {
        Value::Object(None) => true,
        Value::Object(Some(current)) => current == unassigned,
        _ => false,
    };
    let this = scope.get(this_h);
    let has_backtrace = matches!(
        throwable_field_get(&mut **scope, this, "backtrace"),
        Value::Object(Some(_))
    );
    if !unset || !has_backtrace {
        return Ok(None);
    }
    let this = scope.get(this_h);
    let arr = scope.get(arr_h);
    throwable_field_set(&mut **scope, this, "stackTrace", Value::Object(Some(arr)));
    let arr = scope.get(arr_h);
    stack_trace_array_copy(&mut **scope, arr).map(Some)
}

pub fn native_throwable_set_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // MEASURED -- `javap -c java.lang.Throwable`. The first instructions of
    // `setStackTrace` are `aload_1; invokevirtual clone; checkcast`, then a
    // scan that throws `NullPointerException("stackTrace[" + i + "]")`. Both
    // happen BEFORE the monitor and before the store, so both refusals are
    // owed even by a throwable whose stack trace is not writable.
    //
    // This body stored the caller's array reference verbatim and checked
    // nothing, so all three rows were wrong at once:
    //
    //   t.setStackTrace(null)                       HotSpot NPE   was a no-op
    //   t.setStackTrace(new StackTraceElement[]{null})
    //                                               HotSpot NPE   was a no-op
    //   a = {..}; t.setStackTrace(a); a[0] = other;
    //     t.getStackTrace()[0]                      HotSpot the ORIGINAL
    //                                               was `other`
    //
    // MEASURED by `ThrowableFamilySweep.java`.
    let Some(Value::Object(Some(src))) = args.get(1).copied() else {
        // `stackTrace.clone()` on a null argument: the implicit NPE from the
        // `invokevirtual`, carrying HotSpot's helpful message. This is a
        // site-specific LITERAL, not an implementation of helpful NPEs -- both
        // halves of the text are fixed by `setStackTrace`'s own signature (the
        // receiver expression is its parameter `stackTrace`, the method is
        // `[Ljava/lang/StackTraceElement;.clone()`), so there is nothing here
        // for a general mechanism to compute. MEASURED on HotSpot 25.0.4+7 by
        // `ThrowableFamilySweep.java`.
        return Err(throwable_refusal(
            ctx,
            "java/lang/NullPointerException",
            "Cannot invoke \"[Ljava.lang.StackTraceElement;.clone()\" because \"stackTrace\" is null",
            None,
        ));
    };
    let len = ctx.array_length(src);
    for i in 0..len {
        if !matches!(ctx.get_array_element(src, i), Value::Object(Some(_))) {
            return Err(throwable_refusal(
                ctx,
                "java/lang/NullPointerException",
                &format!("stackTrace[{i}]"),
                None,
            ));
        }
    }
    // `synchronized (this) { if (this.stackTrace == null && backtrace ==
    // null) return; ... }`: an immutable stack (the `writableStackTrace ==
    // false` constructor) keeps its empty trace. This stored the copy, so
    // `getStackTrace().length` answered 1 where HotSpot answers 0.
    if throwable_native_trace_field() && stack_trace_is_immutable(ctx, this) {
        return Ok(None);
    }
    let element_class = ctx
        .class_id_by_name("java/lang/StackTraceElement")
        .unwrap_or(ClassId::new(0));
    // gc-common w19-e: the copy's allocation can collect; the source array
    // was copied from, and the receiver written, through their addresses from
    // before it.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let src_h = scope.root(src);
    let copy = scope.new_ref_array(element_class, len);
    let src = scope.get(&src_h);
    for i in 0..len {
        let elem = scope.get_array_element(src, i);
        scope.set_array_element(copy, i, elem);
    }
    let this = scope.get(&this_h);
    throwable_field_set(&mut *scope, this, "stackTrace", Value::Object(Some(copy)));
    Ok(None)
}

// ===========================================================================
// java.lang.Enum
// ===========================================================================

pub fn register_enum_natives(r: &mut NativeMethodRegistry) {
            let e = "java/lang/Enum";
    r.register(e, "<init>", "(Ljava/lang/String;I)V", native_enum_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(e, "ordinal", "()I", native_enum_ordinal, cratonvm_native_api::NativeKind::Bridge);
    r.register(e, "name", "()Ljava/lang/String;", native_enum_name, cratonvm_native_api::NativeKind::Bridge);
    r.register(e, "toString", "()Ljava/lang/String;", native_enum_name, cratonvm_native_api::NativeKind::Bridge); // delegates to name()
    r.register(
        e,
        "compareTo",
        "(Ljava/lang/Enum;)I",
        native_enum_compare_to, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(e, "equals", "(Ljava/lang/Object;)Z", native_enum_equals, cratonvm_native_api::NativeKind::Bridge);
    r.register(e, "hashCode", "()I", native_enum_hash_code, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        e,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_enum_get_declaring_class, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// Enum.<init>(String name, int ordinal) — store name in field 0, ordinal in field 1
pub fn native_enum_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [this, name(String ref), ordinal(int)]
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let name_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let ordinal = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    ctx.set_field(this, 0, name_val);
    ctx.set_field(this, 1, Value::Int(ordinal));
    Ok(None)
}

/// Slot of `java.lang.Enum`'s own `name` field. `Enum` declares `name` then
/// `ordinal`, and inherited fields come first in the layout, so these hold for
/// any enum subclass regardless of what fields IT declares — which is the
/// whole point: see `native_enum_name` for what resolving `"name"` on the
/// receiver's class instead cost.
///
/// E36-1 N4: `pub` since 2026-09-20, so `phases_late/nio_file.rs` can
/// import these instead of keeping its own copy of the same two numbers. Four
/// copies of this pair existed at one point (`lang_misc.rs`, `stack_walker.rs`,
/// `phases_late/net_channels.rs`, `phases_late/nio_file.rs`) — the shape that
/// let one of them (`nio_file.rs`'s) be inverted for a time without anything
/// noticing. `stack_walker.rs` and `net_channels.rs` no longer keep their own
/// copies; `nio_file.rs`'s is the one this promotion retires.
pub const ENUM_NAME_SLOT: usize = 0;

/// Slot of `java.lang.Enum`'s own `ordinal` field. See [`ENUM_NAME_SLOT`].
pub const ENUM_ORDINAL_SLOT: usize = 1;

/// Enum.ordinal() — return field 1 (int)
pub fn native_enum_ordinal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, ENUM_ORDINAL_SLOT)))}

/// Enum.name() and Enum.toString() — return `java.lang.Enum`'s OWN `name`.
///
/// `java/lang/Enum` declares `name` first and `ordinal` second, so the constant
/// name is slot 0 (`native_enum_compare_to` reads ordinal at slot 1 on the same
/// assumption). Both methods are registered as `()Ljava/lang/String;`, so this
/// must never return a primitive `Value`, and two things could make it:
///
/// * the slot-0 read is descriptor-decoded only when the receiver's class
///   metadata resolves the descriptor for slot 0 (`vm_exec.rs:8605`); for a
///   synthetic stand-in with no resolvable layout it degrades to the raw read,
///   and a zeroed slot decodes as `Value::Int(0)` — not `Object(None)`
///   (`gc/src/heap.rs:398-411`);
/// * an `Enum` allocated but never `<init>`-ed has an unwritten `name`.
///
/// Handing `Int(0)` to bytecode about to `areturn`/`checkcast` a `String` is
/// unsound either way, so any non-reference tag degrades to null.
///
/// **Do NOT resolve `"name"` on the RECEIVER's class.** That was tried
/// (`5bc7458e4`) and is wrong: `resolve_field_index_by_class_id` returns the
/// MOST-DERIVED declaration (see §6.2 of
/// `docs/feature-designs/by-name-field-reads.md`), and an enum may declare its own
/// field called `name`, which shadows `Enum`'s. Spring Boot's
/// `WebEndpointTest.Infrastructure` does exactly that — `JERSEY("Jersey")`,
/// `MVC("WebMvc")`, `WEBFLUX("WebFlux")` — so `name()` answered `"Jersey"`
/// instead of `"JERSEY"`. `Enum.valueOf` matches on `name()`, so it then threw
/// `IllegalArgumentException: No enum constant JERSEY`, the annotation
/// machinery could not resolve the enum-valued attribute, and JUnit rejected
/// the whole class with `PreconditionViolationException: displayName must not
/// be null or blank` — zero tests run, before any Spring context started.
/// `probes/EnumShadowedNameProbe.java` pins it.
pub fn native_enum_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(enum_constant_name(ctx, this)))}

/// Read an enum constant's JVM name — `java.lang.Enum`'s OWN `name` field.
///
/// The single reader behind `Enum.name()` and `Enum.toString()`, wherever they
/// are registered. It existed as two independent copies in `lang_misc` and in
/// `lib.rs`, and only the `lib.rs` pair is actually registered, so a fix
/// applied to the other one is inert — which is exactly how this defect
/// survived a first repair attempt.
///
/// Resolution is scoped to `java/lang/Enum` (like the `ordinal` native beside
/// it), never to the receiver's class: see [`native_enum_name`] for what the
/// receiver-scoped read cost. Slot 0 is the fallback — `Enum` declares `name`
/// then `ordinal`, and `native_enum_init` writes those two slots directly.
pub fn enum_constant_name(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let slot = ctx
        .resolve_field_index("java/lang/Enum", "name")
        .unwrap_or(ENUM_NAME_SLOT);
    match ctx.get_field(this, slot) {
        v @ Value::Object(_) => v,
        // `()Ljava/lang/String;` must never surface a primitive tag: an
        // unwritten reference slot reads back as `Value::Int(0)`, and handing
        // that to bytecode about to `areturn`/`checkcast` a String is unsound.
        _ => Value::Object(None),
    }
}

/// Enum.compareTo(Enum other) — this.ordinal - other.ordinal
pub fn native_enum_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_ord = match ctx.get_field(this, 1) {
        Value::Int(i) => i,
        _ => 0,
    };
    let other_ord = match ctx.get_field(other, 1) {
        Value::Int(i) => i,
        _ => 0,
    };
    Ok(Some(Value::Int(this_ord - other_ord)))}

/// Enum.equals(Object) — identity comparison (reference equality)
pub fn native_enum_equals(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let eq = match (this, other) {
        (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
        _ => false,
    };
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))}

/// Enum.hashCode() — identity hash code
pub fn native_enum_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.identity_hash_code(this))))}

/// Enum.getDeclaringClass() — return Class mirror for this enum's class
pub fn native_enum_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = ctx.class_id_of_object(this);
    let mirror = ctx.get_class_mirror(class_id);
    Ok(Some(Value::Object(Some(mirror))))
}

// --- StackTraceElement ---
const STE_FIELD_CLASS: usize = 0;
const STE_FIELD_METHOD: usize = 1;
const STE_FIELD_FILE: usize = 2;
const STE_FIELD_LINE: usize = 3;

/// Read a logical StackTraceElement field, layout-aware.
///
/// Real-JDK `StackTraceElement` keeps the class-name String in the named
/// field `declaringClass` (slot 4, after `declaringClassObject` and the
/// module/loader strings), not slot 0. The legacy synthetic-stub layout
/// uses raw slots `[class, method, file, line]`. Prefer the named field
/// when the class carries it; fall back to the raw slot otherwise.
pub fn ste_read_field(
    ctx: &dyn NativeContext,
    ste: ObjectRef,
    named: &str,
    raw_slot: usize,
) -> Value {
    if ctx
        .resolve_field_index("java/lang/StackTraceElement", "declaringClass")
        .is_some()
    {
        ctx.get_field_by_name(ste, named)
    } else {
        ctx.get_field(ste, raw_slot)
    }
}

pub fn native_ste_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // The 4-arg ctor is `(declaringClass, methodName, fileName, lineNumber)`.
    // Decode the String args and route through the shared layout-aware
    // filler so real-JDK STEs get `declaringClass`/`methodName`/... in the
    // correct named slots (and `declaringClassObject` populated).
    let class_dotted = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let method = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let file = match args.get(3) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let line = match args.get(4) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    let class_slashed = class_dotted.replace('.', "/");
    // The public constructor names no module or loader (round 13 wave 13).
    fill_stack_trace_element_as(
        ctx,
        this,
        &class_slashed,
        &class_dotted,
        &method,
        file.as_deref(),
        line,
        false,
        None,
        None,
        None,
    );
    Ok(None)
}

pub fn native_ste_get_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(
        ctx,
        this,
        "declaringClass",
        STE_FIELD_CLASS,
    )))
}

pub fn native_ste_get_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(
        ctx,
        this,
        "methodName",
        STE_FIELD_METHOD,
    )))
}

pub fn native_ste_get_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(ctx, this, "fileName", STE_FIELD_FILE)))}

pub fn native_ste_get_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(ste_read_field(
        ctx,
        this,
        "lineNumber",
        STE_FIELD_LINE,
    )))
}

pub fn native_ste_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class = match ste_read_field(ctx, this, "declaringClass", STE_FIELD_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "Unknown".to_string(),
    };
    let method = match ste_read_field(ctx, this, "methodName", STE_FIELD_METHOD) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "unknown".to_string(),
    };
    let file = match ste_read_field(ctx, this, "fileName", STE_FIELD_FILE) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "Unknown Source".to_string(),
    };
    let line = match ste_read_field(ctx, this, "lineNumber", STE_FIELD_LINE) {
        Value::Int(v) => v,
        _ => -1,
    };
    let s = if line >= 0 {
        format!("{}.{}({}:{})", class, method, file, line)
    } else {
        format!("{}.{}({})", class, method, file)
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn register_phase53_record(r: &mut NativeMethodRegistry) {
            let rec = "java/lang/Record";
    // Records are just normal classes with some special semantics
    // We register equals/hashCode/toString stubs that work via fields.
    //
    // S107 fix (constructor_probe test 10): the no-arg `<init>()V` is void,
    // so it must return `Ok(None)`. Returning `Ok(Some(Value::Object(None)))`
    // pushes a stray null onto the operand stack, which silently corrupts
    // the caller record's `<init>` frame — for compact-canonical records
    // with a validation body, this can shift max_stack and cause the
    // throw branch to be skipped or mis-dispatched. See WP2.6.
    //
    // KEEP (genuinely empty, not a stub): `java.lang.Record` declares NO
    // instance fields and its sole constructor is `protected Record() {}` —
    // every record's canonical ctor chains here through `invokespecial` and the
    // real body does nothing but `super()`. Component fields are written by the
    // subclass ctor, never here. Empty is exact.
    r.register(rec, "<init>", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge);
    r.register(rec, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            // Records: must be same class, not just same field count
            let this_cid = ctx.class_id_of_object(this);
            let other_cid = ctx.class_id_of_object(*other);
            if this_cid != other_cid {
                return Ok(Some(Value::Int(0)));
            }
            let other = *other;
            // gen r4w3/rooting: each component `equals` upcall is a GC point;
            // pin both records and re-read them at the top of every iteration.
            let this_pin = ctx.pin_native_root(this);
            let other_pin = ctx.pin_native_root(other);
            let nf = ctx.object_num_fields(this);
            for i in 0..nf {
                let this = ctx.read_native_pin(this_pin, this);
                let other = ctx.read_native_pin(other_pin, other);
                let a = ctx.get_field(this, i);
                let b = ctx.get_field(other, i);
                // MED fix: per the JLS record-equality contract (and the
                // `java.lang.runtime.ObjectMethods` bootstrap HotSpot uses),
                // reference-typed components are compared with `Objects.equals`
                // (null-safe `a.equals(b)` via virtual dispatch), NOT by pointer
                // identity. Two records with equal-but-distinct String/boxed
                // components must be `equals`. Primitive components compare by
                // value; `float`/`double` use bitwise (`Float`/`Double.compare`)
                // semantics to match the generated canonical `equals`.
                let component_equal = match (&a, &b) {
                    // Reference components: null-safe virtual `equals`.
                    (Value::Object(_), _) | (_, Value::Object(_)) => {
                        match (a, b) {
                            (Value::Object(None), Value::Object(None)) => true,
                            (Value::Object(None), _) | (_, Value::Object(None)) => false,
                            (Value::Object(Some(ra)), Value::Object(Some(rb))) => {
                                // Identity fast-path, then dispatch `ra.equals(rb)`.
                                if std::ptr::eq(ra.as_ptr(), rb.as_ptr()) {
                                    true
                                } else {
                                    match ctx.invoke_virtual(
                                        ra,
                                        "equals",
                                        "(Ljava/lang/Object;)Z",
                                        &[Value::Object(Some(rb))],
                                    ) {
                                        Ok(Some(Value::Int(v))) => v != 0,
                                        Ok(_) => false,
                                        Err(e) => {
                                            ctx.unpin_native_roots(this_pin);
                                            return Err(e);
                                        }
                                    }
                                }
                            }
                            // Mismatched kinds (a reference vs. a primitive slot)
                            // cannot occur for a well-formed record, but treat as
                            // not-equal rather than mis-comparing.
                            _ => false,
                        }
                    }
                    // Primitive components: compare by value. `float`/`double`
                    // use bitwise compare so `NaN==NaN` and `-0.0!=0.0`, matching
                    // the canonical generated `equals` (Float/Double.compare).
                    (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
                    (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
                    _ => a == b,
                };
                if !component_equal {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Int(0)));
                }
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rec, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let nf = ctx.object_num_fields(this);
        let mut hash: i32 = 0;
        // gen r4w3/rooting: each component `hashCode` upcall is a GC point;
        // pin the record and re-read it at the top of every iteration.
        let this_pin = ctx.pin_native_root(this);
        for i in 0..nf {
            let this = ctx.read_native_pin(this_pin, this);
            let v = ctx.get_field(this, i);
            // MED fix: derive a reference component's contribution from its
            // virtual `hashCode()` (null -> 0), NOT its pointer address, so the
            // result is stable across equal-but-distinct components and honours
            // the record equals/hashCode contract. Primitives hash by value.
            let h = match v {
                Value::Int(n) => n,
                Value::Long(n) => (n ^ (n >> 32)) as i32,
                Value::Float(f) => f.to_bits() as i32,
                Value::Double(d) => {
                    let bits = d.to_bits();
                    (bits ^ (bits >> 32)) as i32
                }
                Value::Object(None) => 0,
                Value::Object(Some(r)) => {
                    // gen r4w3/rooting: the fallback `identity_hash_code(r)`
                    // runs after the upcall; pin `r` so it names the live copy.
                    let r_pin = ctx.pin_native_root(r);
                    let called = ctx.invoke_virtual(r, "hashCode", "()I", &[]);
                    let r = ctx.read_native_pin(r_pin, r);
                    ctx.unpin_native_roots(r_pin);
                    match called {
                        Ok(Some(Value::Int(hc))) => hc,
                        Ok(_) => ctx.identity_hash_code(r),
                        Err(e) => {
                            ctx.unpin_native_roots(this_pin);
                            return Err(e);
                        }
                    }
                }
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(h);
        }
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Int(hash)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rec, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cid = ctx.class_id_of_object(this);
        let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
        // Use simple name (after last '/')
        let simple = class_name.rsplit('/').next().unwrap_or(&class_name);
        let components = ctx.record_components(cid);
        if components.is_empty() {
            // Fallback for non-record or unknown
            let text = format!("{}@{:x}", simple, this.as_ptr() as usize);
            return Ok(Some(Value::Object(Some(ctx.create_string(&text)))));
        }
        let mut result = format!("{}[", simple);
        for (i, (name, descriptor)) in components.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            let val = ctx.get_field(this, i);
            let val_str = match val {
                Value::Int(n) => {
                    if descriptor == "Z" {
                        if n != 0 {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    } else if descriptor == "C" {
                        format!("{}", char::from_u32(n as u32).unwrap_or('?'))
                    } else {
                        n.to_string()
                    }
                }
                Value::Long(n) => n.to_string(),
                Value::Float(f) => f.to_string(),
                Value::Double(d) => d.to_string(),
                Value::Object(Some(obj)) => ctx
                    .read_string(obj)
                    .unwrap_or_else(|| format!("object@{:x}", obj.as_ptr() as usize)),
                Value::Object(None) => "null".to_string(),
                _ => "?".to_string(),
            };
            result.push_str(&format!("{}={}", name, val_str));
        }
        result.push(']');
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// =============================================================================
// java.lang.Record expansion — components, equals, hashCode, toString stubs
// =============================================================================

pub fn register_p60_record(r: &mut NativeMethodRegistry) {
            // Record equals/hashCode/toString already registered in earlier phase with proper
    // field-by-field comparison — only add RecordComponent here.

    // RecordComponent = 3-field (name=0, type=1, declaringRecord=2)
    let rc = "java/lang/reflect/RecordComponent";
    r.register(rc, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rc, "getType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        rc,
        "getDeclaringRecord",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// =============================================================================
// WP8.10.7 — Throwable.getMessage / printStackTrace on synthetic-stub
// Throwable subclasses
// =============================================================================
//
// Background: `getMessage()`, `getLocalizedMessage()` and `printStackTrace()`
// (both no-arg and `(PrintStream)V`) are declared on `java/lang/Throwable`,
// but the registry dispatch is keyed by class name — so a `catch (Throwable t) {
// t.getMessage(); }` whose `t` is a `NoClassDefFoundError` synthetic-stub will
// surface a secondary `NoSuchMethodError` because no native is registered under
// `java/lang/NoClassDefFoundError.getMessage`.
//
// Field layout (verified against
// `classloading/src/class_manager.rs::synthetic_stub_fields` arm at line 3373+):
//   slot 0 = detailMessage (String)
//   slot 1 = cause (Throwable)
// Each subclass is allocated with `instance_fields(2)` so the slot indexing
// is robust as long as that arm is not re-ordered.
//
// We intentionally re-register `java/lang/Throwable` itself too so the
// behavior is uniform across the family — `register` is last-write-wins
// and the new closure is a strict superset of the existing native (it
// reads slot 0, validates it's a string, and falls back to null).
/// The throwable-family classes whose Throwable surface this file bridges.
///
/// Module-level so the gate at the bottom of this file can walk it: every
/// constructor descriptor `cratonvm_classloading::throwable_ctor_descriptors`
/// declares for one of these must have a native here, or the synthetic stub
/// declares a method that resolves to nothing.
pub const THROWABLE_FAMILY_CLASSES: &[&str] = &[
    "java/lang/Throwable",
    "java/lang/Exception",
    "java/lang/RuntimeException",
    "java/lang/Error",
    "java/lang/LinkageError",
    "java/lang/NoClassDefFoundError",
    "java/lang/SecurityException",
    "java/lang/ReflectiveOperationException",
    "java/lang/ClassNotFoundException",
    "java/lang/NoSuchMethodError",
    "java/lang/NoSuchFieldError",
    "java/lang/NoSuchMethodException",
    "java/lang/NoSuchFieldException",
    "java/lang/CloneNotSupportedException",
    "java/lang/InstantiationException",
    "java/lang/IllegalAccessException",
    "java/lang/reflect/InaccessibleObjectException",
    "java/lang/reflect/InvocationTargetException",
    "java/lang/InterruptedException",
    "java/lang/NullPointerException",
    "java/lang/ArithmeticException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/IndexOutOfBoundsException",
    "java/lang/StringIndexOutOfBoundsException",
    "java/lang/ClassCastException",
    "java/lang/IllegalArgumentException",
    "java/lang/IllegalStateException",
    "java/lang/UnsupportedOperationException",
    "java/lang/TypeNotPresentException",
    "java/lang/StackOverflowError",
    "java/lang/OutOfMemoryError",
    "java/util/NoSuchElementException",
    "java/util/InputMismatchException",
    "java/util/MissingResourceException",
    "java/util/FormatterClosedException",
    "java/io/IOException",
    "java/io/FileNotFoundException",
    "java/io/UncheckedIOException",
    "java/io/NotSerializableException",
    // `java/io/InvalidClassException` is deliberately NOT in this list, for
    // the same reason `java/util/regex/PatternSyntaxException` is not: it
    // OVERRIDES `getMessage()`, prepending the offending class name to the
    // detail message. A blanket bridge in front of that override returns
    // the bare `Throwable.detailMessage`, so
    // `new InvalidClassException("com.example.Foo", "bad serialVersionUID")`
    // reported "bad serialVersionUID" where HotSpot reports
    // "com.example.Foo; bad serialVersionUID" -- and deserialization
    // diagnostics lose the one field that says WHICH class failed.
    //
    // Found 2026-08-05 by checking `javap -p` for a declared
    // getMessage/getLocalizedMessage/toString across every class in these
    // two lists; it and `NullPointerException` were the only two left after
    // PatternSyntaxException. See
    // `a-bridge-in-front-of-an-overridden-getmessage-FIXED-20260805.md`.
    "java/io/EOFException",
    "java/io/UnsupportedEncodingException",
    "java/net/MalformedURLException",
    "java/net/UnknownHostException",
    "java/lang/NumberFormatException",
    "java/util/ConcurrentModificationException",
    "java/util/concurrent/TimeoutException",
    "java/util/concurrent/RejectedExecutionException",
    "java/util/concurrent/CancellationException",
    "java/util/concurrent/CompletionException",
    "java/util/concurrent/ExecutionException",
    "java/util/concurrent/BrokenBarrierException",
    "java/text/ParseException",
    // `java/util/regex/PatternSyntaxException` is deliberately NOT in this
    // list. It is the one exception here that OVERRIDES `getMessage()`:
    // the JDK builds a three-line report ("Unclosed character class near
    // index 0", the pattern, a caret) from its `desc`/`pattern`/`index`
    // fields and never sets `Throwable.detailMessage`. A blanket
    // `getMessage` bridge in front of that override returns the null
    // `detailMessage`, so `"x".split("[")` reported `getMessage() == null`
    // where HotSpot gives the full report. Its real constructor is
    // `(String,String,int)V`, which is not among the `<init>` shapes
    // registered here either, so every bridge this loop would add is
    // either dead or actively wrong. Removed 2026-08-05.
    "java/lang/NegativeArraySizeException",
    "java/lang/AssertionError",
    "java/lang/MatchException",
    "java/lang/IncompatibleClassChangeError",
    "java/lang/IllegalAccessError",
    "java/lang/ExceptionInInitializerError",
    "java/lang/VerifyError",
    "java/lang/AbstractMethodError",
    "java/lang/InternalError",
    "java/lang/UnsatisfiedLinkError",
];

/// The native body for one `(class, constructor descriptor)` pair from
/// [`cratonvm_classloading::throwable_ctor_descriptors`].
///
/// `None` means the table names a descriptor this file cannot implement, which
/// is a bug in one of the two — the caller `debug_assert!`s on it rather than
/// registering a declaration with no body.
///
/// Most rows are class-independent: the four `Throwable` shapes mean the same
/// thing everywhere. The exceptions are the point of the table — the SAME
/// descriptor means different things on different classes. `(I)V` is
/// `String.valueOf(int)` on `AssertionError` and "Array index out of range: N"
/// on `ArrayIndexOutOfBoundsException`, so the body is chosen by class here
/// rather than sniffed from the receiver at call time.
fn throwable_ctor_native(cls: &str, descriptor: &str) -> Option<NativeCallback> {
    // Class-specific rows first: a later generic arm must not shadow them.
    match (cls, descriptor) {
        ("java/lang/AssertionError", "(Ljava/lang/Object;)V") => {
            return Some(native_assertion_error_init_object)
        }
        ("java/lang/AssertionError", "(Z)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Boolean))
        }
        ("java/lang/AssertionError", "(C)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Char))
        }
        ("java/lang/AssertionError", "(I)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Int))
        }
        ("java/lang/AssertionError", "(J)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Long))
        }
        ("java/lang/AssertionError", "(F)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Float))
        }
        ("java/lang/AssertionError", "(D)V") => {
            return Some(|ctx, args| assertion_error_init_scalar(ctx, args, ScalarKind::Double))
        }
        ("java/lang/ArrayIndexOutOfBoundsException", "(I)V") => {
            return Some(|ctx, args| {
                index_exception_init_index(ctx, args, "Array index out of range: ")
            })
        }
        ("java/lang/StringIndexOutOfBoundsException", "(I)V") => {
            return Some(|ctx, args| {
                index_exception_init_index(ctx, args, "String index out of range: ")
            })
        }
        ("java/lang/IndexOutOfBoundsException", "(I)V" | "(J)V") => {
            return Some(|ctx, args| index_exception_init_index(ctx, args, "Index out of range: "))
        }
        ("java/io/UncheckedIOException", "(Ljava/io/IOException;)V") => {
            return Some(native_unchecked_io_init_cause)
        }
        ("java/io/UncheckedIOException", "(Ljava/lang/String;Ljava/io/IOException;)V") => {
            return Some(native_unchecked_io_init_message_cause)
        }
        ("java/text/ParseException", "(Ljava/lang/String;I)V") => {
            return Some(native_parse_exception_init)
        }
        (
            "java/util/MissingResourceException",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        ) => return Some(native_missing_resource_init),
        ("java/lang/TypeNotPresentException", "(Ljava/lang/String;Ljava/lang/Throwable;)V") => {
            return Some(native_type_not_present_exception_init)
        }
        // `ExceptionInInitializerError` is NOT the generic cause-only shape.
        // MEASURED — `javap -c java.lang.ExceptionInInitializerError` on the
        // 25.0.4+7 image this VM runs against:
        //
        //   ExceptionInInitializerError(Throwable)
        //     aconst_null; aload_1; invokespecial LinkageError.<init>(String,Throwable)
        //   ExceptionInInitializerError()
        //     invokespecial LinkageError.<init>(); aload_0; aconst_null;
        //     invokevirtual initCause
        //
        // So the message is an explicit NULL, not `cause.toString()`, and the
        // no-arg form calls `initCause(null)` -- which leaves `cause` at a real
        // null rather than the `cause == this` sentinel, and therefore REFUSES
        // a later `initCause` with `IllegalStateException`. Routing it through
        // the generic arms got both wrong:
        //
        //   new ExceptionInInitializerError(new IllegalStateException("seed"))
        //     HotSpot  toString  java.lang.ExceptionInInitializerError
        //     was                java.lang.ExceptionInInitializerError: java.lang.IllegalStateException: seed
        //
        // MEASURED by `ThrowableFamilySweep.java`. There is no
        // `exception` FIELD to write on a modern image: `getException()` is
        // `return super.getCause()`, which the registered `getCause` already
        // answers, and `readObject` maps the serialized `exception` name onto
        // the cause. This is only about which super constructor runs.
        ("java/lang/ExceptionInInitializerError", "(Ljava/lang/Throwable;)V") => {
            return Some(native_exception_in_initializer_init_thrown)
        }
        // The three no-arg constructors that null the cause rather than write
        // the sentinel -- see [`native_exc_init_noargs_null_cause`] for the
        // disassembly of all 62 and why these are the only three.
        (
            "java/lang/ExceptionInInitializerError"
            | "java/lang/ClassNotFoundException"
            | "java/lang/reflect/InvocationTargetException",
            "()V",
        ) => return Some(native_exc_init_noargs_null_cause),
        (
            "java/lang/ExceptionInInitializerError" | "java/lang/ClassNotFoundException",
            "(Ljava/lang/String;)V",
        ) => return Some(native_exc_init_message_null_cause),
        // `InvocationTargetException` keeps the wrapped throwable in its own
        // `target` field, not in `Throwable.cause` — see those bodies.
        ("java/lang/reflect/InvocationTargetException", "(Ljava/lang/Throwable;)V") => {
            return Some(native_invocation_target_exception_init_target)
        }
        (
            "java/lang/reflect/InvocationTargetException",
            "(Ljava/lang/Throwable;Ljava/lang/String;)V",
        ) => return Some(native_invocation_target_exception_init_target_message),
        _ => {}
    }

    match descriptor {
        "()V" => Some(native_exc_init_noargs),
        "(Ljava/lang/String;)V" => Some(native_exc_init_message),
        "(Ljava/lang/String;Ljava/lang/Throwable;)V" => Some(native_exc_init_message_cause),
        "(Ljava/lang/Throwable;)V" => Some(native_exc_init_cause),
        _ => None,
    }
}

pub fn register_throwable_subclass_natives(r: &mut NativeMethodRegistry) {
            // Subset that surfaces in jboss-modules / WildFly catch-blocks (per
    // bench/wildfly-boot/diagnostic.md §WP8.10.7) plus the broader
    // Error/Exception families that any defensive catch will see.
    let throwable_classes = THROWABLE_FAMILY_CLASSES;

    for cls in throwable_classes.iter() {
        // The CONSTRUCTORS come from the measured per-class table, shared with
        // the synthetic stub's method table so the two cannot disagree about
        // which ones exist. See
        // `cratonvm_classloading::throwable_ctor_descriptors`.
        for descriptor in cratonvm_classloading::throwable_ctor_descriptors(cls) {
            if let Some(body) = throwable_ctor_native(cls, descriptor) {
                r.register(cls, "<init>", descriptor, body, cratonvm_native_api::NativeKind::Bridge);
            } else {
                debug_assert!(
                    false,
                    "{cls} declares <init>{descriptor} with no native to back it"
                );
            }
        }

        if *cls == "java/lang/reflect/InvocationTargetException" {
            r.register(
                cls,
                "getMessage",
                "()Ljava/lang/String;",
                native_throwable_get_message, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "getLocalizedMessage",
                "()Ljava/lang/String;",
                native_throwable_get_message, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "printStackTrace",
                "()V",
                native_throwable_print_stack_trace, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "printStackTrace",
                "(Ljava/io/PrintStream;)V",
                native_throwable_print_stack_trace_to_stream, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "printStackTrace",
                "(Ljava/io/PrintWriter;)V",
                native_throwable_print_stack_trace_to_stream, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "toString",
                "()Ljava/lang/String;",
                native_throwable_to_string, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "getCause",
                "()Ljava/lang/Throwable;",
                native_invocation_target_exception_get_target, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "getTargetException",
                "()Ljava/lang/Throwable;",
                native_invocation_target_exception_get_target, cratonvm_native_api::NativeKind::Bridge,
            );
            r.register(
                cls,
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                native_throwable_init_cause, cratonvm_native_api::NativeKind::Bridge,
            );
            continue;
        }
        // The constructors were registered from the per-class table at the top
        // of this loop. They used to be four hard-coded `r.register` calls
        // here — `()V`, `(String)V`, `(String,Throwable)V`, `(Throwable)V` —
        // for EVERY class in the list, which is 103 descriptors the real JDK
        // class does not declare and 16 it does that nobody registered.
        //
        // Older still, and worth keeping: audit-2026-05-16 replaced a generic
        // `native_noop_with_this` behind those four descriptors that left
        // `cause` un-initialised. The (String) ctor writes the JDK sentinel
        // `cause = this`, so a later `initCause()` succeeded after a (String)
        // ctor and failed after the no-arg one.

        // getMessage()Ljava/lang/String; — read slot 0 (detailMessage).
        r.register(
            cls,
            "getMessage",
            "()Ljava/lang/String;",
            native_throwable_get_message, cratonvm_native_api::NativeKind::Bridge,
        );
        // getLocalizedMessage()Ljava/lang/String; — JDK delegates to virtual
        // getMessage by default.
        r.register(
            cls,
            "getLocalizedMessage",
            "()Ljava/lang/String;",
            native_throwable_get_localized_message, cratonvm_native_api::NativeKind::Bridge,
        );
        // printStackTrace()V — no-arg overload, prints to System.err equivalent.
        r.register(
            cls,
            "printStackTrace",
            "()V",
            native_throwable_print_stack_trace, cratonvm_native_api::NativeKind::Bridge,
        );
        // printStackTrace(Ljava/io/PrintStream;)V — JDK 25 overload.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintStream;)V",
            native_throwable_print_stack_trace_to_stream, cratonvm_native_api::NativeKind::Bridge,
        );
        // printStackTrace(Ljava/io/PrintWriter;)V — same shape, same sink.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintWriter;)V",
            native_throwable_print_stack_trace_to_stream, cratonvm_native_api::NativeKind::Bridge,
        );
        // toString()Ljava/lang/String; — "ClassName: message".
        r.register(
            cls,
            "toString",
            "()Ljava/lang/String;",
            native_throwable_to_string, cratonvm_native_api::NativeKind::Bridge,
        );
        // getCause()Ljava/lang/Throwable; — read slot 1.
        r.register(
            cls,
            "getCause",
            "()Ljava/lang/Throwable;",
            native_throwable_get_cause, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "initCause",
            "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
            native_throwable_init_cause, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "addSuppressed",
            "(Ljava/lang/Throwable;)V",
            native_throwable_add_suppressed, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getSuppressed",
            "()[Ljava/lang/Throwable;",
            native_throwable_get_suppressed, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getStackTrace",
            "()[Ljava/lang/StackTraceElement;",
            native_throwable_get_stack_trace_array, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "setStackTrace",
            "([Ljava/lang/StackTraceElement;)V",
            native_throwable_set_stack_trace, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

/// printStackTrace(Ljava/io/PrintStream;)V (and PrintWriter overload).
///
/// Args layout: [this, stream]. We honour the requested sink: when `stream`
/// is the `System.out` static we route to stdout (fd 1) so
/// `t.printStackTrace(System.out)` lands on stdout exactly like HotSpot;
/// otherwise (the common `System.err` case, or any stream we can't map) we
/// keep the stderr sink (fd 2). Output is always also mirrored into the
/// recorded-line buffer so tests scanning `thread.printed_lines` keep working.
///
/// Null-safe: if `this` is null we no-op; if `stream` is null we still
/// print to stderr, because catch-block code paths frequently call
/// `t.printStackTrace(System.err)` and the synthetic-stub System.err
/// static may itself be null on early boot — we don't want to throw a
/// secondary NPE inside a catch handler. Once `System.err` is set, a null
/// stream is the caller's own and throws, as the JDK does.
pub fn native_throwable_print_stack_trace_to_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Honour the requested stream: `printStackTrace(System.out)` must land on
    // stdout (fd 1), not the stderr sink. Any other / unmappable stream
    // (notably `System.err`) keeps fd 2.
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => Some(*s),
        _ => None,
    };
    // An explicit null stream once `System.err` exists is the caller's null,
    // and the JDK's `printStackTrace(PrintStream/PrintWriter)` throws
    // `NullPointerException` for it (round 13 wave 12, lane misc12,
    // [`throwable_native_trace_field`]). Only the early-boot case below keeps
    // the stderr fallback.
    if stream.is_none() && throwable_native_trace_field() && system_err_is_set(ctx) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    match stream {
        // Null stream (e.g. System.err still null on early boot): fall back to
        // the stderr fd so a catch-handler's printStackTrace never NPEs.
        None => print_throwable_chain_to_fd(ctx, this, 2)?,
        // A CANONICAL (unwrapped) System.out / System.err: keep the
        // battle-tested host-fd sink so boot-time exception dumps stay
        // visible exactly as before. But `System.out`/`err` can be
        // REDIRECTED (`System.setOut(tee)` — Spring Boot's
        // `OutputCaptureExtension`/`CapturedOutput` does exactly this for
        // every test using it, e.g.
        // `SpringApplicationTests.failureInANativeImageWritesFailureToSystemOut`,
        // whose `NativeDetector.inNativeImage()` branch does
        // `System.out.println("Application run failed");
        // failure.printStackTrace(System.out)`); at that point the current
        // `System.out` VALUE is the tee stream, and `is_system_out_or_err`
        // correctly says "yes, this IS System.out" — but writing straight to
        // the raw host fd bypasses that tee's Java-level buffer entirely, so
        // the capturing extension's assertion sees the leading
        // println but NONE of the exception detail. `stream_writeln` (the
        // `println` native path) avoids exactly this trap via
        // `route_write_through_out` — mirror that here: a non-null `out`
        // delegate field means the stream is wrapped, so route through the
        // object's own `println` (which itself is capture-aware) instead of
        // the fd fast path.
        Some(s)
            if is_system_out_or_err(ctx, s)
                && !matches!(ctx.get_field_by_name(s, "out"), Value::Object(Some(_))) =>
        {
            let fd = print_stream_target_fd(ctx, Some(s));
            print_throwable_chain_to_fd(ctx, this, fd)?;
        }
        // Any other stream — including a WRAPPED System.out/err, and any
        // user sink the fd write cannot reach (e.g. a
        // ByteArrayOutputStream-backed PrintWriter). Drive it through the
        // object's own println so the trace is actually captured — this is
        // the path Spring's AggressiveFactoryBeanInstantiationTests.checkLinkageError
        // and every log-to-string idiom depends on.
        Some(s) => print_throwable_chain_to_stream_obj(ctx, this, s)?,
    }
    Ok(None)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod throwable_ctor_table_tests;

/// Round 12 wave 8 (lane compat2): an application `fillInStackTrace()`
/// override runs in `Throwable.<init>`'s order and its exception leaves `new`.
/// The shadows are mode-independent code (they serve the throwable
/// constructors wherever the registry routes them -- `--compatible`, and
/// `--real-jdk` for an app class whose `super` is a shadowed JDK constructor
/// the dispatcher keeps native), so these tests have no mode axis.
#[cfg(test)]
mod r12_compat2_fill_override_order_tests;

#[cfg(test)]
mod r13_mhffm_fill_override_bridge_tests;

/// Round 13 wave 12, lane misc12: the native printer's frame text follows a
/// trace `setStackTrace` stored, not the capture store
/// (`r13w12-shadow4-throwable-native-print-ignores-set-stack-trace-FIXED-20260929.md`).
/// The mock models a REAL-layout `java/lang/Throwable` (its six instance
/// fields and the `UNASSIGNED_STACK` static), so the identity rule is the one
/// under test; the mock has no capture store, so "captured frames" is empty.
#[cfg(test)]
mod r13_misc12_set_stack_trace_print_tests;

/// Interpreter round i3 wave 77, lane L2
/// (`CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`): the native
/// `Throwable.getStackTrace()` follows the JDK's `getOurStackTrace().clone()`
/// state machine with the switch on, and answers as before with it off. The
/// mock models the real-layout `Throwable` of the module above; it has no
/// capture store, so a built array is empty (identity is what is under test).
#[cfg(test)]
mod i77_l2_keeps_array_tests;

/// Round 13 wave 13 (lane trace3): the native printer's cause loop and the
/// shared frame prefix.
#[cfg(test)]
mod r13_trace3_print_tests;

/// nio21 wave 12 lane L5, `CRATONVM_THROWABLE_PRINT_VIRTUAL_TOSTRING`: the
/// native `printStackTrace` header is the virtual `toString()`'s answer, and an
/// exception that override throws leaves `printStackTrace` (HotSpot's
/// `s.println(this)`), which is what `Nio21UncaughtMain tostring` measures.
#[cfg(test)]
mod nio21_w12_print_virtual_to_string_tests;

/// Round 14 wave 1 (lane compat), `CRATONVM_EXC_MESSAGE_PROPAGATE`: an
/// exception thrown by an overriding message getter leaves `toString()` /
/// `getLocalizedMessage()`, as on HotSpot. Both tests fail on the old bodies,
/// which answered the field read instead.
#[cfg(test)]
mod r14_compat_message_propagate_tests;

/// Round 14 wave 2 (lane trace): the `format` bits a captured element carries
/// (`r13w13-trace3-stack-trace-element-origin-residuals` item 1).
#[cfg(test)]
mod r14w2_trace_ste_format_tests;

#[cfg(test)]
mod r14w3_trace_ste_origin_memo_tests;

/// Interpreter round i3 wave 75, lane L2 (`CRATONVM_STACK_TRACE_ORIGIN_RECORD`):
/// the per-VM frame-origin record is seeded from a filled element and copied
/// onto a later one, field for field, and stays empty with the switch off.
#[cfg(test)]
mod i75_l2_frame_origin_record_tests;

/// Round 15 wave 3 (lane trace4;
/// `r15w3-trace3-thread-fields-read-by-name-FIXED-20260930.md`): the `Thread`
/// natives address the fields `java.lang.Thread` declares, not a subclass's
/// field of the same name.
#[cfg(test)]
mod r15w3_trace4_thread_field_tests;

/// Round 15 wave 4 (lane threadvm;
/// `r15w3-trace4-thread-name-tid-by-name-FIXED-20260930.md`, TR4W3-1): `name` /
/// `tid` are read through the fields `java.lang.Thread` declares, and the
/// served `Thread.run` task read goes through `thread_declared_field`.
#[cfg(test)]
mod r15w4_threadvm_thread_field_tests;

/// Round 15 wave 5 (lane threadvm2;
/// `r15w4-threadvm-pool-workers-registered-daemon-FIXED-20260930.md`): the VM's own
/// async pool is told apart from every other `ThreadPoolExecutor`, per VM.
#[cfg(test)]
mod r15w5_threadvm2_async_pool_worker_tests;

#[cfg(test)]
mod gcg_g6oo_stack_trace_door4_tests;

#[cfg(test)]
mod i74_l2_stack_trace_element_of_tests;

/// Interpreter round i3 wave 77, lane L2
/// (`CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC`): the
/// `StackTraceElement.of(StackFrameInfo)` intrinsic fills through the native
/// `initStackTraceElement` is bound to, and derives the `format` through the
/// same per-class answer as the array intrinsic, so the walker's element and
/// the JDK body's cannot drift apart.
#[cfg(test)]
mod i77_l2_stack_trace_element_of_frame_tests;

/// Interpreter round i3 wave 76, lane L2: the fill's name-from-text rule
/// (`CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT`) gives the by-name text, and
/// the `StackTraceElement.toString()` intrinsic
/// (`CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC`) renders the JDK
/// body's text from the stored fields.
#[cfg(test)]
mod i76_l2_stack_trace_text_tests;
