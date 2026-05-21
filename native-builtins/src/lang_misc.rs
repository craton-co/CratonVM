//! Throwable, StackTraceElement, Enum, and Record native method implementations.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ClassId, ObjectRef, Value};
use cratonvm_types::error::MethodCallResult;

use crate::obj_arg;

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
// All caches use sentinel values rather than `OnceLock<Option<…>>` so that
// a failed lookup on early boot (Throwable class not yet loaded) does not
// poison the cache permanently — the next call will retry.
// ---------------------------------------------------------------------------

/// Sentinel meaning "field index not yet resolved." `resolve_field_index`
/// returns `usize`, so any real index will be far below this. We use
/// `AtomicUsize` because the index is read on every Throwable ctor call
/// and stored at most once per process lifetime — relaxed ordering is fine
/// (write races are idempotent and the bounds-check on every use guards
/// against stale snapshots).
const UNRESOLVED_FIELD_INDEX: usize = usize::MAX;
static THROWABLE_DETAIL_MESSAGE_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);
static THROWABLE_CAUSE_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);

/// Sentinel meaning "STE class id not yet resolved." `ClassId::new(0)` is
/// the legacy fallback (java/lang/Object) — using it as a sentinel would
/// be ambiguous with the audit-flagged bug it replaces, so we use
/// `u32::MAX` instead and treat any non-MAX value as cached.
const UNRESOLVED_CLASS_ID: u32 = u32::MAX;
static STE_CLASS_ID: AtomicU32 = AtomicU32::new(UNRESOLVED_CLASS_ID);

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
fn cached_throwable_field_index(
    ctx: &dyn NativeContext,
    cache: &AtomicUsize,
    field_name: &str,
) -> Option<usize> {
    let cached = cache.load(Ordering::Relaxed);
    if cached != UNRESOLVED_FIELD_INDEX {
        return Some(cached);
    }
    let idx = ctx.resolve_field_index("java/lang/Throwable", field_name)?;
    // Race-safe: any concurrent resolver will compute the same index since
    // the class layout is immutable once loaded. Last-write-wins is fine.
    cache.store(idx, Ordering::Relaxed);
    Some(idx)
}

/// Write a Throwable field, preferring the cached slot index and falling
/// back to name-based lookup if the cached index is stale (object has
/// fewer slots than expected — synthetic-stub layout).
#[inline]
fn write_throwable_field_cached(
    ctx: &mut dyn NativeContext,
    cache: &AtomicUsize,
    field_name: &str,
    this: ObjectRef,
    value: Value,
) {
    if let Some(idx) = cached_throwable_field_index(ctx, cache, field_name) {
        if idx < ctx.object_num_fields(this) {
            ctx.set_field(this, idx, value);
            return;
        }
    }
    ctx.set_field_by_name(this, field_name, value);
}

/// Resolve & cache the `java/lang/StackTraceElement` class id. Falls back
/// to `ClassId::new(0)` only if the class still isn't loaded — that's the
/// pre-fix behaviour, preserved here so we don't regress on early-boot
/// callers that pre-date class loading. Once the class loads, every
/// subsequent build_ste call gets the correct id.
#[inline]
fn cached_ste_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    let cached = STE_CLASS_ID.load(Ordering::Relaxed);
    if cached != UNRESOLVED_CLASS_ID {
        return ClassId::new(cached);
    }
    if let Some(cid) = ctx.class_id_by_name("java/lang/StackTraceElement") {
        STE_CLASS_ID.store(cid.as_u32(), Ordering::Relaxed);
        return cid;
    }
    // Class not loaded yet — return the legacy fallback but DO NOT cache,
    // so a later call (after the class loads) can re-resolve correctly.
    ClassId::new(0)
}

