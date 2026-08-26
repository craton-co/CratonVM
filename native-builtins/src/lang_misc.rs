// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Throwable, StackTraceElement, Enum, and Record native method implementations.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cratonvm_native_api::{NativeCallback, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

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
static THROWABLE_SUPPRESSED_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);
static THROWABLE_BACKTRACE_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);
static THROWABLE_DEPTH_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);
static THROWABLE_STACK_TRACE_INDEX: AtomicUsize = AtomicUsize::new(UNRESOLVED_FIELD_INDEX);

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
    let cache = throwable_field_cache(field_name)?;
    let declared = cached_throwable_field_index(ctx, cache, field_name)?;
    let own = ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), field_name)?;
    if own == declared {
        return None;
    }
    (declared < ctx.object_num_fields(this)).then_some(declared)
}

/// Per-name cache cell for [`shadowed_throwable_slot`]. `None` for a name
/// `java/lang/Throwable` does not declare — there is nothing to shadow.
fn throwable_field_cache(field_name: &str) -> Option<&'static AtomicUsize> {
    match field_name {
        "cause" => Some(&THROWABLE_CAUSE_INDEX),
        "detailMessage" => Some(&THROWABLE_DETAIL_MESSAGE_INDEX),
        "suppressedExceptions" => Some(&THROWABLE_SUPPRESSED_INDEX),
        "backtrace" => Some(&THROWABLE_BACKTRACE_INDEX),
        "depth" => Some(&THROWABLE_DEPTH_INDEX),
        "stackTrace" => Some(&THROWABLE_STACK_TRACE_INDEX),
        _ => None,
    }
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
    let cached = STE_CLASS_ID.load(Ordering::Relaxed);
    if cached != UNRESOLVED_CLASS_ID {
        return ClassId::new(cached);
    }
    if let Some(cid) = ctx.class_id_by_name("java/lang/StackTraceElement") {
        STE_CLASS_ID.store(cid.as_u32(), Ordering::Relaxed);
        return cid;
    }
    if let Ok(cid) = ctx.ensure_class_initialized("java/lang/StackTraceElement") {
        STE_CLASS_ID.store(cid.as_u32(), Ordering::Relaxed);
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
pub(crate) fn fill_stack_trace_element(
    ctx: &mut dyn NativeContext,
    ste: ObjectRef,
    class_slashed: &str,
    class_dotted: &str,
    method_name: &str,
    file_name: Option<&str>,
    line: i32,
) {
    let cls_str = ctx.create_string(class_dotted);
    let meth_str = ctx.create_string(method_name);
    let file_val = match file_name {
        Some(f) => Value::Object(Some(ctx.create_string(f))),
        None => Value::Object(None),
    };

    // Real-JDK layout iff the loaded class carries a named `declaringClass`
    // String field. Synthetic-stub StackTraceElement has no named fields.
    let real_layout = ctx
        .resolve_field_index("java/lang/StackTraceElement", "declaringClass")
        .is_some();

    if real_layout {
        ctx.set_field_by_name(ste, "declaringClass", Value::Object(Some(cls_str)));
        ctx.set_field_by_name(ste, "methodName", Value::Object(Some(meth_str)));
        ctx.set_field_by_name(ste, "fileName", file_val);
        ctx.set_field_by_name(ste, "lineNumber", Value::Int(line));
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
        let mirror_cid = ctx
            .class_id_by_name(class_slashed)
            .or_else(|| ctx.class_id_by_name("java/lang/Object"));
        if let Some(cid) = mirror_cid {
            let mirror = ctx.get_class_mirror(cid);
            ctx.set_field_by_name(ste, "declaringClassObject", Value::Object(Some(mirror)));
        }
    } else {
        // Legacy synthetic-stub layout: [class, method, file, line].
        ctx.set_field(ste, 0, Value::Object(Some(cls_str)));
        ctx.set_field(ste, 1, Value::Object(Some(meth_str)));
        ctx.set_field(ste, 2, file_val);
        ctx.set_field(ste, 3, Value::Int(line));
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
pub(crate) fn write_throwable_detail_message(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    msg: Value,
) {
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
    write_throwable_field_cached(ctx, &THROWABLE_CAUSE_INDEX, "cause", this, cause);

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
                    let idx = THROWABLE_CAUSE_INDEX.load(Ordering::Relaxed);
                    if idx != UNRESOLVED_FIELD_INDEX {
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
pub(crate) fn capture_throwable_trace(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = capture_throwable_trace_body(ctx, *this);
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
/// store keyed by identity hash) come back empty. That hid the origin of every
/// exception built via `new SomeException(...)` bytecode.
///
/// We mirror `fillInStackTrace` here: capture the current frames into the
/// VM-owned trace store keyed by the throwable's identity hash, and set
/// the `backtrace`/`depth` fields so the real-JDK `getOurStackTrace()` path
/// also works for callers that hit it directly.
///
/// `pub(crate)` so the *other* exception-constructor natives in `lib.rs`
/// (`native_exception_init_msg` / `native_exception_init_empty`, registered
/// by `register_exception_extras_natives`, and the RKC16N ctor closures in
/// `register_essential_natives`) can route through the same capture logic.
/// Those natives are registered LATER than `register_throwable_subclass_natives`
/// and therefore win the registry slot for ~50 exception subclasses
/// (IllegalStateException, IllegalArgumentException, NumberFormatException, …);
/// without this capture they left `getStackTrace()` empty for every such
/// subclass thrown from bytecode (only the Throwable/Exception/RuntimeException/
/// Error base classes — which those lists omit — kept a working trace).
pub(crate) fn capture_throwable_trace_body(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let hash = ctx.identity_hash_code(this);
    if crate::nbflags().dbg_sttrace {
        // The throwable's CLASS, not just its identity. Counting captures tells
        // you a workload throws a lot; only the class tells you what. Naming it
        // here is what separated "Quartz leaks memory" from "Quartz throws the
        // same exception 25,000 times" — see
        // known-issues/springboot/quartz-endpoint-web-jit-only-spin-loop-20260818.
        eprintln!("STTRACE_DBG_CTOR_CAP this={:?} hash={hash}", this.as_ptr());
    }
    let trace = ctx.capture_throwable_stack_trace(this);
    let depth = trace.len() as i32;
    if crate::nbflags().dbg_sttrace {
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
    // in the identity-hash-keyed trace store).
    throwable_field_set(ctx, this, "backtrace", Value::Object(Some(this)));
    throwable_field_set(ctx, this, "depth", Value::Int(depth));
    // Mirror the JDK field initializer `suppressedExceptions = SUPPRESSED_SENTINEL`.
    init_suppressed_sentinel(ctx, this);
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
    // Don't overwrite an already-initialized list — a populated `ArrayList`
    // from `addSuppressed`, or the sentinel itself (a re-entrant
    // `fillInStackTrace()` also funnels through `capture_throwable_trace`).
    // An unset reference slot reads back as `Int(0)`, not `Object(None)`, so
    // skip ONLY when the field already holds a non-null object reference.
    if let Value::Object(Some(_)) = read_throwable_field(ctx, this, "suppressedExceptions") {
        return;
    }
    let sentinel = ctx.class_id_by_name("java/lang/Throwable").and_then(|cid| {
        ctx.static_field_index_by_name(cid, "SUPPRESSED_SENTINEL")
            .map(|idx| ctx.get_static_field(cid, idx))
    });
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
/// 3. Only `Object(None)` counts. An *unset* reference slot reads back as
///    `Int(0)` (see `init_suppressed_sentinel`), which is again not a verdict.
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
        capture_throwable_trace(ctx, &mut this);
    }
    Ok(None)
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
        capture_throwable_trace(ctx, &mut this);
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
        // detailMessage = (cause == null ? null : cause.toString())
        let detail_msg: Value = match cause {
            Value::Object(Some(cause_ref)) => {
                match ctx.invoke_virtual(*cause_ref, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(v @ Value::Object(Some(_)))) => v,
                    // toString returned null / non-object, or dispatch failed:
                    // leave detailMessage null rather than smuggle a bad value.
                    _ => Value::Object(None),
                }
            }
            _ => Value::Object(None),
        };
        write_throwable_detail_message(ctx, *this, detail_msg);
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace(ctx, &mut this);
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
    capture_throwable_trace(ctx, &mut this);
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
        capture_throwable_trace(ctx, &mut this);
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
        capture_throwable_trace(ctx, &mut this);
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
        if let Some(offset @ Value::Int(_)) = args.get(2) {
            ctx.set_field_by_name(this, "errorOffset", *offset);
        }
        capture_throwable_trace(ctx, &mut this);
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
        if let Some(class_name) = args.get(2) {
            ctx.set_field_by_name(this, "className", *class_name);
        }
        if let Some(key) = args.get(3) {
            ctx.set_field_by_name(this, "key", *key);
        }
        capture_throwable_trace(ctx, &mut this);
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
        ctx.set_field_by_name(*this, "target", target);
        write_throwable_cause(ctx, *this, Value::Object(None));
        write_throwable_detail_message(ctx, *this, Value::Object(None));
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace(ctx, &mut this);
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
        ctx.set_field_by_name(*this, "target", target);
        write_throwable_cause(ctx, *this, Value::Object(None));
        write_throwable_detail_message(ctx, *this, msg);
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        capture_throwable_trace(ctx, &mut this);
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
        capture_throwable_trace(ctx, &mut this);
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

    capture_throwable_trace(ctx, &mut this);

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
    let trace_data: Vec<(
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        Option<std::sync::Arc<str>>,
        i32,
    )> = ctx
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

    let cap = ctx.array_length(elements);
    if crate::nbflags().dbg_sttrace {
        eprintln!(
            "[STTRACE init] cap={cap} trace_data.len={}",
            trace_data.len()
        );
        for (i, (c, m, _, _)) in trace_data.iter().rev().take(cap).enumerate() {
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
    let mut filled = 0usize;
    for (i, (cls_slashed, meth, file, line)) in trace_data.iter().rev().take(cap).enumerate() {
        let ste = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(ctx.vm_identity(), cid, cls_slashed),
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        fill_stack_trace_element(
            ctx,
            ste,
            cls_slashed,
            &cls_dotted,
            meth,
            file.as_deref(),
            *line,
        );
        ctx.set_array_element(elements, i, Value::Object(Some(ste)));
        filled = i + 1;
    }
    // ES-FAIL-06 (root): `StackTraceElement.of(x, depth)` pre-fills the array
    // with empty `new StackTraceElement()` objects (null `declaringClass`).
    // `depth` comes from `getStackTraceDepth()` (keyed on the throwable), but
    // this native looks the trace up by the `backtrace` object's identity hash
    // (arg #1). For most throwables CratonVM stores `backtrace == throwable`
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
            let ste = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
            fill_stack_trace_element(ctx, ste, "(unknown)", "(unknown)", "(unknown)", None, -1);
            ctx.set_array_element(elements, i, Value::Object(Some(ste)));
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
    // Stored trace is outermost-first; `getStackTraceElement(index)` is
    // innermost-first (index 0 = throw site), so map to the reversed index.
    let entry_data = ctx.get_stack_trace(hash).and_then(|t| {
        let len = t.len();
        if index >= 0 && (index as usize) < len {
            t.get(len - 1 - index as usize).cloned()
        } else {
            None
        }
    });

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
        fill_stack_trace_element(
            ctx,
            ste_obj,
            class_slashed,
            class_dotted,
            method_name,
            file_name,
            line,
        );
        ste_obj
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
                None => std::sync::Arc::from(ste.class_name.replace('/', ".")),
            };
            let obj = build_ste(
                ctx,
                &ste.class_name,
                &dotted,
                &ste.method_name,
                ste.source_file.as_deref(),
                ste.line_number,
            );
            Ok(Some(Value::Object(Some(obj))))
        }
        None => {
            let obj = build_ste(ctx, "<unknown>", "<unknown>", "<unknown>", None, -1);
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
                if ctx.class_name_arc_of_id(ctx.class_id_of_object(o)).as_deref()
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
    ctx.unpin_native_roots(pin);

    match result {
        Ok(Some(Value::Object(obj))) => Ok(Some(Value::Object(obj))),
        Ok(Some(_)) | Ok(None) | Err(_) => native_throwable_get_message(ctx, args),
    }
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
        &[
            Value::Object(Some(msg_ref)),
            Value::Object(Some(cause)),
        ],
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
        // An unset reference slot reads back as `Int(0)` — no verdict.
        _ => false,
    };
    if crate::nbflags().dbg_cause {
        let this_cls = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        let raw = throwable_field_get(ctx, this, "cause");
        let idx = ctx.resolve_field_index_by_class_id(class_id, "cause");
        let nf = ctx.object_num_fields(this);
        let frames = ctx.capture_throwable_stack_trace(this);
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

    let (_, result) = throwable_to_string_text(ctx, this);
    let str_obj = ctx.create_string(&result);
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
                if ctx.class_name_arc_of_id(ctx.class_id_of_object(o)).as_deref()
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

/// Read the cause field, returning None if missing or self-referential
/// (the JDK `cause = this` "uninitialized" sentinel).
fn throwable_cause(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<ObjectRef> {
    let by_name = throwable_field_get(ctx, t, "cause");
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

/// Format the captured stack-trace frames for `t`, innermost first, without
/// indentation. Keeping the raw rendered frame text lets the print routine
/// apply HotSpot's common-suffix elision consistently to causes and suppressed
/// exceptions.
fn throwable_frame_text(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<String> {
    let hash = ctx.identity_hash_code(t);
    let frames: Vec<(String, String, Option<String>, i32)> = ctx
        .get_stack_trace(hash)
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
            format!("{cls}.{meth}({loc})")
        })
        .collect()
}

/// Return the real suppressed-throwable elements. The JDK sentinel is a List,
/// while CratonVM's native `addSuppressed` replaces it with a Throwable array;
/// only the latter represents user-visible suppressed exceptions.
fn throwable_suppressed(ctx: &mut dyn NativeContext, t: ObjectRef) -> Vec<ObjectRef> {
    let Value::Object(Some(array)) = throwable_field_get(ctx, t, "suppressedExceptions") else {
        return Vec::new();
    };
    if ctx.heap_kind_of(array) != cratonvm_types::ObjectKind::Array {
        return Vec::new();
    }
    (0..ctx.array_length(array))
        .filter_map(|index| match ctx.get_array_element(array, index) {
            Value::Object(Some(throwable)) => Some(throwable),
            _ => None,
        })
        .collect()
}

/// Emit a single line: record it for in-VM consumers and write it to the
/// host process's stderr fd (2) so `printStackTrace` actually shows up
/// when the JVM runs an embedded program. The record_printed_line call
/// keeps existing tests that scan `thread.printed_lines` working.
fn emit_stack_line(ctx: &mut dyn NativeContext, line: String, fd: u32) {
    ctx.record_printed_line(line.clone());
    let sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "\r\n".to_string()
            } else {
                "\n".to_string()
            }
        });
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
        ctx.static_field_index_by_name(sys, "err")
            .and_then(|idx| match ctx.get_static_field(sys, idx) {
                Value::Object(Some(s)) => Some(s),
                _ => None,
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
    fn append_throwable(
        ctx: &mut dyn NativeContext,
        lines: &mut Vec<String>,
        seen: &mut Vec<ObjectRef>,
        throwable: ObjectRef,
        caption: &str,
        prefix: &str,
        enclosing_frames: &[String],
        depth: usize,
    ) {
        if depth > 32
            || seen
                .iter()
                .any(|seen_throwable| *seen_throwable == throwable)
        {
            return;
        }
        let (throwable, header) = throwable_to_string_text(ctx, throwable);
        if seen
            .iter()
            .any(|seen_throwable| *seen_throwable == throwable)
        {
            return;
        }
        seen.push(throwable);
        let frames = throwable_frame_text(ctx, throwable);
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

        for suppressed in throwable_suppressed(ctx, throwable) {
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
        if let Some(cause) = throwable_cause(ctx, throwable) {
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
        }
    }

    let mut lines = Vec::new();
    let mut seen = Vec::new();
    append_throwable(ctx, &mut lines, &mut seen, this, "", "", &[], 0);
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
    let lines = collect_throwable_chain_lines(ctx, this);
    // Pin the stream across the allocating create_string / re-entrant println.
    let pin = ctx.pin_native_root(stream);
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
/// `mirror_throwable_field_initialisers` in `vm/src/runtime/exceptions.rs`.
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
    // Get existing suppressed array (or the SUPPRESSED_SENTINEL / null).
    let existing = read_throwable_field(ctx, this, "suppressedExceptions");
    match existing {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == cratonvm_types::ObjectKind::Array => {
            // Grow the array: copy old elements + append new one
            let old_len = ctx.array_length(arr);
            let new_arr = ctx.new_ref_array(ClassId::new(0), old_len + 1);
            for i in 0..old_len {
                let elem = ctx.get_array_element(arr, i);
                ctx.set_array_element(new_arr, i, elem);
            }
            ctx.set_array_element(new_arr, old_len, Value::Object(Some(suppressed)));
            write_throwable_field(
                ctx,
                this,
                "suppressedExceptions",
                Value::Object(Some(new_arr)),
            );
        }
        _ => {
            // No existing array (null, or still the SUPPRESSED_SENTINEL list)
            // — create one with a single element.
            let new_arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(new_arr, 0, Value::Object(Some(suppressed)));
            write_throwable_field(
                ctx,
                this,
                "suppressedExceptions",
                Value::Object(Some(new_arr)),
            );
        }
    }
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
    if let Value::Object(Some(arr)) = read_throwable_field(ctx, this, "suppressedExceptions") {
        if ctx.heap_kind_of(arr) == cratonvm_types::ObjectKind::Array {
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // No suppressed exceptions stored (null, or still SUPPRESSED_SENTINEL) —
    // return an empty array.
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
    // ES-FAIL-06 (proper fix): honor an explicitly-set `stackTrace` array.
    // `RandomizedRunner.augmentStackTrace()` prepends a synthetic
    // `__randomizedtesting.SeedInfo.seed(...)` frame via `setStackTrace()`, and
    // the JDK stores the (cloned) array in the `stackTrace` field. Previously
    // this native ALWAYS re-materialised from the captured backtrace, dropping
    // that augmentation and yielding length/content mismatches (the masked
    // ES-suite failures). The JDK sentinel `UNASSIGNED_STACK` is a zero-length
    // array, so a non-empty `stackTrace` field means it was set (or cached) and
    // must be returned verbatim instead of re-deriving from the backtrace.
    if let Value::Object(Some(set_arr)) = throwable_field_get(ctx, this, "stackTrace") {
        if ctx.array_length(set_arr) > 0 {
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
    }
    let hash = ctx.identity_hash_code(this);
    if crate::nbflags().dbg_sttrace {
        eprintln!("STTRACE_DBG_GET_ARRAY this={:?} hash={hash}", this.as_ptr());
    }
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
    let ste_cid = cached_ste_class_id(ctx);
    let arr = ctx.new_ref_array(ste_cid, len);
    // stored trace is outermost-first; getStackTrace() wants index 0 = the
    // throw site (innermost), so fill the array reversed.
    for (i, (cls_slashed, meth, file, line)) in trace_data.iter().rev().enumerate() {
        let ste = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
        // Reuse the dotted-name cache shared with `Class.getName()` so
        // repeat frames in the same trace (recursion) hit the cached
        // Arc<str> instead of re-allocating.
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(ctx.vm_identity(), cid, cls_slashed),
            None => std::sync::Arc::from(cls_slashed.replace('/', ".")),
        };
        fill_stack_trace_element(
            ctx,
            ste,
            cls_slashed,
            &cls_dotted,
            meth,
            file.as_deref(),
            *line,
        );
        ctx.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    if crate::nbflags().dbg_sttrace {
        let n = ctx.array_length(arr);
        for i in 0..n {
            if let Value::Object(Some(e)) = ctx.get_array_element(arr, i) {
                let cn = ctx.get_field_by_name(e, "declaringClass");
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
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let stack = args.get(1).copied().unwrap_or(Value::Object(None));
    throwable_field_set(ctx, this, "stackTrace", stack);
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
const ENUM_NAME_SLOT: usize = 0;

/// Slot of `java.lang.Enum`'s own `ordinal` field. See [`ENUM_NAME_SLOT`].
const ENUM_ORDINAL_SLOT: usize = 1;

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
pub(crate) fn ste_read_field(ctx: &dyn NativeContext, ste: ObjectRef, named: &str, raw_slot: usize) -> Value {
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
    fill_stack_trace_element(
        ctx,
        this,
        &class_slashed,
        &class_dotted,
        &method,
        file.as_deref(),
        line,
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
            let nf = ctx.object_num_fields(this);
            for i in 0..nf {
                let a = ctx.get_field(this, i);
                let b = ctx.get_field(*other, i);
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
                                    )? {
                                        Some(Value::Int(v)) => v != 0,
                                        _ => false,
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
                Value::Object(Some(r)) => match ctx.invoke_virtual(r, "hashCode", "()I", &[])? {
                    Some(Value::Int(hc)) => hc,
                    _ => ctx.identity_hash_code(r),
                },
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
/// secondary NPE inside a catch handler.
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
            assert!(!unchecked_io.contains(dead), "{dead} is dead on UncheckedIOException");
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
