// SPDX-License-Identifier: Apache-2.0
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
pub(crate) fn w19e_root_value(
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
pub(crate) fn w19e_rooted_value(
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
pub(crate) fn throwable_field_get(
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
pub(crate) fn throwable_field_set(
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
/// `docs/internal/gc-common-round-20260923/applied/handoff-w19e-fill-stack-trace-element-callers.md`
/// (both root the element and re-read it).
///
/// For a CAPTURED frame (every caller but the 4-argument constructor): the
/// element also gets the frame class's module and loader names, as HotSpot's
/// `java_lang_StackTraceElement::fill_in` gives it (round 13 wave 13, lane
/// trace3; [`stack_trace_module_prefix`]).
pub(crate) fn fill_stack_trace_element(
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
pub(crate) fn fill_captured_stack_trace_element(
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
    )
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
/// `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO=0` fills every element afresh.
struct SteOriginMemo<'h> {
    /// The array being built (rooted by the caller).
    array: &'h cratonvm_native_api::NativeHandle,
    /// The index the element being filled is stored at.
    current: usize,
    /// Class -> index of an element already filled for it.
    rows: std::collections::HashMap<ClassId, usize>,
    enabled: bool,
}

impl<'h> SteOriginMemo<'h> {
    fn new(array: &'h cratonvm_native_api::NativeHandle) -> Self {
        Self {
            array,
            current: 0,
            rows: std::collections::HashMap::new(),
            enabled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO",
            ),
        }
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
        for field in STE_ORIGIN_FIELDS {
            let value = scope.get_field_by_name(source, field);
            scope.set_field_by_name(ste, field, value);
        }
        true
    }

    /// The element at the current index was filled for `class_id`.
    fn remember(&mut self, class_id: ClassId) {
        if self.enabled {
            self.rows.entry(class_id).or_insert(self.current);
        }
    }
}

/// `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` -- default ON (round 14 wave 2,
/// lane trace, both modes): see [`fill_captured_stack_trace_element`]. `0`
/// resolves every frame's class by name again. Cached: read per stack frame.
fn stack_trace_element_exact_class() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS")
    })
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
    origin_memo: Option<&mut SteOriginMemo<'_>>,
) -> ObjectRef {
    // `ste` is a young object the caller allocated one statement ago, and every
    // `create_string` below can collect. Holding the caller's address across
    // them and storing through it writes the whole element into a copy nothing
    // reads: `getStackTrace()` then answers an array of blank frames, with no
    // probe firing, because the values stored were live the whole time and only
    // the RECEIVER was stale. Root it — and the strings, which have the same
    // problem in the other direction — and read every address back at the store.
    let mut scope = NativeHandleScope::new(ctx);
    let ste_h = scope.root(ste);
    // HotSpot interns all three (`java_lang_StackTraceElement::fill_in`), so
    // they name the pooled constructor (bigdec page design A, step 1).
    let cls_obj = scope.intern_string(class_dotted);
    let cls_h = scope.root(cls_obj);
    let meth_obj = scope.intern_string(method_name);
    let meth_h = scope.root(meth_obj);
    let file_h = match file_name {
        Some(f) => {
            let s = scope.intern_string(f);
            Some(scope.root(s))
        }
        None => None,
    };

    // Real-JDK layout iff the loaded class carries a named `declaringClass`
    // String field. Synthetic-stub StackTraceElement has no named fields.
    // (This can load the class, so it stays ahead of every address read.)
    let real_layout = scope
        .resolve_field_index("java/lang/StackTraceElement", "declaringClass")
        .is_some();

    let ste = scope.get(&ste_h);
    let cls_str = scope.get(&cls_h);
    let meth_str = scope.get(&meth_h);
    let file_val = match &file_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => Value::Object(None),
    };

    if real_layout {
        scope.set_field_by_name(ste, "declaringClass", Value::Object(Some(cls_str)));
        scope.set_field_by_name(ste, "methodName", Value::Object(Some(meth_str)));
        scope.set_field_by_name(ste, "fileName", file_val);
        scope.set_field_by_name(ste, "lineNumber", Value::Int(line));
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
        let frame_cid = exact_class
            .filter(|&cid| {
                stack_trace_element_exact_class()
                    && scope.class_name_of_id(cid).as_deref() == Some(class_slashed)
            })
            .or_else(|| scope.class_id_by_name(class_slashed));
        let mirror_cid = frame_cid.or_else(|| scope.class_id_by_name("java/lang/Object"));
        if let Some(cid) = mirror_cid {
            // Materialising a mirror allocates, so `ste` has to be read again.
            let mirror = scope.get_class_mirror(cid);
            let ste = scope.get(&ste_h);
            scope.set_field_by_name(ste, "declaringClassObject", Value::Object(Some(mirror)));
        }
        // Round 13 wave 13 (lane trace3): the module and loader fields
        // HotSpot's `fill_in` sets too, so `toString()` prints
        // `java.base/java.lang.Thread.run(Thread.java:1583)`. Only for the
        // frame's own class, never the `java/lang/Object` fallback above.
        if captured_frame && stack_trace_module_prefix() {
            if let Some(cid) = frame_cid {
                let copied = origin_memo
                    .as_deref()
                    .is_some_and(|memo| memo.copy_into(&scope, &ste_h, cid));
                if !copied && fill_stack_trace_element_origin(&mut scope, &ste_h, cid) {
                    if let Some(memo) = origin_memo {
                        memo.remember(cid);
                    }
                }
            }
        }
    } else {
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
/// prefix-less elements and text. Cached: read per stack frame.
fn stack_trace_module_prefix() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACK_TRACE_MODULE_PREFIX")
    })
}

/// A JDK module, whose version `StackTraceElement.computeFormat` drops (the
/// modules hashed in `java.base`; this VM approximates the set by name).
fn is_jdk_module_name(module: &str) -> bool {
    module.starts_with("java.") || module.starts_with("jdk.")
}