/// Populate a freshly-allocated `java/lang/StackTraceElement` instance.
///
/// Real-JDK 25 `StackTraceElement` instance layout is NOT
/// `[declaringClass, methodName, fileName, lineNumber]`. The actual field
/// order is:
///   0 declaringClassObject (Class)  1 classLoaderName (String)
///   2 moduleName (String)           3 moduleVersion (String)
///   4 declaringClass (String)       5 methodName (String)
///   6 fileName (String)             7 lineNumber (int)
///   8 format (byte)
///
/// The previous native helpers wrote class/method/file/line into slots
/// 0..=3, which clobbered `declaringClassObject` with a `String` and left
/// the real `declaringClass`/`methodName`/`fileName`/`lineNumber` unset.
/// `StackTraceElement.computeFormat()` then loaded `declaringClassObject`
/// (a `String`) and dispatched `Class.getClassLoader0()` on it, surfacing
/// as `NoSuchMethodError: java/lang/String.getClassLoader0` during
/// Keycloak 26 boot.
///
/// We resolve the slots by field name (correct for the real loaded class)
/// and fall back to the legacy 0..=3 layout only when the class isn't
/// loaded yet (early-boot synthetic-stub path).
///
/// `decl_class_internal` is the `/`-separated internal name of the frame's
/// declaring class. When the real STE class is loaded we resolve its
/// `Class` mirror and store it into `declaringClassObject` — real-JDK 25
/// `StackTraceElement.of(backtrace, depth)` calls `computeFormat()` right
/// after the native populates the element, and `computeFormat` does
/// `declaringClassObject.getClassLoader0()`. If the field is left null
/// (the previous behaviour) `computeFormat` NPEs with
/// "Cannot invoke getClassLoader0 on null".
pub(crate) fn write_ste_fields(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    class_name: Value,
    method_name: Value,
    file_name: Value,
    line: Value,
    decl_class_internal: Option<&str>,
) {
    let by_name = ctx
        .resolve_field_index("java/lang/StackTraceElement", "declaringClass")
        .is_some();
    if by_name {
        ctx.set_field_by_name(ste, "declaringClass", class_name);
        ctx.set_field_by_name(ste, "methodName", method_name);
        ctx.set_field_by_name(ste, "fileName", file_name);
        ctx.set_field_by_name(ste, "lineNumber", line);
        // `declaringClassObject` must be a real `Class` so `computeFormat()`
        // can call `getClassLoader0()`/`getModule()` on it. Resolve the
        // mirror from the frame's class name when it is loaded.
        let mirror = decl_class_internal
            .and_then(|n| ctx.class_id_by_name(n))
            .map(|cid| Value::Object(Some(ctx.get_class_mirror(cid))));
        if let Some(m) = mirror {
            ctx.set_field_by_name(ste, "declaringClassObject", m);
        }
    } else {
        // Synthetic-stub layout (class not loaded): legacy 4-slot order.
        ctx.set_field(ste, 0, class_name);
        ctx.set_field(ste, 1, method_name);
        ctx.set_field(ste, 2, file_name);
        ctx.set_field(ste, 3, line);
    }
}

/// Helper: write Throwable.detailMessage on a Throwable subclass.
///
/// Real-JDK Throwable layout: slot 0 = `backtrace` (an internal Object
/// reference), slot 1 = `detailMessage`, slot 2 = `cause`. We resolve the
/// slot index once per process (cached in `THROWABLE_DETAIL_MESSAGE_INDEX`)
/// and reuse it; falls back to name-based resolution if the cached index
/// is out of bounds for `this` (synthetic-stub layout). The previous
/// implementation also mirrored to slot 0 to support a now-removed
/// synthetic-stub layout — that mirror clobbered Throwable.backtrace with
/// a String reference and corrupted any downstream consumer that read
/// backtrace as an Object[].
fn write_throwable_detail_message(ctx: &mut dyn NativeContext, this: ObjectRef, msg: Value) {
    write_throwable_field_cached(
        ctx,
        &THROWABLE_DETAIL_MESSAGE_INDEX,
        "detailMessage",
        this,
        msg,
    );
}

/// Helper: write Throwable.cause on a Throwable subclass.
///
/// Real-JDK Throwable layout: `cause` is at slot 2. We resolve the slot
/// index once per process (cached in `THROWABLE_CAUSE_INDEX`) and reuse
/// it; falls back to name-based resolution on bounds mismatch. The
/// previous implementation also mirrored to slot 1 (the synthetic-stub
/// cause slot), but slot 1 in the real-JDK layout is `detailMessage` —
/// the mirror clobbered the message field whenever both helpers ran
/// (e.g. via `<init>(String, Throwable)`).
fn write_throwable_cause(ctx: &mut dyn NativeContext, this: ObjectRef, cause: Value) {
    write_throwable_field_cached(ctx, &THROWABLE_CAUSE_INDEX, "cause", this, cause);
}

