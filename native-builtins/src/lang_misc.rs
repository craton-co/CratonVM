// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Throwable, StackTraceElement, Enum, and Record native method implementations.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
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
    if std::env::var_os("CRATONVM_DBG_CAUSE").is_some() {
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
            if let Ok(watch_cls) = std::env::var("CRATONVM_DBG_WATCH_CAUSE_SELF") {
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
pub(crate) fn capture_throwable_trace(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let hash = ctx.identity_hash_code(this);
    if std::env::var_os("CRATONVM_DBG_STTRACE").is_some() {
        eprintln!("STTRACE_DBG_CTOR_CAP this={:?} hash={hash}", this.as_ptr());
    }
    let trace = ctx.capture_throwable_stack_trace(this);
    let depth = trace.len() as i32;
    // `getOurStackTrace()` only materialises frames when `backtrace != null`;
    // park a self-reference as the non-null marker (the real frame data lives
    // in the identity-hash-keyed trace store).
    ctx.set_field_by_name(this, "backtrace", Value::Object(Some(this)));
    ctx.set_field_by_name(this, "depth", Value::Int(depth));
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
    if let Value::Object(Some(_)) = ctx.get_field_by_name(this, "suppressedExceptions") {
        return;
    }
    let sentinel = ctx.class_id_by_name("java/lang/Throwable").and_then(|cid| {
        ctx.static_field_index_by_name(cid, "SUPPRESSED_SENTINEL")
            .map(|idx| ctx.get_static_field(cid, idx))
    });
    // Only mirror once `Throwable.<clinit>` has populated the sentinel; before
    // that (bootstrap-era throwables) leave the field as-is.
    if let Some(v @ Value::Object(Some(_))) = sentinel {
        ctx.set_field_by_name(this, "suppressedExceptions", v);
    }
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
        capture_throwable_trace(ctx, *this);
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
        capture_throwable_trace(ctx, *this);
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
        capture_throwable_trace(ctx, *this);
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
        capture_throwable_trace(ctx, *this);
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
        capture_throwable_trace(ctx, *this);
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
            let cause = ctx.get_field_by_name(this, "cause");
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
    if std::env::var_os("CRATONVM_DBG_STTRACE").is_some() {
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
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(cid, cls_slashed),
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
            let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
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
                Some(cid) => crate::lang_class::dotted_class_name(cid, &ste.class_name),
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
    let by_name = ctx.get_field_by_name(this, "detailMessage");
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
                if ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref()
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
    let by_name_cause = ctx.get_field_by_name(this, "cause");
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

/// initCause(Throwable) — set the cause field, return this.
pub(crate) fn native_throwable_init_cause(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cause_val = args.get(1).cloned().unwrap_or(Value::Object(None));
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
    let detail = match ctx.get_field_by_name(t, "detailMessage") {
        v @ Value::Object(Some(_)) => v,
        _ if has_named_detail_message => Value::Object(None),
        _ => match ctx.get_field(t, 0) {
            v @ Value::Object(Some(o))
                if ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref()
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
    let by_name = ctx.get_field_by_name(t, "cause");
    if let Value::Object(Some(c)) = by_name {
        if c == t {
            return None;
        }
        if std::env::var_os("CRATONVM_DBG_CAUSE").is_some() {
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
    let Value::Object(Some(array)) = ctx.get_field_by_name(t, "suppressedExceptions") else {
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
    // Get existing suppressed array (or the SUPPRESSED_SENTINEL / null).
    let existing = ctx.get_field_by_name(this, "suppressedExceptions");
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
            ctx.set_field_by_name(this, "suppressedExceptions", Value::Object(Some(new_arr)));
        }
        _ => {
            // No existing array (null, or still the SUPPRESSED_SENTINEL list)
            // — create one with a single element.
            let new_arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(new_arr, 0, Value::Object(Some(suppressed)));
            ctx.set_field_by_name(this, "suppressedExceptions", Value::Object(Some(new_arr)));
        }
    }
    Ok(None)
}

/// getSuppressed() — return Throwable[] from `suppressedExceptions`, or an
/// empty array if none were added. See `native_throwable_add_suppressed`'s
/// doc comment for why this reads by field NAME rather than a hardcoded
/// index.
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
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(this, "suppressedExceptions") {
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
    if let Value::Object(Some(set_arr)) = ctx.get_field_by_name(this, "stackTrace") {
        if ctx.array_length(set_arr) > 0 {
            if std::env::var_os("CRATONVM_DBG_STTRACE").is_some() {
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
    if std::env::var_os("CRATONVM_DBG_STTRACE").is_some() {
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
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
        // Reuse the dotted-name cache shared with `Class.getName()` so
        // repeat frames in the same trace (recursion) hit the cached
        // Arc<str> instead of re-allocating.
        let cls_dotted = match ctx.class_id_by_name(cls_slashed) {
            Some(cid) => crate::lang_class::dotted_class_name(cid, cls_slashed),
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
    if std::env::var_os("CRATONVM_DBG_STTRACE").is_some() {
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
    ctx.set_field_by_name(this, "stackTrace", stack);
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
fn ste_read_field(ctx: &dyn NativeContext, ste: ObjectRef, named: &str, raw_slot: usize) -> Value {
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
pub fn register_throwable_subclass_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
        "java/io/InvalidClassException",
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
        "java/util/regex/PatternSyntaxException",
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

    for cls in throwable_classes.iter() {
        if *cls == "java/lang/reflect/InvocationTargetException" {
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
                native_invocation_target_exception_init_target,
            );
            r.register(
                cls,
                "<init>",
                "(Ljava/lang/Throwable;Ljava/lang/String;)V",
                native_invocation_target_exception_init_target_message,
            );
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