/// `(moduleName, moduleVersion)` of a frame of `class_id` as they end up
/// printed: the class's named module (`None` for the unnamed one), and its
/// version only outside the JDK's own modules (HotSpot fills the JDK's too,
/// but `computeFormat` never prints it; see the page).
///
/// By the module's identity: a class of a non-boot `ModuleLayer`'s module
/// (`--jdk-only`) names that module and its descriptor's version, as
/// HotSpot's `fill_in` does from the class's `ModuleEntry`; its
/// `module_name` is `None` (interpreter round i1 wave 45, lane L5).
fn frame_module(ctx: &dyn NativeContext, class_id: ClassId) -> (Option<String>, Option<String>) {
    let module = match ctx.module_identity_of_class(class_id) {
        cratonvm_native_api::ClassModuleIdentity::Layer { name, version, .. } => {
            return (Some(name), version.filter(|v| !v.is_empty()));
        }
        cratonvm_native_api::ClassModuleIdentity::Named(name) => name,
        cratonvm_native_api::ClassModuleIdentity::Unnamed => return (None, None),
    };
    let version = if is_jdk_module_name(&module) {
        None
    } else {
        ctx.module_version(&module).filter(|v| !v.is_empty())
    };
    (Some(module), version)
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
    match ctx.get_field_by_name(loader, "name") {
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
pub(crate) fn stack_frame_prefix(
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
pub(crate) fn printed_frame_prefix(ctx: &mut dyn NativeContext, class_id: ClassId) -> String {
    if !stack_trace_module_prefix() {
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
    // are stored here with the fields; under `--jdk-only` `computeFormat`
    // recomputes the same bits from `declaringClassObject`.
    if stack_trace_element_full_origin()
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
/// round 13's printed-subset fields. Cached: read per stack frame.
fn stack_trace_element_full_origin() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN")
    })
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
/// `java.base`, approximated by name as [`is_jdk_module_name`] does.
fn stack_trace_element_format_bits(loader_id: i32, module: Option<&str>) -> i32 {
    let mut bits = 0;
    if loader_id == 1 || loader_id == 2 {
        bits |= STE_FORMAT_BUILTIN_CLASS_LOADER;
    }
    if module.is_some_and(is_jdk_module_name) {
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
    let (module, version) = match scope.module_identity_of_class(class_id) {
        cratonvm_native_api::ClassModuleIdentity::Layer { name, version, .. } => {
            (Some(name), version.filter(|v| !v.is_empty()))
        }
        _ => {
            let module = scope
                .module_name_of_class(class_id)
                .filter(|m| !m.is_empty());
            let version = module
                .as_deref()
                .and_then(|m| scope.module_version(m))
                .filter(|v| !v.is_empty());
            (module, version)
        }
    };
    let loader_id = scope.loader_id_of_class(class_id);
    let format = stack_trace_element_format_bits(loader_id, module.as_deref());
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
            if let Value::Object(Some(name)) = scope.get_field_by_name(loader, "name") {
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
pub(crate) fn clear_dumped_stack_trace_element_formats(
    ctx: &mut dyn NativeContext,
    elements: ObjectRef,
) {
    if !stack_trace_element_full_origin()
        || ctx
            .resolve_field_index("java/lang/StackTraceElement", "format")
            .is_none()
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_DUMP_THREADS_UNFORMATTED_ELEMENTS",
        )
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
pub(crate) fn write_throwable_detail_message(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    msg: Value,
) {
    write_throwable_field_cached(
        ctx,
        "detailMessage",
        this,
        msg,
    );
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
pub(crate) fn write_throwable_cause(ctx: &mut dyn NativeContext, this: ObjectRef, cause: Value) {
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
pub(crate) fn capture_throwable_trace(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let _ = capture_throwable_trace_ctor(ctx, this);
}

/// [`capture_throwable_trace`] for a constructor shadow that can fail: `Err`
/// is what an application `fillInStackTrace()` override threw, which the
/// shadow must return so `new` throws it (round 12 wave 8, lane compat2).
pub(crate) fn capture_throwable_trace_ctor(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if constructor_defers_to_fill_override(ctx, this)? {
        return Ok(());
    }
    capture_throwable_trace_with_mode(ctx, this, true);
    Ok(())
}

/// `CRATONVM_THROWABLE_CTOR_HONOURS_FILL_OVERRIDE` -- default ON (round 12
/// wave 7, lane compat, both modes). `0` restores the eager capture for every
/// class.
fn ctor_honours_fill_override() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_CTOR_HONOURS_FILL_OVERRIDE",
        )
    })
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
    bridged && fill_override_bridges()
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

/// `CRATONVM_THROWABLE_FILL_OVERRIDE_BRIDGES` -- default ON (round 13 wave 1,
/// lane mhffm, both modes): a covariant `fillInStackTrace()` override seen
/// only through its bridge counts as an override. `0` restores the eager
/// capture for such a class. Read only when the bridge scan found one.
fn fill_override_bridges() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_FILL_OVERRIDE_BRIDGES")
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
    if !ctor_honours_fill_override() {
        return Ok(false);
    }
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_CTOR_FILL_OVERRIDE_EXACT")
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
pub(crate) fn refresh_throwable_trace(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
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
/// `pub(crate)` so the *other* exception-constructor natives in `lib.rs`
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
pub(crate) fn capture_throwable_trace_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    fresh_constructor: bool,
) {
    // The identity hash is DIAGNOSTIC ONLY here -- the retained trace is keyed
    // by address (`SharedVm::store_throwable_stack_trace`). Minting it on every
    // capture was not free: under `--jdk-only` the real
    // `synchronized Throwable.fillInStackTrace()` reaches this body with `this`
    // THIN-LOCKED, and hashing a thin-locked object inflates its monitor -- a
    // `MonitorTable` insert and an `Arc<Monitor>` allocation per throwable.
    // `tools/probes/ThrowCost.java`'s `javaNewOnly` measured it.
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
    write_throwable_field_cached(ctx, "backtrace", this, Value::Object(Some(this)));
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
    // holds a list. This is THE common case under `--jdk-only`, where the real
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
    if let Value::Object(Some(_)) = read_throwable_field(ctx, this, "suppressedExceptions") {
        return;
    }
    let sentinel = throwable_suppressed_sentinel(ctx);
    // Only mirror once `Throwable.<clinit>` has populated the sentinel; before
    // that (bootstrap-era throwables) leave the field as-is.
    if let Some(v @ Value::Object(Some(_))) = sentinel {
        throwable_field_set(ctx, this, "suppressedExceptions", v);
    }
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
pub(crate) fn native_exc_init_message(
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
pub(crate) fn native_exception_in_initializer_init_thrown(
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
/// MEASURED by `apps/probes/ThrowableFamilySweep.java`:
/// `new ClassNotFoundException().initCause(new Error("later"))` is an
/// `IllegalStateException` on HotSpot and was a silent success here.
pub(crate) fn native_exc_init_noargs_null_cause(
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
/// `apps/probes/ThrowableFamilySweep.java`.
pub(crate) fn native_exc_init_message_null_cause(
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
pub(crate) fn native_exc_init_message_cause(
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
pub(crate) fn native_exc_init_cause(
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
        let fill_first = cause_ctor_fill_first();
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

/// `CRATONVM_THROWABLE_CAUSE_CTOR_FILL_FIRST` -- default ON (round 12 wave 8
/// orchestrator, both modes). See [`native_exc_init_cause`].
fn cause_ctor_fill_first() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_CAUSE_CTOR_FILL_FIRST")
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
pub(crate) fn native_assertion_error_init_object(
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
pub(crate) enum ScalarKind {
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
pub(crate) fn native_unchecked_io_init_cause(
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
pub(crate) fn native_unchecked_io_init_message_cause(
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
pub(crate) fn native_parse_exception_init(
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
pub(crate) fn native_missing_resource_init(
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
    if !cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_THROWABLE_SUBCLASS_FIELDS_AFTER_FILL",
    ) {
        for (name, value) in fields {
            ctx.set_field_by_name(*this, name, *value);
        }
        return capture_throwable_trace_ctor(ctx, this);
    }
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
pub(crate) fn native_type_not_present_exception_init(
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
pub(crate) fn native_invocation_target_exception_init_target(
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
pub(crate) fn native_invocation_target_exception_init_target_message(
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
pub(crate) fn native_invocation_target_exception_get_target(
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
pub(crate) fn native_exc_init_noargs(
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

pub(crate) fn native_throwable_fill_in_stack_trace(
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
pub(crate) fn native_init_stack_trace_elements(
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

    // Clone trace data to release the immutable borrow before allocating.
    // Keyed by the object, not its identity hash: see
    // `NativeExceptionAccess::get_throwable_stack_trace`.
    let trace_data: Vec<(
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        Option<std::sync::Arc<str>>,
        i32,
        Option<ClassId>,
    )> = ctx
        .get_throwable_stack_trace(backtrace)
        .map(|t| {
            t.iter()
                .map(|e| {
                    (
                        std::sync::Arc::clone(&e.class_name),
                        std::sync::Arc::clone(&e.method_name),
                        e.source_file.as_ref().map(std::sync::Arc::clone),
                        e.line_number,
                        e.class_id,
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let cap = ctx.array_length(elements);
    if crate::nbflags().dbg_sttrace {
        eprintln!(
            "[STTRACE init] cap={cap} trace_data.len={}",
            trace_data.len()
        );
        for (i, (c, m, _, _, _)) in trace_data.iter().rev().take(cap).enumerate() {
            eprintln!("[STTRACE init]   [{i}] {c}.{m}");
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
    let mut origin_memo = SteOriginMemo::new(&elements_h);
    let mut filled = 0usize;
    for (i, (cls_slashed, meth, file, line, class_id)) in
        trace_data.iter().rev().take(cap).enumerate()
    {
        let ste =
            crate::try_alloc_concurrent_synthetic(&mut *scope, "java/lang/StackTraceElement", 4)?;
        let ste_h = scope.root(ste);
        let cls_dotted = match scope.class_id_by_name(cls_slashed) {
            Some(cid) => {
                crate::lang_class::dotted_class_name(scope.vm_identity(), cid, cls_slashed)
            }
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        origin_memo.at(i);
        fill_captured_stack_trace_element_memo(
            &mut *scope,
            ste,
            *class_id,
            cls_slashed,
            &cls_dotted,
            meth,
            file.as_deref(),
            *line,
            &mut origin_memo,
        );
        let elements = scope.get(&elements_h);
        let ste = scope.get(&ste_h);
        scope.set_array_element(elements, i, Value::Object(Some(ste)));
        filled = i + 1;
    }
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
            let ste = crate::try_alloc_concurrent_synthetic(
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
    Ok(None)
}

pub(crate) fn native_throwable_get_stack_trace_depth(
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
pub(crate) fn native_throwable_get_stack_trace_element(
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
                None => std::sync::Arc::from(crate::lang_class::dotted_binary_name(&ste.class_name)),
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

pub(crate) fn native_throwable_get_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
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

pub(crate) fn native_throwable_get_localized_message(
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
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_EXC_MESSAGE_PROPAGATE");
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
pub(crate) fn native_throwable_get_cause(
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
/// 2026-08-12 on `--real-jdk`: CratonVM's `cause` field already tracks HotSpot's
/// exactly (`isSelf` after `new RuntimeException("m")`, not-self after
/// `(String,Throwable)`, null after `initCause(null)`), so testing the raw field
/// is testing the same thing HotSpot tests.
pub(crate) fn native_throwable_init_cause(
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
pub(crate) fn native_throwable_to_string(
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
    ctx.class_name_of_id(ctx.class_id_of_object(t))
        .unwrap_or_else(|| "java/lang/Throwable".to_string())
        .replace('/', ".")
}

fn throwable_detail_message_text(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<String> {
    let has_named_detail_message = ctx
        .class_name_of_id(ctx.class_id_of_object(t))
        .and_then(|cn| ctx.resolve_field_index(&cn, "detailMessage"))
        .is_some();
    let detail = match throwable_field_get(ctx, t, "detailMessage") {
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
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e));
        }
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
/// MEASURED by `apps/probes/ThrowableFamilySweep.java`. This is the same
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_NATIVE_TRACE_FIELD")
}

/// `CRATONVM_THROWABLE_PRINT_CAUSE_LOOP` -- default ON (round 13 wave 13, lane
/// trace3, both modes; a correctness fix). The native `printStackTrace` walks
/// the `Caused by:` chain in a loop, so a chain of any length prints whole;
/// `0` restores the recursion that stopped after 32 nested levels. Read once
/// per top-level or `Suppressed:` walk: printing is not hot.
fn throwable_print_cause_loop() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_PRINT_CAUSE_LOOP")
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
                    _ => None,
                }
            }
            _ => None,
        };
        // `String.valueOf(null)`: `setStackTrace` refuses null elements, so
        // only a failed `toString` lands here.
        out.push(text.unwrap_or_else(|| "null".to_string()));
    }
    ctx.unpin_native_roots(pin);
    out
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
    let frames: Vec<(String, String, Option<String>, i32, Option<ClassId>)> = ctx
        .get_throwable_stack_trace(t)
        .map(|tr| {
            // stored trace is outermost-first; printStackTrace prints the
            // throw site first, so reverse to innermost-first.
            tr.iter()
                .rev()
                .map(|e| {
                    (
                        e.class_name.replace('/', "."),
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
    let with_prefix = stack_trace_module_prefix();
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
                (_, LINE_NUMBER_NATIVE_METHOD) if jdk_location => {
                    "Native Method".to_string()
                }
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
    matches!(ctx.get_static_field(sys, idx), Value::Object(Some(_)))
}

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
pub(crate) fn native_throwable_print_stack_trace(
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
            print_throwable_chain_to_stream_obj(ctx, this, s);
        }
        _ => print_throwable_chain_to_fd(ctx, this, 2),
    }
    Ok(None)
}

/// Collect the full printStackTrace text (header + captured frames for the
/// throwable and its cause chain) as a list of lines, WITHOUT emitting them.
/// Shared by the fd sink and the stream-object sink so both produce identical
/// text. The header path pins each throwable while it asks the receiver for its
/// virtual localized message, matching `Throwable.toString()` semantics.
fn collect_throwable_chain_lines(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<String> {
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
    ) {
        if depth > 32 {
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
            // `tools/probes/interp/L7/L7W23ErrorShapes.java` `cycle`).
            let already_seen = seen_contains(&*ctx, seen, throwable);
            let (current, header) = throwable_to_string_text(ctx, throwable);
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
                );
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
    let base = ctx.pin_native_root(this);
    append_throwable(ctx, &mut lines, &mut seen, this, "", "", &[], 0);
    ctx.unpin_native_roots(base);
    lines
}

/// Shared body for the fd sink of all `printStackTrace` overloads: writes the
/// header + captured frames for the throwable and its cause chain to `fd`.
fn print_throwable_chain_to_fd(ctx: &mut dyn NativeContext, this: ObjectRef, fd: u32) {
    for line in collect_throwable_chain_lines(ctx, this) {
        emit_stack_line(ctx, line, fd);
    }
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
) {
    // Pin the stream across the allocating create_string / re-entrant println.
    // gen r4w3/rooting: pin it BEFORE collecting the lines — that walk runs
    // `getLocalizedMessage` / `getCause` bytecode and can move `stream`.
    let pin = ctx.pin_native_root(stream);
    let lines = collect_throwable_chain_lines(ctx, this);
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
/// on `--real-jdk`: CratonVM already reproduces both states exactly
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
/// what a native wrote. MEASURED by `apps/probes/ThrowableFamilySweep.java`.
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

pub(crate) fn native_throwable_add_suppressed(
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
pub(crate) fn native_throwable_get_suppressed(
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
    // MEASURED by `apps/probes/ThrowableFamilySweep.java`. The empty case is
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
pub(crate) fn native_throwable_get_stack_trace_array(
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
        return Ok(Some(Value::Object(Some(set_arr))));
    }
    if crate::nbflags().dbg_sttrace {
        eprintln!(
            "STTRACE_DBG_GET_ARRAY this={:?} hash={}",
            this.as_ptr(),
            ctx.identity_hash_code(this)
        );
    }
    // Clone trace data to avoid borrow conflict with ctx. We keep the
    // slashed `class_name` Arc<str> (not the dotted form) so we can pass
    // it back through the cached `dotted_class_name` helper below and
    // share the `Arc<str>` across repeat traces of the same class.
    // Keyed by the object: see `NativeExceptionAccess::get_throwable_stack_trace`.
    let trace_data: Vec<_> = ctx
        .get_throwable_stack_trace(this)
        .map(|t| {
            t.iter()
                .map(|e| {
                    (
                        std::sync::Arc::clone(&e.class_name),
                        std::sync::Arc::clone(&e.method_name),
                        e.source_file.as_ref().map(std::sync::Arc::clone),
                        e.line_number,
                        e.class_id,
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let len = trace_data.len();
    let ste_cid = cached_ste_class_id(ctx);
    // The array outlives one allocation per frame (the element, its three
    // strings, possibly a class mirror), so it cannot be held as an address.
    let mut scope = NativeHandleScope::new(ctx);
    let arr_obj = scope.new_ref_array(ste_cid, len);
    let arr_h = scope.root(arr_obj);
    let mut origin_memo = SteOriginMemo::new(&arr_h);
    // stored trace is outermost-first; getStackTrace() wants index 0 = the
    // throw site (innermost), so fill the array reversed.
    for (i, (cls_slashed, meth, file, line, class_id)) in trace_data.iter().rev().enumerate() {
        let ste =
            crate::try_alloc_concurrent_synthetic(&mut *scope, "java/lang/StackTraceElement", 4)?;
        // The element itself crosses `fill_stack_trace_element`, which
        // allocates three strings and possibly a mirror.
        let ste_h = scope.root(ste);
        // Reuse the dotted-name cache shared with `Class.getName()` so
        // repeat frames in the same trace (recursion) hit the cached
        // Arc<str> instead of re-allocating.
        let cls_dotted = match scope.class_id_by_name(cls_slashed) {
            Some(cid) => {
                crate::lang_class::dotted_class_name(scope.vm_identity(), cid, cls_slashed)
            }
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        origin_memo.at(i);
        fill_captured_stack_trace_element_memo(
            &mut *scope,
            ste,
            *class_id,
            cls_slashed,
            &cls_dotted,
            meth,
            file.as_deref(),
            *line,
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
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_throwable_set_stack_trace(
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
    // MEASURED by `apps/probes/ThrowableFamilySweep.java`.
    let Some(Value::Object(Some(src))) = args.get(1).copied() else {
        // `stackTrace.clone()` on a null argument: the implicit NPE from the
        // `invokevirtual`, carrying HotSpot's helpful message. This is a
        // site-specific LITERAL, not an implementation of helpful NPEs -- both
        // halves of the text are fixed by `setStackTrace`'s own signature (the
        // receiver expression is its parameter `stackTrace`, the method is
        // `[Ljava/lang/StackTraceElement;.clone()`), so there is nothing here
        // for a general mechanism to compute. MEASURED on HotSpot 25.0.4+7 by
        // `apps/probes/ThrowableFamilySweep.java`.
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

pub(crate) fn register_enum_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let e = "java/lang/Enum";
    r.register(e, "<init>", "(Ljava/lang/String;I)V", native_enum_init);
    r.register(e, "ordinal", "()I", native_enum_ordinal);
    r.register(e, "name", "()Ljava/lang/String;", native_enum_name);
    r.register(e, "toString", "()Ljava/lang/String;", native_enum_name); // delegates to name()
    r.register(
        e,
        "compareTo",
        "(Ljava/lang/Enum;)I",
        native_enum_compare_to,
    );
    r.register(e, "equals", "(Ljava/lang/Object;)Z", native_enum_equals);
    r.register(e, "hashCode", "()I", native_enum_hash_code);
    r.register(
        e,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_enum_get_declaring_class,
    );
    r.set_category(__prev_cat);
}

/// Enum.<init>(String name, int ordinal) — store name in field 0, ordinal in field 1
pub(crate) fn native_enum_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
/// E36-1 N4: `pub(crate)` since 2026-09-20, so `phases_late/nio_file.rs` can
/// import these instead of keeping its own copy of the same two numbers. Four
/// copies of this pair existed at one point (`lang_misc.rs`, `stack_walker.rs`,
/// `phases_late/net_channels.rs`, `phases_late/nio_file.rs`) — the shape that
/// let one of them (`nio_file.rs`'s) be inverted for a time without anything
/// noticing. `stack_walker.rs` and `net_channels.rs` no longer keep their own
/// copies; `nio_file.rs`'s is the one this promotion retires.
pub(crate) const ENUM_NAME_SLOT: usize = 0;

/// Slot of `java.lang.Enum`'s own `ordinal` field. See [`ENUM_NAME_SLOT`].
pub(crate) const ENUM_ORDINAL_SLOT: usize = 1;

/// Enum.ordinal() — return field 1 (int)
pub(crate) fn native_enum_ordinal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, ENUM_ORDINAL_SLOT)))
}

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
pub(crate) fn native_enum_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(enum_constant_name(ctx, this)))
}

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
pub(crate) fn enum_constant_name(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
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
pub(crate) fn native_enum_compare_to(
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
    Ok(Some(Value::Int(this_ord - other_ord)))
}

/// Enum.equals(Object) — identity comparison (reference equality)
pub(crate) fn native_enum_equals(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

/// Enum.hashCode() — identity hash code
pub(crate) fn native_enum_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.identity_hash_code(this))))
}

/// Enum.getDeclaringClass() — return Class mirror for this enum's class
pub(crate) fn native_enum_get_declaring_class(
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
pub(crate) fn ste_read_field(
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

pub(crate) fn native_ste_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    );
    Ok(None)
}

pub(crate) fn native_ste_get_class(
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

pub(crate) fn native_ste_get_method(
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

pub(crate) fn native_ste_get_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(ctx, this, "fileName", STE_FIELD_FILE)))
}

pub(crate) fn native_ste_get_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_ste_to_string(
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

pub(crate) fn register_phase53_record(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    r.register(rec, "<init>", "()V", |_ctx, _args| Ok(None));
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
    });
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
    });
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
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.Record expansion — components, equals, hashCode, toString stubs
// =============================================================================

pub(crate) fn register_p60_record(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Record equals/hashCode/toString already registered in earlier phase with proper
    // field-by-field comparison — only add RecordComponent here.

    // RecordComponent = 3-field (name=0, type=1, declaringRecord=2)
    let rc = "java/lang/reflect/RecordComponent";
    r.register(rc, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(rc, "getType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        rc,
        "getDeclaringRecord",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.set_category(__prev_cat);
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
pub(crate) const THROWABLE_FAMILY_CLASSES: &[&str] = &[
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
        (
            "java/lang/TypeNotPresentException",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        ) => return Some(native_type_not_present_exception_init),
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
        // MEASURED by `apps/probes/ThrowableFamilySweep.java`. There is no
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
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
                r.register(cls, "<init>", descriptor, body);
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
                native_throwable_get_message,
            );
            r.register(
                cls,
                "getLocalizedMessage",
                "()Ljava/lang/String;",
                native_throwable_get_message,
            );
            r.register(
                cls,
                "printStackTrace",
                "()V",
                native_throwable_print_stack_trace,
            );
            r.register(
                cls,
                "printStackTrace",
                "(Ljava/io/PrintStream;)V",
                native_throwable_print_stack_trace_to_stream,
            );
            r.register(
                cls,
                "printStackTrace",
                "(Ljava/io/PrintWriter;)V",
                native_throwable_print_stack_trace_to_stream,
            );
            r.register(
                cls,
                "toString",
                "()Ljava/lang/String;",
                native_throwable_to_string,
            );
            r.register(
                cls,
                "getCause",
                "()Ljava/lang/Throwable;",
                native_invocation_target_exception_get_target,
            );
            r.register(
                cls,
                "getTargetException",
                "()Ljava/lang/Throwable;",
                native_invocation_target_exception_get_target,
            );
            r.register(
                cls,
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                native_throwable_init_cause,
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
            native_throwable_get_message,
        );
        // getLocalizedMessage()Ljava/lang/String; — JDK delegates to virtual
        // getMessage by default.
        r.register(
            cls,
            "getLocalizedMessage",
            "()Ljava/lang/String;",
            native_throwable_get_localized_message,
        );
        // printStackTrace()V — no-arg overload, prints to System.err equivalent.
        r.register(
            cls,
            "printStackTrace",
            "()V",
            native_throwable_print_stack_trace,
        );
        // printStackTrace(Ljava/io/PrintStream;)V — JDK 25 overload.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintStream;)V",
            native_throwable_print_stack_trace_to_stream,
        );
        // printStackTrace(Ljava/io/PrintWriter;)V — same shape, same sink.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintWriter;)V",
            native_throwable_print_stack_trace_to_stream,
        );
        // toString()Ljava/lang/String; — "ClassName: message".
        r.register(
            cls,
            "toString",
            "()Ljava/lang/String;",
            native_throwable_to_string,
        );
        // getCause()Ljava/lang/Throwable; — read slot 1.
        r.register(
            cls,
            "getCause",
            "()Ljava/lang/Throwable;",
            native_throwable_get_cause,
        );
        r.register(
            cls,
            "initCause",
            "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
            native_throwable_init_cause,
        );
        r.register(
            cls,
            "addSuppressed",
            "(Ljava/lang/Throwable;)V",
            native_throwable_add_suppressed,
        );
        r.register(
            cls,
            "getSuppressed",
            "()[Ljava/lang/Throwable;",
            native_throwable_get_suppressed,
        );
        r.register(
            cls,
            "getStackTrace",
            "()[Ljava/lang/StackTraceElement;",
            native_throwable_get_stack_trace_array,
        );
        r.register(
            cls,
            "setStackTrace",
            "([Ljava/lang/StackTraceElement;)V",
            native_throwable_set_stack_trace,
        );
    }
    r.set_category(__prev_cat);
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
pub(crate) fn native_throwable_print_stack_trace_to_stream(
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
        None => print_throwable_chain_to_fd(ctx, this, 2),
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
            print_throwable_chain_to_fd(ctx, this, fd);
        }
        // Any other stream — including a WRAPPED System.out/err, and any
        // user sink the fd write cannot reach (e.g. a
        // ByteArrayOutputStream-backed PrintWriter). Drive it through the
        // object's own println so the trace is actually captured — this is
        // the path Spring's AggressiveFactoryBeanInstantiationTests.checkLinkageError
        // and every log-to-string idiom depends on.
        Some(s) => print_throwable_chain_to_stream_obj(ctx, this, s),
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::{NativeClassAccess, NativeContext, NativeHeapAccess};

    /// `Enum.name()` must answer `java.lang.Enum`'s OWN `name`, never a field
    /// of the same name declared by the enum subclass.
    ///
    /// `resolve_field_index_by_class_id` returns the MOST-DERIVED declaration
    /// (§6.2 of `docs/feature-designs/by-name-field-reads.md`), so resolving
    /// `"name"` on the receiver's class picks the subclass's field whenever an
    /// enum declares one. Spring Boot's `WebEndpointTest.Infrastructure` does
    /// — `JERSEY("Jersey")` — and `name()` then answered `"Jersey"` instead of
    /// `"JERSEY"`. `Enum.valueOf` matches on `name()`, so it threw
    /// `IllegalArgumentException: No enum constant JERSEY`, the enum-valued
    /// annotation attribute resolved to nothing, and JUnit rejected the whole
    /// test class with `displayName must not be null or blank` — zero tests
    /// run. The mock resolver maps `test/ShadowedNameEnum`'s `"name"` to slot
    /// 2 so this fails if that branch ever comes back.
    #[test]
    fn enum_name_reads_enums_own_slot_not_a_shadowing_subclass_field() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("test/ShadowedNameEnum").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        let constant = ctx.create_string("JERSEY");
        let shadowing = ctx.create_string("Jersey");
        // Slot 0 is `java.lang.Enum.name`; slot 2 is the subclass's own field,
        // which is what the by-name resolver answers with.
        ctx.set_field(obj, ENUM_NAME_SLOT, Value::Object(Some(constant)));
        ctx.set_field(obj, 2, Value::Object(Some(shadowing)));
        assert_eq!(
            ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "name"),
            Some(2),
            "premise: the by-name resolver prefers the subclass's shadowing field",
        );

        let got = native_enum_name(&mut ctx, &[Value::Object(Some(obj))])
            .expect("native_enum_name must not fail")
            .expect("native_enum_name must return a value");
        let got = match got {
            Value::Object(Some(s)) => ctx.read_string(s),
            other => panic!("expected a String reference, got {other:?}"),
        };
        assert_eq!(
            got.as_deref(),
            Some("JERSEY"),
            "Enum.name() must be the JVM constant name, not the subclass's `name` field",
        );
    }

    /// The soundness property `5bc7458e4` was written for still holds: both
    /// methods are `()Ljava/lang/String;`, so an unwritten slot must surface as
    /// null rather than as the `Value::Int(0)` a raw read produces.
    #[test]
    fn enum_name_degrades_an_unwritten_slot_to_null_not_a_primitive() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("test/UninitialisedEnum").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        assert_eq!(
            ctx.get_field(obj, ENUM_NAME_SLOT),
            Value::Int(0),
            "premise: an unwritten reference slot reads back as a primitive tag",
        );
        assert_eq!(
            native_enum_name(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
            Some(Value::Object(None)),
        );
    }
}

#[cfg(test)]
mod throwable_ctor_table_tests {
    use super::{throwable_ctor_native, THROWABLE_FAMILY_CLASSES};
    use cratonvm_classloading::{throwable_ctor_descriptors, THROWABLE_DEFAULT_CTORS};

    /// The stub's method table and the native registry are built from ONE list,
    /// but they are in different crates and only this gate says so out loud: a
    /// descriptor the stub declares with no native here is a method that
    /// resolves and then has no body, which is worse than the
    /// `NoSuchMethodError` it replaces because it fails later and less clearly.
    #[test]
    fn every_declared_throwable_ctor_has_a_native() {
        for class in THROWABLE_FAMILY_CLASSES {
            for descriptor in throwable_ctor_descriptors(class) {
                assert!(
                    throwable_ctor_native(class, descriptor).is_some(),
                    "{class} declares <init>{descriptor} but no native backs it",
                );
            }
        }
    }

    /// The measured facts this change is built on, kept where a reader can see
    /// them fail. Re-derive with `probes/ThrowableCtorCensusProbe.java` after a
    /// JDK bump — these are JDK 25 numbers, not invariants.
    #[test]
    fn the_measured_census_still_describes_the_table() {
        // `AssertionError(String)` is PRIVATE in the JDK, so registering it
        // fabricates a constructor javac will never emit — and `(Object)V`,
        // which javac emits for BOTH `new AssertionError(msg)` and
        // `assert cond : msg`, is the one that must be there.
        let assertion_error = throwable_ctor_descriptors("java/lang/AssertionError");
        assert!(assertion_error.contains(&"(Ljava/lang/Object;)V"));
        assert!(!assertion_error.contains(&"(Ljava/lang/String;)V"));
        assert!(!assertion_error.contains(&"(Ljava/lang/Throwable;)V"));

        // `UncheckedIOException` is the worst in kind: all four blanket
        // descriptors are dead on it, and neither of its two real constructors
        // was registered.
        let unchecked_io = throwable_ctor_descriptors("java/io/UncheckedIOException");
        assert_eq!(
            unchecked_io,
            [
                "(Ljava/io/IOException;)V",
                "(Ljava/lang/String;Ljava/io/IOException;)V"
            ]
        );
        for dead in THROWABLE_DEFAULT_CTORS {
            assert!(
                !unchecked_io.contains(dead),
                "{dead} is dead on UncheckedIOException"
            );
        }

        // `NoClassDefFoundError` has no cause-taking constructor at all — the
        // one in-tree caller that used `(String,Throwable)V` on it was fixed to
        // message-plus-`initCause`, which is what the JDK's own ClassLoader does.
        assert_eq!(
            throwable_ctor_descriptors("java/lang/NoClassDefFoundError"),
            ["()V", "(Ljava/lang/String;)V"]
        );

        // A class this table has never measured still gets the common four:
        // the caller's `is_throwable_like` test is a NAME heuristic and fires
        // for application classes.
        assert_eq!(
            throwable_ctor_descriptors("com/example/TotallyUnknownException"),
            THROWABLE_DEFAULT_CTORS
        );
    }
}

/// Round 12 wave 8 (lane compat2): an application `fillInStackTrace()`
/// override runs in `Throwable.<init>`'s order and its exception leaves `new`.
/// The shadows are mode-independent code (they serve the throwable
/// constructors wherever the registry routes them -- `--compatible`, and
/// `--jdk-only` for an app class whose `super` is a shadowed JDK constructor
/// the dispatcher keeps native), so these tests have no mode axis.
#[cfg(test)]
mod r12_compat2_fill_override_order_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::error::MethodCallFailed;
    use std::cell::Cell;

    const MESSAGE_SLOT: usize = 1;
    const CAUSE_SLOT: usize = 2;

    thread_local! {
        static SEEN_MESSAGE: Cell<Option<Value>> = const { Cell::new(None) };
        static SEEN_CAUSE: Cell<Option<Value>> = const { Cell::new(None) };
        static OVERRIDE_THROWS: Cell<Option<ObjectRef>> = const { Cell::new(None) };
        static OVERRIDE_SETS_CAUSE: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    }

    fn throwable_fields(declaring: ClassId) -> Vec<FieldMetadata> {
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: declaring,
            is_static: false,
        };
        vec![
            field("detailMessage", "Ljava/lang/String;", MESSAGE_SLOT),
            field("cause", "Ljava/lang/Throwable;", CAUSE_SLOT),
        ]
    }

    /// The override: records what it sees, may call `initCause`, may throw.
    fn fill_override(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if name != "fillInStackTrace" {
            return None;
        }
        SEEN_MESSAGE.with(|c| c.set(Some(ctx.get_field(receiver, MESSAGE_SLOT))));
        SEEN_CAUSE.with(|c| c.set(Some(ctx.get_field(receiver, CAUSE_SLOT))));
        if let Some(cause) = OVERRIDE_SETS_CAUSE.with(Cell::get) {
            ctx.set_field(receiver, CAUSE_SLOT, Value::Object(Some(cause)));
        }
        Some(match OVERRIDE_THROWS.with(Cell::get) {
            Some(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
            None => Ok(Some(Value::Object(Some(receiver)))),
        })
    }

    /// A throwable as a shadow leaves it just before the fill: message and
    /// cause already assigned.
    fn shadowed_throwable(ctx: &mut MockNativeContext) -> ObjectRef {
        SEEN_MESSAGE.with(|c| c.set(None));
        SEEN_CAUSE.with(|c| c.set(None));
        OVERRIDE_THROWS.with(|c| c.set(None));
        OVERRIDE_SETS_CAUSE.with(|c| c.set(None));
        let throwable = ctx
            .ensure_class_initialized("java/lang/Throwable")
            .expect("mock class");
        ctx.set_declared_fields(throwable, throwable_fields(throwable));
        let this = match ctx.new_object("test/QuietOverride") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        let own = ctx.class_id_of_object(this);
        ctx.set_declared_fields(own, throwable_fields(throwable));
        ctx.set_invoke_virtual_hook(fill_override);
        this
    }

    #[test]
    fn the_override_runs_before_the_message_and_cause_are_assigned() {
        let mut ctx = mock_ctx();
        let this = shadowed_throwable(&mut ctx);
        let message = ctx.create_string("msg");
        let cause = match ctx.new_object("java/lang/Error") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
        write_throwable_cause(&mut ctx, this, Value::Object(Some(cause)));

        let mut refreshed = this;
        run_fill_override_in_ctor_order(&mut ctx, &mut refreshed).expect("override returned");

        // `Throwable(String, Throwable)`: fillInStackTrace() sees the field
        // initialisers only -- detailMessage null, cause == this.
        assert_eq!(SEEN_MESSAGE.with(Cell::get), Some(Value::Object(None)));
        assert_eq!(SEEN_CAUSE.with(Cell::get), Some(Value::Object(Some(this))));
        // ... and the constructor's assignments land afterwards.
        assert_eq!(ctx.get_field(refreshed, MESSAGE_SLOT), Value::Object(Some(message)));
        assert_eq!(ctx.get_field(refreshed, CAUSE_SLOT), Value::Object(Some(cause)));
    }

    #[test]
    fn what_the_override_throws_is_returned_for_new_to_throw() {
        let mut ctx = mock_ctx();
        let this = shadowed_throwable(&mut ctx);
        let message = ctx.create_string("msg");
        write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
        write_throwable_cause(&mut ctx, this, Value::Object(Some(this)));
        let thrown = match ctx.new_object("java/lang/IllegalStateException") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        OVERRIDE_THROWS.with(|c| c.set(Some(thrown)));

        let mut refreshed = this;
        let outcome = run_fill_override_in_ctor_order(&mut ctx, &mut refreshed);

        // Wave 7 dropped this (`let _ = invoke_virtual(..)`), so `new` completed.
        assert!(
            matches!(outcome, Err(MethodCallFailed::ExceptionThrown(e)) if e == thrown),
            "the override's exception must leave the constructor",
        );
    }

    #[test]
    fn a_cause_left_at_the_sentinel_keeps_what_the_override_set() {
        // `Throwable(String)` never assigns `cause` after the fill, so an
        // `initCause` inside the override survives on HotSpot.
        let mut ctx = mock_ctx();
        let this = shadowed_throwable(&mut ctx);
        let message = ctx.create_string("msg");
        write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
        write_throwable_cause(&mut ctx, this, Value::Object(Some(this)));
        let set_by_override = match ctx.new_object("java/lang/Error") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        OVERRIDE_SETS_CAUSE.with(|c| c.set(Some(set_by_override)));

        let mut refreshed = this;
        run_fill_override_in_ctor_order(&mut ctx, &mut refreshed).expect("override returned");

        assert_eq!(SEEN_MESSAGE.with(Cell::get), Some(Value::Object(None)));
        assert_eq!(
            ctx.get_field(refreshed, CAUSE_SLOT),
            Value::Object(Some(set_by_override))
        );
        assert_eq!(ctx.get_field(refreshed, MESSAGE_SLOT), Value::Object(Some(message)));
    }
}

#[cfg(test)]
mod r13_mhffm_fill_override_bridge_tests {
    use super::declares_fill_override_bridge;
    use cratonvm_native_api::MethodMetadata;
    use cratonvm_types::ClassId;

    fn method(name: &str, descriptor: &str, access_flags: u16) -> MethodMetadata {
        MethodMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags,
            declaring_class_id: ClassId::new(7),
            exceptions: Vec::new(),
            signature: None,
        }
    }

    #[test]
    fn a_covariant_override_to_a_non_ancestor_is_found_through_its_bridge() {
        // `class U extends RuntimeException { public Error fillInStackTrace() }`:
        // javac emits the real method and an ACC_BRIDGE|ACC_SYNTHETIC twin.
        let methods = vec![
            method("<init>", "()V", 0x0000),
            method("fillInStackTrace", "()Ljava/lang/Error;", 0x0001),
            method("fillInStackTrace", "()Ljava/lang/Throwable;", 0x1041),
        ];
        assert!(declares_fill_override_bridge(&methods));
    }

    #[test]
    fn no_override_and_a_static_lookalike_are_not_overrides() {
        let none = vec![
            method("<init>", "()V", 0x0000),
            method("getMessage", "()Ljava/lang/String;", 0x0001),
        ];
        assert!(!declares_fill_override_bridge(&none));
        let static_one = vec![method("fillInStackTrace", "()Ljava/lang/Throwable;", 0x0009)];
        assert!(!declares_fill_override_bridge(&static_one));
        let other_desc = vec![method("fillInStackTrace", "()Ljava/lang/Error;", 0x0001)];
        assert!(!declares_fill_override_bridge(&other_desc));
    }
}

/// Round 13 wave 12, lane misc12: the native printer's frame text follows a
/// trace `setStackTrace` stored, not the capture store
/// (`r13w12-shadow4-throwable-native-print-ignores-set-stack-trace-FIXED-20260929.md`).
/// The mock models a REAL-layout `java/lang/Throwable` (its six instance
/// fields and the `UNASSIGNED_STACK` static), so the identity rule is the one
/// under test; the mock has no capture store, so "captured frames" is empty.
#[cfg(test)]
mod r13_misc12_set_stack_trace_print_tests {
    use super::{native_throwable_set_stack_trace, throwable_frame_text};
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
    };
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::{ClassId, ObjectRef, Value};

    const BACKTRACE: usize = 0;
    const STACK_TRACE: usize = 3;

    /// `toString()` on a mock element answers the string in its slot 0.
    fn hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        (method == "toString").then(|| Ok(Some(ctx.get_field(receiver, 0))))
    }

    /// A real-layout Throwable class; answers `(class, sentinel)`.
    fn real_throwable(ctx: &mut MockNativeContext) -> (ClassId, ObjectRef) {
        let tid = ctx
            .ensure_class_initialized("java/lang/Throwable")
            .unwrap_or_else(|e| panic!("no Throwable: {e:?}"));
        let field = |name: &str, desc: &str, slot: usize, is_static: bool| FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: if is_static { 0x001A } else { 0x0002 },
            slot_index: slot,
            declaring_class_id: tid,
            is_static,
        };
        ctx.set_declared_fields(
            tid,
            vec![
                field("backtrace", "Ljava/lang/Object;", BACKTRACE, false),
                field("detailMessage", "Ljava/lang/String;", 1, false),
                field("cause", "Ljava/lang/Throwable;", 2, false),
                field("stackTrace", "[Ljava/lang/StackTraceElement;", STACK_TRACE, false),
                field("depth", "I", 4, false),
                field("suppressedExceptions", "Ljava/util/List;", 5, false),
                field("UNASSIGNED_STACK", "[Ljava/lang/StackTraceElement;", 0, true),
            ],
        );
        let sentinel = ctx.new_ref_array(tid, 0);
        ctx.set_static_field(tid, 0, Value::Object(Some(sentinel)));
        (tid, sentinel)
    }

    /// A `StackTraceElement[]` whose elements print as `texts`.
    fn trace(ctx: &mut MockNativeContext, texts: &[&str]) -> ObjectRef {
        // A typed producer (`typed_array_producer_ratchet`).
        let arr = ctx.new_ref_array(ClassId::new(1), texts.len());
        for (i, text) in texts.iter().enumerate() {
            let s = ctx.create_string_uninterned(text);
            let elem = ctx.alloc_object(ClassId::new(1), 1);
            ctx.set_field(elem, 0, Value::Object(Some(s)));
            ctx.set_array_element(arr, i, Value::Object(Some(elem)));
        }
        arr
    }

    #[test]
    fn a_set_trace_prints_its_elements_and_the_switch_restores_the_captured_text() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(hook);
        let (tid, sentinel) = real_throwable(&mut ctx);
        let t = ctx.alloc_object(tid, 6);
        let arr = trace(&mut ctx, &["p.C.m(C.java:7)", "p.D.n(D.java:9)"]);
        ctx.set_field(t, STACK_TRACE, Value::Object(Some(arr)));

        assert_eq!(
            throwable_frame_text(&mut ctx, t),
            vec!["p.C.m(C.java:7)".to_string(), "p.D.n(D.java:9)".to_string()],
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_THROWABLE_NATIVE_TRACE_FIELD", Some("0"))],
            || throwable_frame_text(&mut ctx, t),
        );
        assert!(off.is_empty(), "the kill switch prints the capture store: {off:?}");

        // The sentinel is "not set", by identity.
        ctx.set_field(t, STACK_TRACE, Value::Object(Some(sentinel)));
        assert!(throwable_frame_text(&mut ctx, t).is_empty());
    }

    /// The `writableStackTrace == false` constructor leaves `stackTrace` and
    /// `backtrace` null: `setStackTrace` then stores nothing (HotSpot's
    /// `getStackTrace().length` stays 0). A captured throwable stores.
    #[test]
    fn set_stack_trace_on_an_immutable_stack_is_a_no_op() {
        let mut ctx = mock_ctx();
        let (tid, _sentinel) = real_throwable(&mut ctx);
        let immutable = ctx.alloc_object(tid, 6);
        ctx.set_field(immutable, STACK_TRACE, Value::Object(None));
        ctx.set_field(immutable, BACKTRACE, Value::Object(None));
        let arr = trace(&mut ctx, &["p.C.m(C.java:7)"]);
        let args = [Value::Object(Some(immutable)), Value::Object(Some(arr))];
        assert!(native_throwable_set_stack_trace(&mut ctx, &args).is_ok());
        assert_eq!(ctx.get_field(immutable, STACK_TRACE), Value::Object(None));

        // Kill switch: the copy is stored, as before.
        let stored = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_THROWABLE_NATIVE_TRACE_FIELD", Some("0"))],
            || native_throwable_set_stack_trace(&mut ctx, &args).is_ok(),
        );
        assert!(stored);
        assert!(matches!(ctx.get_field(immutable, STACK_TRACE), Value::Object(Some(_))));

        // `backtrace` set (a capture ran): an ordinary, writable stack.
        let captured = ctx.alloc_object(tid, 6);
        ctx.set_field(captured, STACK_TRACE, Value::Object(None));
        ctx.set_field(captured, BACKTRACE, Value::Object(Some(captured)));
        let args = [Value::Object(Some(captured)), Value::Object(Some(arr))];
        assert!(native_throwable_set_stack_trace(&mut ctx, &args).is_ok());
        assert!(matches!(ctx.get_field(captured, STACK_TRACE), Value::Object(Some(_))));
    }
}

/// Round 13 wave 13 (lane trace3): the native printer's cause loop and the
/// shared frame prefix.
#[cfg(test)]
mod r13_trace3_print_tests {
    use super::{collect_throwable_chain_lines, is_jdk_module_name, stack_frame_prefix};
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
    };
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::{ClassId, ObjectRef, Value};

    const CAUSE: usize = 2;

    /// `getCause()` on a mock throwable answers its `cause` slot.
    fn hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        (method == "getCause").then(|| Ok(Some(ctx.get_field(receiver, CAUSE))))
    }

    /// A real-layout Throwable class.
    fn throwable_class(ctx: &mut MockNativeContext) -> ClassId {
        let tid = ctx
            .ensure_class_initialized("java/lang/Throwable")
            .unwrap_or_else(|e| panic!("no Throwable: {e:?}"));
        let field = |name: &str, desc: &str, slot: usize, is_static: bool| FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: if is_static { 0x001A } else { 0x0002 },
            slot_index: slot,
            declaring_class_id: tid,
            is_static,
        };
        ctx.set_declared_fields(
            tid,
            vec![
                field("backtrace", "Ljava/lang/Object;", 0, false),
                field("detailMessage", "Ljava/lang/String;", 1, false),
                field("cause", "Ljava/lang/Throwable;", CAUSE, false),
                field("stackTrace", "[Ljava/lang/StackTraceElement;", 3, false),
                field("depth", "I", 4, false),
                field("suppressedExceptions", "Ljava/util/List;", 5, false),
                field("UNASSIGNED_STACK", "[Ljava/lang/StackTraceElement;", 0, true),
            ],
        );
        let sentinel = ctx.new_ref_array(tid, 0);
        ctx.set_static_field(tid, 0, Value::Object(Some(sentinel)));
        tid
    }

    /// A throwable wrapping `levels` more below it (the innermost has the
    /// `cause == this` sentinel).
    fn chain(ctx: &mut MockNativeContext, tid: ClassId, levels: usize) -> ObjectRef {
        let mut t = ctx.alloc_object(tid, 6);
        ctx.set_field(t, CAUSE, Value::Object(Some(t)));
        for _ in 0..levels {
            let outer = ctx.alloc_object(tid, 6);
            ctx.set_field(outer, CAUSE, Value::Object(Some(t)));
            t = outer;
        }
        t
    }

    fn caused_by_lines(lines: &[String]) -> usize {
        lines.iter().filter(|l| l.starts_with("Caused by: ")).count()
    }

    /// `R13Misc12TraceText` "deep": HotSpot prints all 40 `Caused by:` lines;
    /// the recursion (the kill switch) stopped at 32.
    #[test]
    fn a_forty_deep_cause_chain_prints_every_level() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(hook);
        let tid = throwable_class(&mut ctx);
        let top = chain(&mut ctx, tid, 40);
        let lines = collect_throwable_chain_lines(&mut ctx, top);
        assert_eq!(caused_by_lines(&lines), 40, "{lines:?}");
        assert_eq!(lines[0], "java.lang.Throwable");
        let old = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_THROWABLE_PRINT_CAUSE_LOOP", Some("0"))],
            || collect_throwable_chain_lines(&mut ctx, top),
        );
        assert_eq!(caused_by_lines(&old), 32);
    }

    /// A cause cycle still ends in HotSpot's `[CIRCULAR REFERENCE: ...]` line
    /// under the loop.
    #[test]
    fn a_cause_cycle_is_named_once() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(hook);
        let tid = throwable_class(&mut ctx);
        let a = ctx.alloc_object(tid, 6);
        let b = ctx.alloc_object(tid, 6);
        ctx.set_field(a, CAUSE, Value::Object(Some(b)));
        ctx.set_field(b, CAUSE, Value::Object(Some(a)));
        let lines = collect_throwable_chain_lines(&mut ctx, a);
        assert_eq!(
            lines,
            vec![
                "java.lang.Throwable".to_string(),
                "Caused by: java.lang.Throwable".to_string(),
                "Caused by: [CIRCULAR REFERENCE: java.lang.Throwable]".to_string(),
            ]
        );
    }

    /// `StackTraceElement.toString()`'s prefix rules.
    #[test]
    fn the_frame_prefix_follows_stack_trace_element_to_string() {
        assert_eq!(stack_frame_prefix(None, None, None), "");
        assert_eq!(stack_frame_prefix(None, Some("java.base"), None), "java.base/");
        assert_eq!(stack_frame_prefix(None, Some("com.foo"), Some("1.0")), "com.foo@1.0/");
        assert_eq!(stack_frame_prefix(Some("mine"), None, None), "mine//");
        assert_eq!(stack_frame_prefix(Some("mine"), Some("m"), Some("2")), "mine/m@2/");
        // A version without a module, and empty names, print nothing.
        assert_eq!(stack_frame_prefix(None, None, Some("1.0")), "");
        assert_eq!(stack_frame_prefix(Some(""), Some(""), None), "");
        assert!(is_jdk_module_name("java.base"));
        assert!(is_jdk_module_name("jdk.proxy1"));
        assert!(!is_jdk_module_name("com.foo"));
    }
}

/// Round 14 wave 1 (lane compat), `CRATONVM_EXC_MESSAGE_PROPAGATE`: an
/// exception thrown by an overriding message getter leaves `toString()` /
/// `getLocalizedMessage()`, as on HotSpot. Both tests fail on the old bodies,
/// which answered the field read instead.
#[cfg(test)]
mod r14_compat_message_propagate_tests {
    use super::{native_throwable_get_localized_message, native_throwable_to_string};
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::NativeContext;
    use cratonvm_types::error::MethodCallFailed;
    use cratonvm_types::{ObjectRef, Value};

    fn dynctx(m: &mut MockNativeContext) -> &mut dyn NativeContext {
        m
    }

    fn new_obj(ctx: &mut dyn NativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object({class}) answered {other:?}"),
        }
    }

    #[test]
    fn to_string_propagates_a_throwing_get_localized_message() {
        let mut m = mock_ctx();
        let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
        let thrown = new_obj(dynctx(&mut m), "java/lang/ArithmeticException");
        m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
        match native_throwable_to_string(dynctx(&mut m), &[Value::Object(Some(exc))]) {
            Err(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
            other => panic!("expected the thrown exception, got {other:?}"),
        }
    }

    #[test]
    fn get_localized_message_propagates_a_throwing_get_message() {
        let mut m = mock_ctx();
        let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
        let thrown = new_obj(dynctx(&mut m), "java/lang/ArithmeticException");
        m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
        match native_throwable_get_localized_message(dynctx(&mut m), &[Value::Object(Some(exc))]) {
            Err(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
            other => panic!("expected the thrown exception, got {other:?}"),
        }
    }

    #[test]
    fn to_string_uses_the_dispatched_text() {
        let mut m = mock_ctx();
        let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
        let text = dynctx(&mut m).create_string("L-m");
        m.set_invoke_virtual_result(Ok(Some(Value::Object(Some(text)))));
        let ctx = dynctx(&mut m);
        match native_throwable_to_string(ctx, &[Value::Object(Some(exc))]) {
            Ok(Some(Value::Object(Some(s)))) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("com.example.MyThrowable: L-m"))
            }
            other => panic!("toString answered {other:?}"),
        }
    }
}

/// Round 14 wave 2 (lane trace): the `format` bits a captured element carries
/// (`r13w13-trace3-stack-trace-element-origin-residuals` item 1).
#[cfg(test)]
mod r14w2_trace_ste_format_tests {
    use super::{
        stack_trace_element_format_bits, STE_FORMAT_BUILTIN_CLASS_LOADER,
        STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE,
    };

    #[test]
    fn format_bits_follow_compute_format() {
        // A JDK frame: bootstrap loader (`null`, not a BuiltinClassLoader).
        assert_eq!(
            stack_trace_element_format_bits(0, Some("java.base")),
            STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
        );
        // A platform-module frame: both bits.
        assert_eq!(
            stack_trace_element_format_bits(1, Some("java.sql")),
            STE_FORMAT_BUILTIN_CLASS_LOADER | STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
        );
        // A class-path frame: the `app/` prefix is dropped, no module.
        assert_eq!(stack_trace_element_format_bits(2, None), STE_FORMAT_BUILTIN_CLASS_LOADER);
        // A named user loader in a named application module: both printed.
        assert_eq!(stack_trace_element_format_bits(3, Some("com.example")), 0);
        assert_eq!(stack_trace_element_format_bits(7, None), 0);
    }
}

#[cfg(test)]
mod r14w3_trace_ste_origin_memo_tests {
    use super::{SteOriginMemo, STE_ORIGIN_FIELDS};
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeContext, NativeHandleScope, NativeHeapAccess,
    };
    use cratonvm_types::{ClassId, Value};

    /// The test mock drops a by-name store to a field its class does not
    /// declare and answers `Int(0)` for the read, so the element class is
    /// declared with the four origin fields at slots 0..4.
    fn declare_ste(ctx: &mut crate::test_utils::MockNativeContext) -> ClassId {
        let ste = ctx
            .ensure_class_initialized("java/lang/StackTraceElement")
            .unwrap_or_else(|e| panic!("no StackTraceElement: {e:?}"));
        let fields = STE_ORIGIN_FIELDS
            .iter()
            .enumerate()
            .map(|(slot, name)| FieldMetadata {
                name: (*name).to_string(),
                descriptor: if *name == "format" { "B" } else { "Ljava/lang/String;" }
                    .to_string(),
                access_flags: 0x0002,
                slot_index: slot,
                declaring_class_id: ste,
                is_static: false,
            })
            .collect();
        ctx.set_declared_fields(ste, fields);
        ste
    }

    #[test]
    fn a_later_frame_of_a_class_copies_the_earlier_elements_origin() {
        let mut ctx = mock_ctx();
        let ste_class = declare_ste(&mut ctx);
        let mut scope = NativeHandleScope::new(&mut ctx);
        let arr = scope.new_ref_array(ste_class, 2);
        let arr_h = scope.root(arr);
        let module = scope.alloc_object(ClassId::new(2), 0);
        let version = scope.alloc_object(ClassId::new(2), 0);
        let loader = scope.alloc_object(ClassId::new(2), 0);
        let first = scope.alloc_object(ste_class, STE_ORIGIN_FIELDS.len());
        // Guard against a vacuous test: the stores below must be readable.
        scope.set_field_by_name(first, "format", Value::Int(3));
        assert_eq!(scope.get_field_by_name(first, "format"), Value::Int(3));
        scope.set_field_by_name(first, "moduleName", Value::Object(Some(module)));
        scope.set_field_by_name(first, "moduleVersion", Value::Object(Some(version)));
        scope.set_field_by_name(first, "classLoaderName", Value::Object(Some(loader)));
        scope.set_field_by_name(first, "format", Value::Int(3));
        scope.set_array_element(arr, 0, Value::Object(Some(first)));
        let frame_class = ClassId::new(42);
        let mut memo = SteOriginMemo::new(&arr_h);
        memo.enabled = true;
        memo.at(0);
        memo.remember(frame_class);
        let second = scope.alloc_object(ste_class, STE_ORIGIN_FIELDS.len());
        let second_h = scope.root(second);
        memo.at(1);
        // Another class: nothing to copy.
        assert!(!memo.copy_into(&scope, &second_h, ClassId::new(43)));
        assert_ne!(scope.get_field_by_name(second, "format"), Value::Int(3));
        assert_ne!(
            scope.get_field_by_name(second, "moduleName"),
            Value::Object(Some(module))
        );
        assert!(memo.copy_into(&scope, &second_h, frame_class));
        for field in STE_ORIGIN_FIELDS {
            assert_eq!(
                scope.get_field_by_name(second, field),
                scope.get_field_by_name(first, field),
                "{field}"
            );
        }
        // The first row for a class stays its source.
        memo.remember(frame_class);
        assert_eq!(memo.rows.get(&frame_class), Some(&0));
    }

    #[test]
    fn a_disabled_memo_records_and_copies_nothing() {
        let mut ctx = mock_ctx();
        let mut scope = NativeHandleScope::new(&mut ctx);
        let arr = scope.new_ref_array(ClassId::new(1), 1);
        let arr_h = scope.root(arr);
        let mut memo = SteOriginMemo::new(&arr_h);
        memo.enabled = false;
        memo.remember(ClassId::new(42));
        assert!(memo.rows.is_empty());
        let ste = scope.alloc_object(ClassId::new(1), 8);
        let ste_h = scope.root(ste);
        assert!(!memo.copy_into(&scope, &ste_h, ClassId::new(42)));
    }
}