/// Capture the current call stack for a freshly-constructed throwable.
///
/// These `native_exc_init_*` natives SHADOW the JDK `Throwable.<init>`
/// bytecode (registered per-class by `register_throwable_subclass_natives`).
/// The JDK constructor is the only thing that calls `fillInStackTrace()` —
/// so when our native replaces it, nothing records the stack trace, and a
/// later `printStackTrace()` / `getStackTrace()` (which read the trace store
/// keyed by identity hash) come back empty. That hid the origin of every
/// exception built via `new SomeException(...)` bytecode.
///
/// We mirror `fillInStackTrace` here: capture the current frames into the
/// thread-local trace store keyed by the throwable's identity hash, and set
/// the `backtrace`/`depth` fields so the real-JDK `getOurStackTrace()` path
/// also works for callers that hit it directly.
fn capture_throwable_trace(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let hash = ctx.identity_hash_code(this);
    let trace = ctx.capture_stack_trace(hash);
    let depth = trace.len() as i32;
    // `getOurStackTrace()` only materialises frames when `backtrace != null`;
    // park a self-reference as the non-null marker (the real frame data lives
    // in the identity-hash-keyed trace store).
    ctx.set_field_by_name(this, "backtrace", Value::Object(Some(this)));
    ctx.set_field_by_name(this, "depth", Value::Int(depth));
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
pub(crate) fn native_exc_init_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(msg) = args.get(1) {
            write_throwable_detail_message(ctx, *this, *msg);
        }
        // Initialize cause to self-sentinel so a later initCause() succeeds.
        write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
        capture_throwable_trace(ctx, *this);
    }
    Ok(None)
}

/// Exception <init>(Ljava/lang/String;Ljava/lang/Throwable;)V
pub(crate) fn native_exc_init_message_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(msg) = args.get(1) {
            write_throwable_detail_message(ctx, *this, *msg);
        }
        if let Some(cause) = args.get(2) {
            write_throwable_cause(ctx, *this, *cause);
        }
        capture_throwable_trace(ctx, *this);
    }
    Ok(None)
}

/// Exception <init>(Ljava/lang/Throwable;)V — sets cause.
pub(crate) fn native_exc_init_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let (Some(Value::Object(Some(this))), Some(cause)) = (args.first(), args.get(1)) {
        write_throwable_cause(ctx, *this, *cause);
        capture_throwable_trace(ctx, *this);
    }
    Ok(None)
}

/// Exception <init>()V — no message, no cause. Mirrors the JDK
/// `cause = this` sentinel so later `initCause()` calls succeed.
pub(crate) fn native_exc_init_noargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
        capture_throwable_trace(ctx, *this);
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
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("fillInStackTrace on null".to_string()),
            }
            .into());
        }
    };

    capture_throwable_trace(ctx, this);

    // Return `this` (Throwable.fillInStackTrace returns the Throwable itself)
    Ok(Some(Value::Object(Some(this))))
}

/// `StackTraceElement.initStackTraceElements([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V`
///
/// Real-JDK `Throwable.getOurStackTrace()` allocates a `StackTraceElement[]`
/// of length `depth` and hands it, the opaque `backtrace` object, and `depth`
/// to this native to populate. Our `backtrace` marker is the throwable
/// itself, so we look up its captured trace (thread-local store keyed by
/// identity hash) and fill each STE. Previously registered as a no-op, so
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

    let hash = ctx.identity_hash_code(backtrace);
    // Clone trace data to release the immutable borrow before allocating.
    let trace_data: Vec<(std::sync::Arc<str>, std::sync::Arc<str>, Option<std::sync::Arc<str>>, i32)> =
        ctx.get_stack_trace(hash)
            .map(|t| {
                t.iter()
                    .map(|e| {
                        (
                            std::sync::Arc::clone(&e.class_name),
                            std::sync::Arc::clone(&e.method_name),
                            e.source_file.as_ref().map(std::sync::Arc::clone),
                            e.line_number,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

    let cap = ctx.array_length(elements);
    for (i, (cls_slashed, meth, file, line)) in trace_data.iter().take(cap).enumerate() {
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 9);
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(cid, cls_slashed),
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        let cls_str = ctx.create_string(&cls_dotted);
        let meth_str = ctx.create_string(meth);
        let file_val = match file {
            Some(f) => Value::Object(Some(ctx.create_string(f))),
            None => Value::Object(None),
        };
        write_ste_fields(
            ctx,
            ste,
            Value::Object(Some(cls_str)),
            Value::Object(Some(meth_str)),
            file_val,
            Value::Int(*line),
            Some(cls_slashed),
        );
        ctx.set_array_element(elements, i, Value::Object(Some(ste)));
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

    let hash = ctx.identity_hash_code(this);
    let depth = ctx
        .get_stack_trace(hash)
        .map_or(0, |trace| trace.len() as i32);
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

    let hash = ctx.identity_hash_code(this);

    // Clone the trace entry data to release the immutable borrow on ctx
    // before we need mutable access for object allocation.
    let entry_data = ctx
        .get_stack_trace(hash)
        .and_then(|t| t.get(index as usize))
        .cloned();

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
                     class_name: &str,
                     method_name: &str,
                     file_name: Option<&str>,
                     line: i32,
                     decl_internal: Option<&str>|
     -> ObjectRef {
        let ste_cid = cached_ste_class_id(ctx);
        // Size for the real-JDK 9-slot layout; alloc with the larger of
        // the real field count and 9 so name-based field writes land.
        let real = ctx.class_num_total_fields(ste_cid);
        let ste_obj = ctx.alloc_object(ste_cid, real.max(9));
        let cs = ctx.create_string(class_name);
        let ms = ctx.create_string(method_name);
        let file_val = match file_name {
            Some(f) => Value::Object(Some(ctx.create_string(f))),
            None => Value::Object(None),
        };
        write_ste_fields(
            ctx,
            ste_obj,
            Value::Object(Some(cs)),
            Value::Object(Some(ms)),
            file_val,
            Value::Int(line),
            decl_internal,
        );
        ste_obj
    };

    match entry_data {
        Some(ste) => {
            // Use the shared `dotted_class_name` cache so repeat traces
            // that hit the same class (e.g. recursive frames) reuse the
            // existing `Arc<str>` instead of re-running `replace('/', ".")`.
            let dotted = match ctx.class_id_by_name(&ste.class_name) {
                Some(cid) => crate::lang_class::dotted_class_name(cid, &ste.class_name),
                None => std::sync::Arc::from(ste.class_name.replace('/', ".")),
            };
            let obj = build_ste(
                ctx,
                &dotted,
                &ste.method_name,
                ste.source_file.as_deref(),
                ste.line_number,
                Some(&ste.class_name),
            );
            Ok(Some(Value::Object(Some(obj))))
        }
        None => {
            let obj = build_ste(ctx, "<unknown>", "<unknown>", None, -1, None);
            Ok(Some(Value::Object(Some(obj))))
        }
    }
}

pub(crate) fn native_throwable_get_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK Throwable layout has `detailMessage` at slot 1 (after
    // `backtrace` at slot 0); synthetic-stub layout puts it at slot 0
    // (unnamed `_f0`). Prefer the field-name lookup so the real-JDK
    // bytecode (which writes via `putfield detailMessage`) and our
    // native init helpers (which now mirror to both) agree.
    let by_name = ctx.get_field_by_name(this, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        // Fallback: synthetic-stub layout where the field has no `detailMessage`
        // name and the canonical slot is index 0.
        _ => ctx.get_field(this, 0),
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

// --- Throwable additional methods ---

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
pub(crate) fn native_throwable_get_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let by_name_cause = ctx.get_field_by_name(this, "cause");
    if let Value::Object(Some(cause_obj)) = by_name_cause {
        // JDK sentinel: cause==this means "uninitialized cause"; report null.
        if cause_obj == this {
            return Ok(Some(Value::Object(None)));
        }
        return Ok(Some(by_name_cause));
    }
    // ITE / other wrappers: getCause() returns the dedicated `target`
    // field, not the inherited Throwable.cause. Mirror that here so the
    // wrapper exception propagates the correct cause to JLS-spec callers.
    let by_name_target = ctx.get_field_by_name(this, "target");
    if let Value::Object(Some(target_obj)) = by_name_target {
        // Same self-reference guard, in case any wrapper's bytecode
        // initializes its `target` field with `this` as a sentinel.
        if target_obj == this {
            return Ok(Some(Value::Object(None)));
        }
        return Ok(Some(by_name_target));
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

/// initCause(Throwable) — set the cause field, return this.
pub(crate) fn native_throwable_init_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cause_val = args.get(1).cloned().unwrap_or(Value::Object(None));
    write_throwable_cause(ctx, this, cause_val);
    Ok(Some(Value::Object(Some(this))))
}

/// toString() — build "ClassName: message" or just "ClassName"
pub(crate) fn native_throwable_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Get the class name
    let class_id = ctx.class_id_of_object(this);
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "java/lang/Throwable".to_string())
        .replace('/', ".");

    // Read detailMessage by name first (real-JDK Throwable layout has it
    // at slot 1), with slot-0 fallback for synthetic stubs.
    let by_name = ctx.get_field_by_name(this, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        _ => ctx.get_field(this, 0),
    };
    let result = match detail {
        Value::Object(Some(str_ref)) => {
            if let Some(msg) = ctx.read_string(str_ref) {
                format!("{class_name}: {msg}")
            } else {
                class_name
            }
        }
        _ => class_name,
    };

    let str_obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// Build "ClassName: message" or just "ClassName" for a throwable, reading
/// the real-JDK `detailMessage` field by name with a slot-0 fallback for
/// synthetic stubs.
fn throwable_header_line(ctx: &mut dyn NativeContext, t: ObjectRef) -> String {
    let class_id = ctx.class_id_of_object(t);
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "java/lang/Throwable".to_string())
        .replace('/', ".");
    let by_name = ctx.get_field_by_name(t, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        _ => ctx.get_field(t, 0),
    };
    match detail {
        Value::Object(Some(sr)) => match ctx.read_string(sr) {
            Some(m) => format!("{class_name}: {m}"),
            None => class_name,
        },
        _ => class_name,
    }
}

/// Read the cause field, returning None if missing or self-referential
/// (the JDK `cause = this` "uninitialized" sentinel).
fn throwable_cause(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<ObjectRef> {
    let by_name = ctx.get_field_by_name(t, "cause");
    if let Value::Object(Some(c)) = by_name {
        if c == t { return None; }
        return Some(c);
    }
    None
}

/// Format the captured stack-trace frames for `t` as "\tat C.m(F:L)" lines.
fn throwable_frame_lines(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<String> {
    let hash = ctx.identity_hash_code(t);
    let frames: Vec<(String, String, Option<String>, i32)> = ctx
        .get_stack_trace(hash)
        .map(|tr| {
            tr.iter()
                .map(|e| {
                    (
                        e.class_name.replace('/', "."),
                        e.method_name.to_string(),
                        e.source_file.as_ref().map(|f| f.to_string()),
                        e.line_number,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    frames
        .into_iter()
        .map(|(cls, meth, file, line)| {
            let loc = match (file.as_deref(), line) {
                (Some(f), n) if n > 0 => format!("{f}:{n}"),
                (Some(f), _) => f.to_string(),
                (None, n) if n > 0 => format!("Unknown Source:{n}"),
                _ => "Unknown Source".to_string(),
            };
            format!("\tat {cls}.{meth}({loc})")
        })
        .collect()
}

/// Emit a single line: record it for in-VM consumers and write it to the
/// host process's stderr fd (2) so `printStackTrace` actually shows up
/// when the JVM runs an embedded program. The record_printed_line call
/// keeps existing tests that scan `thread.printed_lines` working.
fn emit_stack_line(ctx: &mut dyn NativeContext, line: String) {
    ctx.record_printed_line(line.clone());
    let sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n".to_string() } else { "\n".to_string() });
    let _ = ctx.fd_table().write_string(2, &line);
    let _ = ctx.fd_table().write_string(2, &sep);
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

    // Header for the top-level throwable.
    let header = throwable_header_line(ctx, this);
    emit_stack_line(ctx, header);
    for f in throwable_frame_lines(ctx, this) {
        emit_stack_line(ctx, f);
    }

    // Walk the cause chain with a cycle guard. Limit depth defensively
    // to avoid pathological loops if `cause` was somehow self-referential
    // through a non-equality identity.
    let mut seen: Vec<ObjectRef> = vec![this];
    let mut current = throwable_cause(ctx, this);
    let mut depth = 0;
    while let Some(c) = current {
        depth += 1;
        if depth > 32 || seen.iter().any(|s| *s == c) {
            break;
        }
        seen.push(c);
        let inner = throwable_header_line(ctx, c);
        emit_stack_line(ctx, format!("Caused by: {inner}"));
        for f in throwable_frame_lines(ctx, c) {
            emit_stack_line(ctx, f);
        }
        current = throwable_cause(ctx, c);
    }

    Ok(None)
}

/// addSuppressed(Throwable) — append to suppressed list stored in field 2
pub(crate) fn native_throwable_add_suppressed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let suppressed = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Don't allow self-suppression
    if this == suppressed {
        return Ok(None);
    }
    // Check if the object has enough fields for suppressed storage (field 2)
    let num_fields = ctx.object_num_fields(this);
    if num_fields < 3 {
        return Ok(None); // object too small, silently ignore
    }
    // Get existing suppressed array from field 2
    let existing = ctx.get_field(this, 2);
    match existing {
        Value::Object(Some(arr)) => {
            // Grow the array: copy old elements + append new one
            let old_len = ctx.array_length(arr);
            let new_arr = ctx.new_ref_array(ClassId::new(0), old_len + 1);
            for i in 0..old_len {
                let elem = ctx.get_array_element(arr, i);
                ctx.set_array_element(new_arr, i, elem);
            }
            ctx.set_array_element(new_arr, old_len, Value::Object(Some(suppressed)));
            ctx.set_field(this, 2, Value::Object(Some(new_arr)));
        }
        _ => {
            // No existing array — create one with single element
            let new_arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(new_arr, 0, Value::Object(Some(suppressed)));
            ctx.set_field(this, 2, Value::Object(Some(new_arr)));
        }
    }
    Ok(None)
}

/// getSuppressed() — return Throwable[] from field 2, or empty array if not set
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
    let num_fields = ctx.object_num_fields(this);
    if num_fields >= 3 {
        if let Value::Object(Some(arr)) = ctx.get_field(this, 2) {
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // No suppressed exceptions stored — return empty array
    let arr = ctx.new_ref_array(ClassId::new(0), 0);
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
    let hash = ctx.identity_hash_code(this);
    // Clone trace data to avoid borrow conflict with ctx. We keep the
    // slashed `class_name` Arc<str> (not the dotted form) so we can pass
    // it back through the cached `dotted_class_name` helper below and
    // share the `Arc<str>` across repeat traces of the same class.
    let trace_data: Vec<_> = ctx
        .get_stack_trace(hash)
        .map(|t| {
            t.iter()
                .map(|e| {
                    (
                        std::sync::Arc::clone(&e.class_name),
                        std::sync::Arc::clone(&e.method_name),
                        e.source_file.as_ref().map(std::sync::Arc::clone),
                        e.line_number,
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let len = trace_data.len();
    let arr = ctx.new_ref_array(ClassId::new(0), len);
    for (i, (cls_slashed, meth, file, line)) in trace_data.iter().enumerate() {
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 9);
        // Reuse the dotted-name cache shared with `Class.getName()` so
        // repeat frames in the same trace (recursion) hit the cached
        // Arc<str> instead of re-allocating.
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(cid, cls_slashed),
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        let cls_str = ctx.create_string(&cls_dotted);
        let meth_str = ctx.create_string(meth);
        let file_val = match file {
            Some(f) => Value::Object(Some(ctx.create_string(f))),
            None => Value::Object(None),
        };
        write_ste_fields(
            ctx,
            ste,
            Value::Object(Some(cls_str)),
            Value::Object(Some(meth_str)),
            file_val,
            Value::Int(*line),
            Some(cls_slashed),
        );
        ctx.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ===========================================================================
// java.lang.Enum
// ===========================================================================

pub(crate) fn register_enum_natives(r: &mut NativeMethodRegistry) {
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

/// Enum.ordinal() — return field 1 (int)
pub(crate) fn native_enum_ordinal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, 1)))
}

/// Enum.name() and Enum.toString() — return field 0 (String ref)
pub(crate) fn native_enum_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

/// Enum.compareTo(Enum other) — this.ordinal - other.ordinal
pub(crate) fn native_enum_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_enum_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
// Legacy 4-slot synthetic layout (used only when the real
// `java/lang/StackTraceElement` class isn't loaded yet). When the real
// class is loaded, fields are resolved by name — see `ste_read_field` /
// `ste_write_field` — because the real-JDK 25 instance layout puts
// `declaringClass`/`methodName`/`fileName`/`lineNumber` at slots 4..=7,
// not 0..=3.
const STE_FIELD_CLASS: usize = 0;
const STE_FIELD_METHOD: usize = 1;
const STE_FIELD_FILE: usize = 2;
const STE_FIELD_LINE: usize = 3;

/// Read an STE field by JDK name, falling back to the legacy slot index
/// when the real class isn't loaded (synthetic-stub layout).
fn ste_read_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    legacy_slot: usize,
) -> Value {
    if ctx
        .resolve_field_index("java/lang/StackTraceElement", name)
        .is_some()
    {
        ctx.get_field_by_name(this, name)
    } else {
        ctx.get_field(this, legacy_slot)
    }
}

/// Write an STE field by JDK name, falling back to the legacy slot index
/// when the real class isn't loaded (synthetic-stub layout).
fn ste_write_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    legacy_slot: usize,
    value: Value,
) {
    if ctx
        .resolve_field_index("java/lang/StackTraceElement", name)
        .is_some()
    {
        ctx.set_field_by_name(this, name, value);
    } else {
        ctx.set_field(this, legacy_slot, value);
    }
}

pub(crate) fn native_ste_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let class_val = args.get(1).copied().unwrap_or(Value::Object(None));
    ste_write_field(ctx, this, "declaringClass", STE_FIELD_CLASS, class_val);
    ste_write_field(
        ctx,
        this,
        "methodName",
        STE_FIELD_METHOD,
        args.get(2).copied().unwrap_or(Value::Object(None)),
    );
    ste_write_field(
        ctx,
        this,
        "fileName",
        STE_FIELD_FILE,
        args.get(3).copied().unwrap_or(Value::Object(None)),
    );
    ste_write_field(
        ctx,
        this,
        "lineNumber",
        STE_FIELD_LINE,
        args.get(4).copied().unwrap_or(Value::Int(-1)),
    );
    // Populate `declaringClassObject` so `computeFormat()` (called lazily
    // by `toString()` on the real class) does not NPE on a null Class.
    // Derive the internal name from the dotted class-name string arg.
    if ctx
        .resolve_field_index("java/lang/StackTraceElement", "declaringClassObject")
        .is_some()
    {
        if let Value::Object(Some(s)) = class_val {
            if let Some(dotted) = ctx.read_string(s) {
                let internal = dotted.replace('.', "/");
                if let Some(cid) = ctx.class_id_by_name(&internal) {
                    let mirror = ctx.get_class_mirror(cid);
                    ctx.set_field_by_name(
                        this,
                        "declaringClassObject",
                        Value::Object(Some(mirror)),
                    );
                }
            }
        }
    }
    Ok(None)
}

pub(crate) fn native_ste_get_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(ctx, this, "declaringClass", STE_FIELD_CLASS)))
}

pub(crate) fn native_ste_get_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ste_read_field(ctx, this, "methodName", STE_FIELD_METHOD)))
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
    Ok(Some(ste_read_field(ctx, this, "lineNumber", STE_FIELD_LINE)))
}

pub(crate) fn native_ste_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    r.register(rec, "<init>", "()V", |_ctx, _args| {
        Ok(None)
    });
    r.register(rec, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            // Records: must be same class, not just same field count
            let this_cid = ctx.class_id_of_object(this);
            let other_cid = ctx.class_id_of_object(*other);
            if this_cid != other_cid {
                return Ok(Some(Value::Int(0)));
            }
            let nf = ctx.object_num_fields(this);
            for i in 0..nf {
                if ctx.get_field(this, i) != ctx.get_field(*other, i) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(rec, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let nf = ctx.object_num_fields(this);
        let mut hash: i32 = 0;
        for i in 0..nf {
            let v = ctx.get_field(this, i);
            let h = match v {
                Value::Int(n) => n,
                Value::Long(n) => (n ^ (n >> 32)) as i32,
                Value::Float(f) => f.to_bits() as i32,
                Value::Double(d) => {
                    let bits = d.to_bits();
                    (bits ^ (bits >> 32)) as i32
                }
                Value::Object(Some(r)) => r.as_ptr() as i32,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(h);
        }
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
            let s = ctx.create_string(&format!("{}@{:x}", simple, this.as_ptr() as usize));
            return Ok(Some(Value::Object(Some(s))));
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
}

// =============================================================================
// java.lang.Record expansion — components, equals, hashCode, toString stubs
// =============================================================================

pub(crate) fn register_p60_record(r: &mut NativeMethodRegistry) {
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
pub fn register_throwable_subclass_natives(r: &mut NativeMethodRegistry) {
    // Subset that surfaces in jboss-modules / WildFly catch-blocks (per
    // bench/wildfly-boot/diagnostic.md §WP8.10.7) plus the broader
    // Error/Exception families that any defensive catch will see.
    let throwable_classes = [
        "java/lang/Throwable",
        "java/lang/Exception",
        "java/lang/RuntimeException",
        "java/lang/Error",
        "java/lang/LinkageError",
        "java/lang/NoClassDefFoundError",
        "java/lang/ClassNotFoundException",
        "java/lang/NoSuchMethodError",
        "java/lang/NoSuchFieldError",
        "java/lang/NoSuchMethodException",
        "java/lang/NoSuchFieldException",
        "java/lang/NullPointerException",
        "java/lang/ArithmeticException",
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/ClassCastException",
        "java/lang/IllegalArgumentException",
        "java/lang/IllegalStateException",
        "java/lang/UnsupportedOperationException",
        "java/lang/StackOverflowError",
        "java/lang/OutOfMemoryError",
        "java/util/NoSuchElementException",
        "java/util/InputMismatchException",
        "java/io/IOException",
        "java/io/FileNotFoundException",
        "java/lang/NumberFormatException",
        "java/util/ConcurrentModificationException",
        "java/lang/NegativeArraySizeException",
        "java/lang/AssertionError",
        "java/lang/MatchException",
        "java/lang/IncompatibleClassChangeError",
        "java/lang/ExceptionInInitializerError",
        "java/lang/VerifyError",
        "java/lang/AbstractMethodError",
        "java/lang/InternalError",
        "java/lang/UnsatisfiedLinkError",
    ];

    for cls in throwable_classes.iter() {
        // Constructor overloads for synthetic Throwable-family stubs.
        // Some bootstrap paths instantiate subclasses directly (for example
        // InternalError and NoSuchMethodError wrappers). Register all common
        // ctor descriptors here so both synthetic and real-JDK flows can
        // initialize message/cause consistently.
        // audit-2026-05-16: previously this registered the generic
        // `native_noop_with_this`, which left `cause` un-initialised; the
        // (String) ctor uses the JDK sentinel `cause = this`, so a later
        // `initCause()` succeeded for (String) ctors but failed for noargs.
        r.register(cls, "<init>", "()V", native_exc_init_noargs);
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/String;)V",
            native_exc_init_message,
        );
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
            native_exc_init_message_cause,
        );
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/Throwable;)V",
            native_exc_init_cause,
        );

        // getMessage()Ljava/lang/String; — read slot 0 (detailMessage).
        r.register(
            cls,
            "getMessage",
            "()Ljava/lang/String;",
            native_throwable_get_message,
        );
        // getLocalizedMessage()Ljava/lang/String; — JDK delegates to
        // getMessage by default.
        r.register(
            cls,
            "getLocalizedMessage",
            "()Ljava/lang/String;",
            native_throwable_get_message,
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
    }
}

/// printStackTrace(Ljava/io/PrintStream;)V (and PrintWriter overload).
///
/// Args layout: [this, stream]. We reuse the no-arg printStackTrace
/// implementation which writes to the recorded-line sink; the
/// PrintStream/PrintWriter argument is intentionally ignored because
/// the recorded-line sink is already wired through to System.err in
/// the lib.rs PrintStream natives.
///
/// Null-safe: if `this` is null we no-op; if `stream` is null we still
/// print, because catch-block code paths frequently call
/// `t.printStackTrace(System.err)` and the synthetic-stub System.err
/// static may itself be null on early boot — we don't want to throw a
/// secondary NPE inside a catch handler.
pub(crate) fn native_throwable_print_stack_trace_to_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_throwable_print_stack_trace(ctx, args)
}

