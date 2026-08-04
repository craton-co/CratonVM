// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19_K3_PROPS_SIDETABLE — robust `java.util.Properties` storage.
//!
//! KC26 / KeycloakMain.<clinit> reads `org.keycloak.common.Version.VERSION`,
//! which is set by `Version.<clinit>` via:
//!
//! ```text
//! is = Version.class.getResourceAsStream("/keycloak-version.properties");
//! Properties p = new Properties();
//! p.load(is);                                // <-- real JDK bytecode
//! VERSION = p.getProperty("version");        // <-- expected non-null
//! ```
//!
//! The resource lookup succeeds (700 bytes served by our
//! `Class.getResourceAsStream` native), and the real JDK bytecode for
//! `Properties.load` calls `this.put(k, v)` for each line.  But the
//! synthetic Properties (allocated by `register_synthetic_overrides`
//! / `alloc_concurrent_synthetic`) has an incomplete inner Hashtable
//! field-shape, so `put` writes to a slot that `get` cannot retrieve.
//! Result: `Properties.getProperty("version")` returns null and
//! `Version.<clinit>` NPEs on `VERSION.toLowerCase()`.
//!
//! The fix: a side-table keyed by the Properties object's pointer
//! identity, with native overrides for the public API surface used by
//! `Properties.load`/`getProperty`/`setProperty`.  The side-table is
//! a `Mutex<FxHashMap<usize, FxHashMap<String, String>>>`; both layers
//! are bounded by `MAX_PROPS_PER_OBJECT` and `MAX_TOTAL_OBJECTS` to
//! prevent unbounded memory growth from misbehaving callers.
//!
//! Security posture:
//!   * Per-object size cap (10_000 keys) — a single Properties object
//!     cannot be coerced into unbounded growth via repeated `put`
//!     calls.
//!   * Total-object cap (10_000 distinct Properties objects) — the
//!     side-table itself cannot grow without bound across many
//!     short-lived Properties.
//!   * Key/value length cap (64 KiB) — defends against malformed
//!     `.properties` files with multi-megabyte continuation lines.
//!   * `Properties.load(InputStream)` validates the input shape and
//!     refuses inputs larger than 16 MiB before parsing.

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHasher};
use std::hash::BuildHasherDefault;
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

/// Per-object property cap.  10_000 keys * 64 KiB max value = 640 MiB
/// per object, but in practice `.properties` files are tiny.  This
/// prevents pathological inputs from coercing growth to unbounded.
const MAX_PROPS_PER_OBJECT: usize = 10_000;

/// Total tracked Properties object cap.  Prevents accidental memory
/// leaks from short-lived Properties accumulating in the side-table.
const MAX_TOTAL_OBJECTS: usize = 262_144;
// Raised from 10_000 (2026-07-28). A real application boot blows through that
// long before it stops creating `Properties`: the Keycloak 26.6.1 / Quarkus
// boot had 24_016 tracked objects by the time Infinispan loaded
// `META-INF/infinispan-version.properties`, so EVERY later `load` /
// `setProperty` on a not-yet-tracked object was refused -- silently. The cap
// is a runaway-growth backstop, not a working-set limit, and the entries it
// bounds are small; `drain_reclaimed` returns slots as objects die.

/// Max bytes accepted by `Properties.load(InputStream)`.  16 MiB is
/// far above any real `.properties` file.
const MAX_LOAD_BYTES: usize = 16 * 1024 * 1024;

/// Max key/value length (64 KiB).  Real `.properties` keys are <128
/// chars; values rarely exceed 4 KiB.  64 KiB caps continuation-line
/// abuse without rejecting realistic inputs.
const MAX_KV_LEN: usize = 64 * 1024;

const MALFORMED_UNICODE_MESSAGE: &str = "Malformed \\uxxxx encoding.";

#[inline]
fn props_stderr_diag() -> bool {
    crate::nbflags().diag_properties
}

fn throw_malformed_unicode_escape(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    let msg = ctx.create_string(MALFORMED_UNICODE_MESSAGE);
    match ctx.new_object_initialized(
        "java/lang/IllegalArgumentException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalArgumentException {
            message: MALFORMED_UNICODE_MESSAGE.to_string(),
        }
        .into(),
    }
}

macro_rules! props_diag_eprintln {
    ($($t:tt)*) => {
        if props_stderr_diag() {
            eprintln!($($t)*);
        }
    };
}

/// Per-object property store. Insertion-ordered (`IndexMap`, not `FxHashMap`)
/// because every enumeration native — `keySet` / `stringPropertyNames` /
/// `entrySet` / `keys` / `elements` / `propertyNames` / `forEach` / `store` —
/// derives its iteration order from it, and a hash-bucket order is both
/// arbitrary and unlike anything the real JDK produces. Insertion order is the
/// deterministic floor for a `Properties` with no real `map`
/// `ConcurrentHashMap` backing (the synthetic `System.getProperties()`
/// singleton, surefire's `store_property_in_sidetable` path);
/// [`ordered_snapshot_kv`] layers the JDK's actual order on top whenever there
/// IS such a backing.
type PropsMap = indexmap::IndexMap<String, String, BuildHasherDefault<FxHasher>>;

fn table() -> &'static Mutex<FxHashMap<usize, PropsMap>> {
    static T: OnceLock<Mutex<FxHashMap<usize, PropsMap>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

// --- GC-stable side-table key -------------------------------------------------
//
// The side-table was keyed by `obj.as_ptr()`, the raw heap address. Under a
// moving GC that address is NOT a stable object identity: after a relocation a
// fresh `Properties` can reuse the address a different `Properties` had, so two
// distinct objects collide on the same side-table entry. The visible symptom is
// catastrophic: `new Properties().setProperty(...)` bleeds into
// `System.getProperties()` (they alias the same entry), so Hibernate's
// `ConfigurationHelper.maskOut` — which clones the global props and masks the
// CLONE's `hibernate.connection.password` to "****" — leaks "****" back into the
// real password, and every EMF/SessionFactory bootstrap connects with the wrong
// password ("Wrong user name or password"). That single defect disabled/failed
// ~500 Hibernate ORM test classes.
//
// Fix: key by a GC-stable identity. `ctx.identity_hash_code(obj)` is stable
// across relocations; a small per-hash generation registry (mirroring
// `native-collections`' `widened_obj_key`) disambiguates genuine 32-bit hash
// collisions among live objects.
struct ObjKeyEntry {
    last_ptr: usize,
    generation: u32,
}

fn obj_key_registry() -> &'static Mutex<FxHashMap<u32, Vec<ObjKeyEntry>>> {
    static R: OnceLock<Mutex<FxHashMap<u32, Vec<ObjKeyEntry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(FxHashMap::default()))
}

#[inline]
fn pack_obj_key(hash: u32, generation: u32) -> usize {
    ((hash as usize) << 32) | (generation as usize)
}

fn key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = obj_key_registry().lock();
    let slots = reg.entry(hash).or_default();
    // 1. Same object seen again at the same address.
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return pack_obj_key(hash, slot.generation);
    }
    // 2. Lone occupant whose address changed (moving GC relocated it): rebind.
    //    Only safe when `hash` is a real assigned identity (non-zero): with a
    //    real unique hash, a lone bucket occupant whose address changed must be
    //    the same object after a GC move. When the hash is 0 (identity not yet
    //    assigned — some synthetic/slow-path allocations leave the header hash
    //    unset until `System.identityHashCode` is first called), DISTINCT
    //    objects all hash to bucket 0, so rebinding would merge them — exactly
    //    the cross-contamination this fix exists to prevent. Fall through to a
    //    fresh per-address generation instead.
    if hash != 0 && slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return pack_obj_key(hash, slots[0].generation);
    }
    // 3. New object for this hash (or a genuine 32-bit collision): fresh slot.
    let generation = slots.len() as u32;
    slots.push(ObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    pack_obj_key(hash, generation)
}

// --- GC-aware reclaim for the total-object cap ---------------------------------
//
// `MAX_TOTAL_OBJECTS` used to be a hard, never-decremented ceiling: once
// 10_000 distinct `Properties` objects had EVER been registered in this
// process's lifetime, every subsequent brand-new object silently lost all
// `put`/`getProperty` calls forever (no exception, no eviction). See
// fixed-suite-bugs/h2-suite-bugs/bug-h2-properties-sidetable-global-cap-silent-drop-FIXED.md.
// H2's `TestAnalyzeTableTx` (10_000 connections in a loop, each constructing
// a JDBC-properties object) crosses that watermark and starts reading back
// empty username/password, which H2 correctly reports as "Wrong user name or
// password".
//
// Fix: track each registered object's liveness with a real
// `java.lang.ref.WeakReference` enqueued on a process-wide
// `ReferenceQueue`, so a `Properties` object that has become unreachable can
// be reclaimed and its slot reused. This makes `MAX_TOTAL_OBJECTS` mean what
// its doc comment always claimed it meant — a cap on *concurrently alive*
// tracked objects — instead of "objects ever constructed".
//
// The `WeakReference` itself is kept alive via `add_global_root` (a real GC
// root) for exactly as long as its target might still be reclaimed; the
// *referent* (the `Properties` object) is only weakly reachable through it,
// so it can still be collected normally. When the referent dies, the GC
// clears and enqueues the `WeakReference`; polling the queue tells us which
// side-table slot to free.
struct WeakTrackEntry {
    /// Packed `key_for` identity of the tracked `Properties` object —
    /// the slot in `table()` (and `system_props_keys()`) to free on reclaim.
    props_key: usize,
    /// `add_global_root` handle for the `WeakReference` object itself.
    global_root: usize,
}

/// `key_for(weak_ref_obj) -> WeakTrackEntry`. Keyed the same GC-stable way as
/// `table()` (see `key_for`) since the `WeakReference` object is itself a
/// live, moving-GC-relocatable object between registration and the moment it
/// gets polled off the queue.
fn weak_track_registry() -> &'static Mutex<FxHashMap<usize, WeakTrackEntry>> {
    static R: OnceLock<Mutex<FxHashMap<usize, WeakTrackEntry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Global root handle for the single process-wide `ReferenceQueue` used to
/// detect reclaimed `Properties` objects. `None` until the first object is
/// ever registered (or permanently, if this `NativeContext` doesn't support
/// global roots / real GC integration — e.g. test mocks — in which case
/// reclaim is simply never attempted and behavior falls back to the old
/// strict-cap semantics).
fn weak_track_queue_root() -> &'static Mutex<Option<usize>> {
    static Q: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(None))
}

/// Get (creating on first use) the shared `ReferenceQueue`'s global-root
/// handle and resolve it to the live object. Returns `None` if this context
/// doesn't support global roots (mocks) or object construction failed.
fn weak_track_queue(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if let Some(handle) = *weak_track_queue_root().lock() {
        return if handle == 0 {
            None
        } else {
            ctx.resolve_global_root(handle)
        };
    }
    // Lazily create the shared queue. Deliberately do NOT hold the registry
    // lock across this call: construction can allocate/trigger a GC, which
    // can run arbitrary Java finalizers that might re-enter this module on
    // the same thread — holding the lock here would self-deadlock (this
    // `Mutex` isn't reentrant). This opens a benign race if two+ threads
    // both reach this branch as the very first-ever callers: at most a
    // couple of extra queues get constructed; every loser below just
    // resolves and uses the winner's queue instead, abandoning its own
    // (global-rooted) queue object — a harmless one-time leak of a tiny
    // object, bounded by thread count, only possible in this first-use
    // window.
    let queue = match ctx.new_object_initialized("java/lang/ref/ReferenceQueue", "()V", &[]) {
        Ok(Some(Value::Object(Some(q)))) => q,
        _ => {
            weak_track_queue_root().lock().get_or_insert(0);
            return None;
        }
    };
    let handle = ctx.add_global_root(queue);
    let winner = {
        let mut slot = weak_track_queue_root().lock();
        if slot.is_none() {
            *slot = Some(handle);
        }
        (*slot).unwrap()
    };
    if winner != handle {
        // Lost the race — use the already-installed queue instead.
        return if winner == 0 {
            None
        } else {
            ctx.resolve_global_root(winner)
        };
    }
    if handle == 0 {
        None
    } else {
        Some(queue)
    }
}

/// Register the object pinned under `obj_pin` (already inserted into
/// `table()` under `props_key`) for GC-aware reclaim. Best-effort: if this
/// context can't create real `WeakReference`/`ReferenceQueue` objects
/// (mocks) or root them, the entry simply never gets reclaimed early —
/// identical to the pre-fix behavior.
///
/// Takes the pin handle (not a bare `ObjectRef`) because
/// `weak_track_queue` may itself allocate (lazily constructing the shared
/// `ReferenceQueue` on first use), which can trigger a moving GC — the
/// pinned reference is re-read afterwards so the object we hand to
/// `WeakReference`'s constructor is never a stale pre-GC pointer.
fn register_weak_track(
    ctx: &mut dyn NativeContext,
    obj_pin: usize,
    obj_fallback: ObjectRef,
    props_key: usize,
) {
    let Some(queue) = weak_track_queue(ctx) else {
        return;
    };
    let obj = ctx.read_native_pin(obj_pin, obj_fallback);
    let weak_ref = match ctx.new_object_initialized(
        "java/lang/ref/WeakReference",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[Value::Object(Some(obj)), Value::Object(Some(queue))],
    ) {
        Ok(Some(Value::Object(Some(w)))) => w,
        _ => return,
    };
    let global_root = ctx.add_global_root(weak_ref);
    if global_root == 0 {
        return;
    }
    let wk = key_for(ctx, weak_ref);
    weak_track_registry().lock().insert(
        wk,
        WeakTrackEntry {
            props_key,
            global_root,
        },
    );
}

/// Drain every reference the GC has already enqueued (i.e. objects it found
/// unreachable), freeing their side-table slots. Cheap when the queue is
/// empty (a handful of `poll()` calls). Never holds `table()` /
/// `weak_track_registry()` locks while calling back into `ctx` — `poll()`
/// can run arbitrary reference-processing bookkeeping.
fn drain_reclaimed(ctx: &mut dyn NativeContext) {
    let Some(handle) = *weak_track_queue_root().lock() else {
        return;
    };
    if handle == 0 {
        return;
    }
    // Bounded by MAX_TOTAL_OBJECTS: that's the most entries that could ever
    // be simultaneously enqueued (one per tracked object).
    for _ in 0..MAX_TOTAL_OBJECTS {
        // Re-resolve on every iteration rather than hoisting `queue` out of
        // the loop: `poll()` can allocate/trigger GC, and a moving collector
        // may relocate the queue object between iterations. A cached
        // pre-call `ObjectRef` would then be stale for the next `poll()`.
        let Some(queue) = ctx.resolve_global_root(handle) else {
            return;
        };
        let polled = ctx.invoke_virtual(queue, "poll", "()Ljava/lang/ref/Reference;", &[]);
        let reference_obj = match polled {
            Ok(Some(Value::Object(Some(r)))) => r,
            _ => break,
        };
        let wk = key_for(ctx, reference_obj);
        if let Some(entry) = weak_track_registry().lock().remove(&wk) {
            table().lock().remove(&entry.props_key);
            system_props_keys().lock().remove(&entry.props_key);
            ctx.remove_global_root(entry.global_root);
        }
    }
}

// --- system-Properties marker -------------------------------------------------
//
// `Properties.setProperty`/`put` formerly mirrored EVERY write to the global
// system-property store (`ctx.set_system_property`) so that
// `System.getProperties().setProperty(k,v)` propagated (getProperties returns a
// fresh synthetic Properties each call, so there is no stable system-props
// object to target). That over-broad mirror meant ANY `new Properties()
// .setProperty(...)` polluted system properties — and `getProperty` falls back
// to the system store on a side-table miss, so distinct Properties objects
// cross-contaminated. In Hibernate that corrupted the masked password
// (`ConfigurationHelper.maskOut` clones the global props and sets the CLONE's
// password to "****"; the "****" leaked into system props and back into the
// real password -> "Wrong user name or password" on bootstrap).
//
// Now only the synthetic Properties objects produced by `System.getProperties()`
// (marked here) mirror their writes to the system store.
fn system_props_keys() -> &'static Mutex<std::collections::HashSet<usize>> {
    static S: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Mark `obj` as a system-properties view (called from the `System.getProperties`
/// native) so its `setProperty`/`put` writes propagate to the system store.
pub fn mark_system_props(ctx: &dyn NativeContext, obj: ObjectRef) {
    system_props_keys().lock().insert(key_for(ctx, obj));
}

pub fn unmark_system_props(ctx: &dyn NativeContext, obj: ObjectRef) {
    system_props_keys().lock().remove(&key_for(ctx, obj));
}

fn is_system_props(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    system_props_keys().lock().contains(&key_for(ctx, obj))
}

/// Read all bytes from an InputStream by repeatedly invoking `read([B,
/// I, I)I` on the input.  Returns `None` if the stream is null, the
/// total exceeds `MAX_LOAD_BYTES`, or a read errors out.
fn drain_input_stream(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<Vec<u8>> {
    // The InputStream allocated by `Class.getResourceAsStream` and
    // `URL.openStream` is a ByteArrayInputStream with `buf` (byte[]),
    // `pos` (int), `count` (int) fields populated.  Read directly to
    // avoid going through the JDK's bytecode which is fragile in our env.
    //
    // Strategy 1: by-name field lookup (works when the JDK class is loaded
    //   and field names resolve to the correct index).
    // Strategy 2: by-index fallback (indices 0=buf, 1=pos, 3=count) —
    //   the standard JDK ByteArrayInputStream layout; covers synthetic
    //   objects allocated with alloc_concurrent_synthetic where by-name
    //   may not work.
    // Strategy 3: invoke_virtual read([BII)I loop — works for any real
    //   InputStream implementation.

    // --- Strategy 1: by-name ---
    let buf_by_name = match ctx.get_field_by_name(stream, "buf") {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count_by_name = match ctx.get_field_by_name(stream, "count") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let pos_by_name = match ctx.get_field_by_name(stream, "pos") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    if let (Some(arr), Some(c), Some(p)) = (buf_by_name, count_by_name, pos_by_name) {
        // Require c > 0: a BufferedInputStream / JarInputStream wrapping
        // another stream has buf=byte[8192], pos=0, count=0 BEFORE the first
        // fill().  Treating that as "0 bytes available" is wrong — the
        // wrapped stream may have content that only flows through invoke_virtual
        // read([BII)I.  See ActiveMQ XBean factory load: BufferedInputStream
        // wrapping a JarURLConnection stream returned by URLClassLoader.
        if c > 0 && c <= MAX_LOAD_BYTES && p <= c {
            let len = c.saturating_sub(p);
            let mut out = Vec::with_capacity(len);
            for i in p..c {
                if out.len() >= MAX_LOAD_BYTES {
                    return None;
                }
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    out.push(b as u8);
                }
            }
            props_diag_eprintln!(
                "[DRAIN-DBG] drain_input_stream: by-name read {} bytes",
                out.len()
            );
            return Some(out);
        }
    }

    // --- Strategy 2: by-index (standard ByteArrayInputStream layout) ---
    // buf=0, pos=1, mark=2, count=3 — the JDK ByteArrayInputStream
    // instance field order (no instance fields in InputStream parent).
    let buf_by_idx = match ctx.get_field(stream, 0) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count_by_idx = match ctx.get_field(stream, 3) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let pos_by_idx = match ctx.get_field(stream, 1) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    if let (Some(arr), Some(c), Some(p)) = (buf_by_idx, count_by_idx, pos_by_idx) {
        if c > 0 && c <= MAX_LOAD_BYTES && p <= c {
            let len = c.saturating_sub(p);
            let mut out = Vec::with_capacity(len);
            for i in p..c {
                if out.len() >= MAX_LOAD_BYTES {
                    return None;
                }
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    out.push(b as u8);
                }
            }
            props_diag_eprintln!(
                "[DRAIN-DBG] drain_input_stream: by-index read {} bytes",
                out.len()
            );
            return Some(out);
        }
    }

    // --- Strategy 3: invoke_virtual read([BII)I loop ---
    // Handles any real InputStream implementation.
    //
    // Producer-#11 fix (stw-residual-close 20260722): this loop re-enters
    // Java (`InputStream.read` — arbitrary real-JDK bytecode, 20+ frames
    // under WildFly's elytron/infinispan boot); any nested safepoint can run
    // a moving young collection, after which the raw `stream`/`chunk_arr`
    // copies captured before the call are from-space addresses whose memory
    // the collection's `Arena::reset` has already zeroed — the next
    // dispatch reads an all-zero header (the fatal
    // `MechanismDatabase.<init>` `Object.read([CII)I` NSME shape) and the
    // element loop reads garbage. Pin both and re-read the pins (remapped
    // in place by every GC path: initiator, safepoint-arrival, blocked-wake)
    // after every re-entry.
    let chunk_size = 8192usize;
    let stream_pin = ctx.pin_native_root(stream);
    let chunk_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, chunk_size);
    let chunk_pin = ctx.pin_native_root(chunk_arr);
    let mut stream = stream;
    let mut chunk_arr = chunk_arr;
    let mut out = Vec::new();
    loop {
        stream = ctx.read_native_pin(stream_pin, stream);
        chunk_arr = ctx.read_native_pin(chunk_pin, chunk_arr);
        let n = match ctx.invoke_virtual(
            stream,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(chunk_arr)),
                Value::Int(0),
                Value::Int(chunk_size as i32),
            ],
        ) {
            Ok(Some(Value::Int(n))) => n,
            Ok(other) => {
                props_diag_eprintln!(
                    "[DRAIN-DBG] drain_input_stream: read returned non-int {:?}",
                    other
                );
                ctx.unpin_native_roots(stream_pin);
                return None;
            }
            Err(_) => {
                ctx.unpin_native_roots(stream_pin);
                return None;
            }
        };
        if n <= 0 {
            break;
        }
        // The nested read may have moved the chunk array — re-read the pin
        // before pulling elements out of it.
        chunk_arr = ctx.read_native_pin(chunk_pin, chunk_arr);
        for i in 0..n as usize {
            if let Value::Int(b) = ctx.get_array_element(chunk_arr, i) {
                out.push(b as u8);
            }
        }
        if out.len() > MAX_LOAD_BYTES {
            ctx.unpin_native_roots(stream_pin);
            return None;
        }
    }
    ctx.unpin_native_roots(stream_pin);
    // Empty stream / immediate EOF is valid — `Properties.load` must still
    // complete (Surefire booter uses an optional props stream).
    props_diag_eprintln!(
        "[DRAIN-DBG] drain_input_stream: invoke_virtual read path, {} bytes",
        out.len()
    );
    Some(out)
}

/// Parse a Java-style `.properties` file's bytes.  Implements the JLS
/// definition (escapes, line continuations, comments, separators).
/// Returns `(key, value)` pairs in order of appearance.
///
/// The parser is intentionally permissive: malformed escapes degrade
/// to literal characters rather than panicking, and keys without
/// values yield empty-string values (matching JDK behaviour).
fn parse_properties(bytes: &[u8]) -> Vec<(String, String)> {
    // Decode as ISO-8859-1 (Java spec for `Properties.load(InputStream)`).
    // Each byte maps to one Unicode code point in 0..=255.
    let raw: String = bytes.iter().map(|&b| b as char).collect();
    parse_properties_text(&raw)
}

fn parse_properties_strict(bytes: &[u8]) -> Result<Vec<(String, String)>, ()> {
    // Decode as ISO-8859-1 (Java spec for `Properties.load(InputStream)`).
    // Each byte maps to one Unicode code point in 0..=255.
    let raw: String = bytes.iter().map(|&b| b as char).collect();
    parse_properties_text_strict(&raw)
}

fn parse_properties_text(raw: &str) -> Vec<(String, String)> {
    parse_properties_text_inner(raw, false).unwrap_or_default()
}

fn parse_properties_text_strict(raw: &str) -> Result<Vec<(String, String)>, ()> {
    parse_properties_text_inner(raw, true)
}

fn parse_properties_text_inner(
    raw: &str,
    strict_unicode: bool,
) -> Result<Vec<(String, String)>, ()> {
    let mut out = Vec::new();
    let mut iter = raw.split('\n').peekable();
    let mut continued = String::new();

    while let Some(line) = iter.next() {
        // Strip a trailing CR.
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();

        // If we're continuing from a previous line, append.
        let active = if !continued.is_empty() {
            let mut joined = continued.clone();
            joined.push_str(trimmed);
            continued.clear();
            joined
        } else {
            // Skip blank lines and comments.
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                continue;
            }
            trimmed.to_string()
        };

        // Check for line continuation: trailing single backslash.
        // (An odd count of trailing backslashes means continuation.)
        let trailing_bs = active.bytes().rev().take_while(|&b| b == b'\\').count();
        if trailing_bs % 2 == 1 {
            // Drop the trailing backslash; keep accumulating.
            continued = active[..active.len() - 1].to_string();
            continue;
        }

        // Split on first unescaped `=`, `:`, or whitespace.
        let (key, value) = split_key_value(&active);
        let key = unescape_inner(&key, strict_unicode)?;
        let value = unescape_inner(&value, strict_unicode)?;
        if key.len() <= MAX_KV_LEN && value.len() <= MAX_KV_LEN {
            out.push((key, value));
        }
        if out.len() >= MAX_PROPS_PER_OBJECT {
            break;
        }
    }
    Ok(out)
}

/// Split a logical line into (key, value) at the first unescaped `=`,
/// `:`, or whitespace separator.  Trailing whitespace is trimmed from
/// the key; leading whitespace is trimmed from the value.
fn split_key_value(line: &str) -> (String, String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if c == b'=' || c == b':' {
            // Found explicit separator.
            let key = line[..i].trim_end();
            let value = line[i + 1..].trim_start();
            return (key.to_string(), value.to_string());
        }
        if c == b' ' || c == b'\t' || c == b'\x0c' {
            // Whitespace separator; consume any subsequent whitespace
            // and a single optional `=`/`:`.
            let key = line[..i].to_string();
            let mut j = i + 1;
            while j < bytes.len() {
                let cc = bytes[j];
                if cc == b' ' || cc == b'\t' || cc == b'\x0c' {
                    j += 1;
                } else if cc == b'=' || cc == b':' {
                    j += 1;
                    while j < bytes.len()
                        && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\x0c')
                    {
                        j += 1;
                    }
                    break;
                } else {
                    break;
                }
            }
            return (key, line[j..].to_string());
        }
        i += 1;
    }
    // No separator — whole line is the key, value is empty.
    (line.to_string(), String::new())
}

/// Decode Java `.properties` escapes (`\n`, `\t`, `\r`, `\f`, `\\`, `\"`,
/// `\'`, `\<space>`, `\:`, `\=`, `\uXXXX`).  Unknown escapes degrade
/// to literal characters.
///
/// Surrogate handling matches `java.util.Properties.load` as closely as a
/// `String`-returning helper can:
///   * A high `\uD800..\uDBFF` immediately followed by a low `\uDC00..\uDFFF`
///     is combined into the supplementary code point it encodes (the prior
///     implementation dropped both halves, losing every emoji / CJK-ext char).
///   * A malformed `\u` (zero or fewer-than-four hex digits) is handled
///     loudly (warn + best-effort decode of the digits present), not silently
///     tolerated, mirroring the JDK's "Malformed \\uxxxx encoding" error.
///
/// LIMITATION / CROSS-FILE FOLLOW-UP: a *lone* surrogate `\uXXXX` (a high or
/// low half with no matching pair) is a valid single UTF-16 code unit that a
/// Rust `String` cannot represent. Exact preservation requires storing the
/// value as `[u16]` units and materialising the Java string via
/// `vm::vm_object::create_java_string_from_units` (the "wide-unit path", cf.
/// the SB-13 GroovyLexer fix). That path is not reachable from this
/// `String`-typed pipeline (`put_kv`/`get_kv` store `String`, and
/// `NativeContext::create_string` re-`encode_utf16`s its `&str`), so we
/// substitute U+FFFD and warn rather than silently dropping the unit. Wiring a
/// units-aware value channel through the side-table is a separate change.
fn unescape(s: &str) -> String {
    unescape_inner(s, false).unwrap_or_default()
}

fn unescape_inner(s: &str, strict_unicode: bool) -> Result<String, ()> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{000c}'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(' ') => out.push(' '),
            Some(':') => out.push(':'),
            Some('=') => out.push('='),
            Some('u') => {
                // `\uXXXX` — a single UTF-16 code unit. Read the hex digits.
                let (code, seen) = read_u_escape(&mut chars);
                if seen == 0 {
                    if strict_unicode {
                        return Err(());
                    }
                    // `\u` with no following hex digit at all. The JDK's
                    // `Properties.load` (`loadConvert`) throws
                    // IllegalArgumentException("Malformed \\uxxxx encoding.")
                    // here. The permissive helper preserves the legacy
                    // best-effort behavior for non-load callers and tests.
                    tracing::warn!(
                        target: "cratonvm_vm::props_sidetable",
                        "Malformed \\u escape in .properties value (no hex digits); \
                         preserving 'u' literally"
                    );
                    out.push('u');
                    continue;
                }
                if seen < 4 {
                    if strict_unicode {
                        return Err(());
                    }
                    // Fewer than 4 hex digits before a non-hex char/EOF. The
                    // JDK treats this as malformed and throws; the permissive
                    // helper decodes the digits actually present.
                    tracing::warn!(
                        target: "cratonvm_vm::props_sidetable",
                        digits = ?seen,
                        "Malformed (short) \\u escape in .properties value; \
                         decoding the hex digits present"
                    );
                }
                if (0xD800..=0xDBFF).contains(&code) {
                    // High surrogate. A valid supplementary code point is
                    // written in .properties as TWO escapes: a high surrogate
                    // immediately followed by `\uDC00..\uDFFF`. Combine them
                    // into one Rust `char` (the previous code dropped BOTH
                    // halves because each lone half failed `char::from_u32`).
                    if let Some(low) = peek_low_surrogate(&mut chars) {
                        let cp = 0x10000 + (((code - 0xD800) << 10) | (low - 0xDC00));
                        if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                            continue;
                        }
                    }
                    // Lone high surrogate (no matching low half). A Rust
                    // `String` cannot hold an unpaired UTF-16 surrogate, so
                    // we cannot preserve it here without the wide-unit path
                    // (`create_java_string_from_units`, in vm/vm_object.rs)
                    // which this `String`-returning function can't reach.
                    // Substitute U+FFFD rather than silently dropping the
                    // unit. See CROSS-FILE note in the header doc.
                    tracing::warn!(
                        target: "cratonvm_vm::props_sidetable",
                        unit = ?code,
                        "Lone high surrogate (\\uXXXX) in .properties value; \
                         cannot be stored as a Rust String — substituting \
                         U+FFFD (wide-unit storage needed for exact \
                         preservation)"
                    );
                    out.push('\u{FFFD}');
                } else if (0xDC00..=0xDFFF).contains(&code) {
                    // Lone low surrogate (no preceding high half). Same
                    // limitation as the lone-high case above.
                    tracing::warn!(
                        target: "cratonvm_vm::props_sidetable",
                        unit = ?code,
                        "Lone low surrogate (\\uXXXX) in .properties value; \
                         cannot be stored as a Rust String — substituting \
                         U+FFFD (wide-unit storage needed for exact \
                         preservation)"
                    );
                    out.push('\u{FFFD}');
                } else if let Some(ch) = char::from_u32(code) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    Ok(out)
}

/// Consume the hex digits of a `\uXXXX` escape (the `\u` prefix has already
/// been consumed). Reads up to 4 ASCII hex digits and returns the decoded
/// code unit together with how many digits were actually read (`0..=4`).
///
/// Fewer than 4 digits means the escape was malformed per the JDK; callers
/// decide how loudly to react. We deliberately read *only* what's present
/// rather than over-consuming, so a non-hex follow-on character (and any
/// subsequent escapes) is still processed by the caller.
fn read_u_escape<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) -> (u32, u32) {
    let mut code = 0u32;
    let mut seen = 0u32;
    while seen < 4 {
        match chars.peek() {
            Some(&h) if h.is_ascii_hexdigit() => {
                code = (code << 4) | h.to_digit(16).unwrap();
                chars.next();
                seen += 1;
            }
            _ => break,
        }
    }
    (code, seen)
}

/// If the next two characters are a `\uXXXX` escape whose value is a low
/// surrogate (`0xDC00..=0xDFFF`), consume them and return that code unit;
/// otherwise leave the iterator untouched and return `None`.
///
/// Used to greedily pair a high surrogate with its trailing low surrogate so
/// supplementary code points (emoji, CJK-ext, …) round-trip through the
/// `String`-typed pipeline. We require the full 4-hex-digit form: a short
/// `\uDC` after a high surrogate is itself malformed and is left for the main
/// loop to report.
fn peek_low_surrogate<I>(chars: &mut std::iter::Peekable<I>) -> Option<u32>
where
    I: Iterator<Item = char> + Clone,
{
    // Speculatively clone the cursor so a non-matching lookahead costs us
    // nothing — we only advance the real iterator on a confirmed low
    // surrogate. `Peekable<Chars>` is `Clone` (chars over a &str slice).
    let mut probe = chars.clone();
    if probe.next() != Some('\\') || probe.next() != Some('u') {
        return None;
    }
    let (code, seen) = read_u_escape(&mut probe);
    if seen == 4 && (0xDC00..=0xDFFF).contains(&code) {
        // Commit: fast-forward the real iterator past `\uXXXX` (6 chars).
        for _ in 0..6 {
            chars.next();
        }
        Some(code)
    } else {
        None
    }
}

/// Insert (or overwrite) a key/value pair in the side-table for a
/// given Properties object.  Enforces per-object and global caps.
///
/// The global cap is now a *concurrently alive* cap, not a *lifetime* one:
/// hitting it triggers an opportunistic reclaim of already-GC'd entries
/// (`drain_reclaimed`), and if that's not enough, an explicit GC cycle to
/// give truly-dead entries a chance to be discovered before falling back to
/// the (now extremely rare) old silent-drop behavior. See
/// `register_weak_track` / `drain_reclaimed` above.
fn put_kv(ctx: &mut dyn NativeContext, obj: ObjectRef, key: &str, value: &str) {
    if key.len() > MAX_KV_LEN || value.len() > MAX_KV_LEN {
        return;
    }
    let k = key_for(ctx, obj);
    // `drain_reclaimed`/`force_gc` below can run a moving collection; pin
    // `obj` so the reference we pass to `register_weak_track` afterwards is
    // still valid (not a pre-GC, potentially-forwarded stale pointer).
    let obj_pin = ctx.pin_native_root(obj);
    let mut at_cap = {
        let t = table().lock();
        t.len() >= MAX_TOTAL_OBJECTS && !t.contains_key(&k)
    };
    if at_cap {
        drain_reclaimed(ctx);
        at_cap = {
            let t = table().lock();
            t.len() >= MAX_TOTAL_OBJECTS && !t.contains_key(&k)
        };
    }
    if at_cap {
        // Nothing had been GC'd yet (e.g. a tight allocation loop that
        // hasn't triggered a collection) — force one so genuinely-dead
        // entries get a chance to be reclaimed before we give up.
        ctx.force_gc();
        drain_reclaimed(ctx);
        at_cap = {
            let t = table().lock();
            t.len() >= MAX_TOTAL_OBJECTS && !t.contains_key(&k)
        };
    }
    if at_cap {
        // Silently dropping a real `Properties.load` result is how a whole
        // configuration file disappears with no error anywhere -- make it
        // visible under CRATONVM_DIAG_PROPERTIES at least.
        props_diag_eprintln!(
            "[PROPS-DBG] put_kv DROPPED key={key} — side-table at capacity ({} objects)",
            table().lock().len()
        );
        ctx.unpin_native_roots(obj_pin);
        return;
    }
    let is_new = {
        let mut t = table().lock();
        let is_new = !t.contains_key(&k);
        let entry = t.entry(k).or_default();
        if entry.len() < MAX_PROPS_PER_OBJECT || entry.contains_key(key) {
            entry.insert(key.to_string(), value.to_string());
        }
        is_new
    };
    if is_new {
        register_weak_track(ctx, obj_pin, obj, k);
    }
    ctx.unpin_native_roots(obj_pin);
}

/// Look up a key in the side-table.  Returns `None` if either the
/// object isn't tracked or the key is absent.
fn get_kv(ctx: &dyn NativeContext, obj: ObjectRef, key: &str) -> Option<String> {
    let k = key_for(ctx, obj);
    table().lock().get(&k)?.get(key).cloned()
}

/// Remove a key from the side-table.  Returns the previous value if it
/// was present, or `None` if either the object isn't tracked or the key
/// was absent.  Used by `native_properties_remove` to back the JDK
/// `Properties.remove(Object) Object` semantics.
fn remove_kv(ctx: &dyn NativeContext, obj: ObjectRef, key: &str) -> Option<String> {
    let k = key_for(ctx, obj);
    let mut t = table().lock();
    let entry = t.get_mut(&k)?;
    // `shift_remove`, not `swap_remove`: the side-table is insertion-ordered
    // and every enumeration native reads that order, so a removal must not
    // teleport the last key into the hole it leaves behind.
    entry.shift_remove(key)
}

/// Cross-module read access for callers that receive a `Properties` object
/// behind an erased `Map` type (e.g. surefire `PropertiesWrapper`).
pub(crate) fn get_property_from_sidetable(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    key: &str,
) -> Option<String> {
    get_kv(ctx, obj, key)
}

/// Public re-export of `remove_kv` so `System.clearProperty` (in `lib.rs`)
/// can keep the cached `System.getProperties()` singleton's side-table in
/// sync when a key is removed via the *static* `System` entry point rather
/// than through the `Properties` object itself -- see the matching
/// `store_property_in_sidetable` call in `System.setProperty`'s native for
/// the full rationale (SC-web-method-spel RC-A). This one-key sync is needed
/// in addition to the wholesale `replace_sidetable` resync that already
/// happens on every fresh `System.getProperties()` call, because a caller
/// that already holds a reference to the singleton (e.g. Spring's
/// `systemProperties` bean) never triggers that resync again.
pub fn remove_property_from_sidetable(ctx: &dyn NativeContext, obj: ObjectRef, key: &str) {
    remove_kv(ctx, obj, key);
}

/// Public re-export of `put_kv` so other modules (e.g. the surefire
/// `SystemPropertyManager.loadProperties` native in `lib.rs`) can store
/// key/value pairs in the side-table keyed by an arbitrary object
/// reference.  Used to back `PropertiesWrapper` lookups when the
/// real-JDK CHM round-trip does not populate the wrapper's internal
/// `properties` field correctly under our interpreter.
pub fn store_property_in_sidetable(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    key: &str,
    value: &str,
) {
    put_kv(ctx, obj, key, value);
}

/// Wholesale-replace the side-table snapshot for `obj` with exactly `entries`.
///
/// Unlike repeated [`store_property_in_sidetable`] (which only adds / overwrites
/// individual keys), this DROPS any key no longer present in `entries`. It lets
/// `System.getProperties()` resync its stable singleton's enumeration view to
/// the current system-property store on every call, so a `System.clearProperty`
/// between calls is reflected (the entry disappears) — not just additions.
pub fn replace_sidetable(ctx: &dyn NativeContext, obj: ObjectRef, entries: &[(String, String)]) {
    let k = key_for(ctx, obj);
    let mut m = PropsMap::default();
    for (key, value) in entries {
        if key.len() > MAX_KV_LEN || value.len() > MAX_KV_LEN {
            continue;
        }
        if m.len() >= MAX_PROPS_PER_OBJECT {
            break;
        }
        m.insert(key.clone(), value.clone());
    }
    table().lock().insert(k, m);
}

/// Public snapshot of side-table entries for a given object, used by
/// surefire `setAsSystemProperties` etc. to iterate entries without
/// going through the inner Map field.
pub fn snapshot_sidetable(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<(String, String)> {
    snapshot_kv(ctx, obj)
}

/// Public re-export of `drain_input_stream` for use from `lib.rs`.
pub fn drain_input_stream_pub(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<Vec<u8>> {
    drain_input_stream(ctx, stream)
}

/// Public re-export of `parse_properties` for use from `lib.rs`.
pub fn parse_properties_pub(bytes: &[u8]) -> Vec<(String, String)> {
    parse_properties(bytes)
}

/// Snapshot the side-table entries for a Properties object.  Returns
/// an empty vector if the object isn't tracked.  Used by `keySet`,
/// `entrySet`, `values`, `keys`, `elements` natives so the iteration
/// view is decoupled from the live mutable side-table.
fn snapshot_kv(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<(String, String)> {
    let k = key_for(ctx, obj);
    match table().lock().get(&k) {
        Some(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        None => Vec::new(),
    }
}

/// The side-table entries for `obj`, ordered the way the **real JDK** would
/// enumerate them.
///
/// Why this exists (`GitInfoContributorTests.withGitIdAndAbbrev`, gh-11892):
/// since JDK 9 `java.util.Properties` is not a `Hashtable` bucket walk at all —
/// it delegates to a private `ConcurrentHashMap<Object,Object> map`, so
/// `keySet()`/`entrySet()`/`stringPropertyNames()` all enumerate in *that CHM's*
/// bucket order. Real code depends on it. Spring Boot's `MapBinder.bindEntries`
/// walks the property source once and does `map.computeIfAbsent(key, ...)`, so
/// for the ambiguous `commit.id` (a leaf value) + `commit.id.abbrev` (a deeper
/// key) shape, **whichever name the iteration yields first decides** whether the
/// `id` entry becomes a scalar String or a nested `Map`. On HotSpot the four
/// git keys come out `[commit.id.full, branch, commit.id.abbrev, commit.id]` —
/// `commit.id` last — so the nested map wins, which is exactly what gh-11892
/// specifies. Ordering the same keys by `FxHashMap` bucket instead put
/// `commit.id` first and silently produced the scalar.
///
/// So: when the receiver has a populated real `map` CHM (every `new
/// Properties()` — `put`/`setProperty`/`load` all mirror String entries into it),
/// that CHM *is* the ordering authority, and CratonVM's own
/// `ConcurrentHashMap` already reproduces HotSpot's order exactly. Keys the CHM
/// does not carry (the synthetic `System.getProperties()` singleton, surefire's
/// `store_property_in_sidetable` path) follow in side-table insertion order.
///
/// Values always come from the side-table; only the *order* is borrowed. Entries
/// the CHM holds but the side-table does not are the callers' business — they
/// append them via [`chm_extra_entries`] with the side-table keys as the skip
/// set, exactly as before.
fn ordered_snapshot_kv(ctx: &mut dyn NativeContext, obj: &mut ObjectRef) -> Vec<(String, String)> {
    let side = snapshot_kv(ctx, *obj);
    if side.len() < 2 {
        return side;
    }
    // `chm_key_order` re-enters Java (`entrySet`/`iterator`/`next`/`getKey`),
    // so it allocates, so it is a GC point — unlike `snapshot_kv`, which is a
    // pure side-table read and is why none of these callers used to need a pin
    // here. Taking `obj` by `&mut` is the point: it forces every caller's own
    // receiver to be refreshed across the walk instead of silently carrying a
    // pre-GC address into the pins and virtual dispatches that follow. That
    // stranding is what a Family-1 stale-`ObjectRef` failure looks like from
    // Java: `NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
    // because "<local5>" is null`, several frames away and nowhere near here.
    let pin = ctx.pin_native_root(*obj);
    let order = chm_key_order(ctx, *obj);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.unpin_native_roots(pin);
    if order.is_empty() {
        // No real CHM backing (the synthetic `System.getProperties()`
        // singleton, surefire's `store_property_in_sidetable` path). The
        // side-table's own insertion order stands.
        return side;
    }
    reorder_by(&side, &order)
}

/// Reorder `side` (insertion-ordered side-table entries) to follow `order`
/// (the real JDK `map` CHM's key order), appending the entries `order` does not
/// mention in their original relative order.
///
/// Split out of [`ordered_snapshot_kv`] purely so it can be unit-tested without
/// a `NativeContext`: the ordering rule is the whole point of the fix, and an
/// end-to-end Spring Boot run is far too coarse an instrument to pin it.
///
/// Every `side` entry appears exactly once in the result, and no entry is
/// invented — `order` only permutes, never filters. Values always come from
/// `side`; `order` contributes nothing but position.
fn reorder_by(side: &[(String, String)], order: &[String]) -> Vec<(String, String)> {
    let index: FxHashMap<&str, &str> = side.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut out: Vec<(String, String)> = Vec::with_capacity(side.len());
    let mut used: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(side.len());
    for key in order {
        if let Some(value) = index.get(key.as_str()) {
            if used.insert(key.as_str()) {
                out.push((key.clone(), (*value).to_string()));
            }
        }
    }
    for (k, v) in side {
        if !used.contains(k.as_str()) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// The String keys of `obj`'s real JDK `map` `ConcurrentHashMap` backing, in
/// that map's own iteration order. Empty when there is no such backing (the
/// synthetic `System.getProperties()` object) or it holds nothing.
///
/// Deliberately reuses [`chm_extra_entries`] with an empty skip set rather than
/// open-coding a second `entrySet()` walk: that walk's pin discipline has been
/// corrected twice already (see its `cceres3` comments), and duplicating it
/// would duplicate the hazard.
fn chm_key_order(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<String> {
    chm_extra_entries(ctx, obj, &std::collections::HashSet::new())
        .into_iter()
        .filter_map(|(_key_obj, _value, key_string)| key_string)
        .collect()
}

/// Number of entries the side-table holds for `obj` (0 if untracked).
fn count_kv(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let k = key_for(ctx, obj);
    table().lock().get(&k).map(|m| m.len()).unwrap_or(0)
}

/// The set of keys the String-only side-table holds for `obj`.  Used as the
/// "already represented" skip set when merging in the CHM-backing entries —
/// see [`chm_extra_entries`].
fn side_key_set(ctx: &dyn NativeContext, obj: ObjectRef) -> std::collections::HashSet<String> {
    match table().lock().get(&key_for(ctx, obj)) {
        Some(m) => m.keys().cloned().collect(),
        None => std::collections::HashSet::new(),
    }
}

/// Collect entries from the Properties' real-JDK `map` ConcurrentHashMap
/// backing whose key string is NOT in `skip`.  Returns `(key_obj, value,
/// key_string)` triples; the value is the *real* stored object (which may be a
/// non-String such as a `Class`), and `key_string` is `None` for the rare
/// non-String key.
///
/// Why this exists: `native_properties_put`/`putAll` store **non-String
/// values** ONLY in the CHM backing (the side-table is `String -> String`).
/// Every read native that consults only the side-table therefore silently
/// drops those entries — `size()` undercounts, `entrySet()`/`keySet()`/
/// `values()` omit them, `containsKey()` returns false, `isEmpty()` reports
/// empty. Kafka's `ConsumerConfig(Properties)` is the canonical victim: its
/// required `key.deserializer` is supplied as a `Class` value, so
/// `AbstractConfig`'s `originals.entrySet()` walk never sees it and it throws
/// `Missing required configuration "key.deserializer"`.
///
/// Merging here is *additive* and *de-duplicated*: every String entry is
/// mirrored into BOTH stores, so `skip` (the side-table keys) prevents
/// double-counting; only the CHM-exclusive (non-String-valued) entries come
/// back. When the receiver has no CHM backing (e.g. a Properties populated
/// purely through the surefire `store_property_in_sidetable` path), this is a
/// no-op and the side-table view stands alone.
fn chm_extra_entries(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    skip: &std::collections::HashSet<String>,
) -> Vec<(ObjectRef, Value, Option<String>)> {
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        _ => return Vec::new(),
    };
    let set = match ctx.invoke_virtual(chm, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return Vec::new(),
    };
    let set_pin = ctx.pin_native_root(set);
    let set_cur = ctx.read_native_pin(set_pin, set);
    let it = match ctx.invoke_virtual(set_cur, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => {
            ctx.unpin_native_roots(set_pin);
            return Vec::new();
        }
    };
    ctx.unpin_native_roots(set_pin);

    struct PinnedExtraEntry {
        key_pin: usize,
        key_fallback: ObjectRef,
        value: Value,
        value_pin: Option<(usize, ObjectRef)>,
        key_string: Option<String>,
    }

    let it_pin = ctx.pin_native_root(it);
    let mut pinned = Vec::new();
    loop {
        let it_cur = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) if n != 0 => {}
            _ => break,
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let entry = match ctx.invoke_virtual(it_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let entry_cur = ctx.read_native_pin(entry_pin, entry);
        let key_obj = match ctx.invoke_virtual(entry_cur, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let key_pin = ctx.pin_native_root(key_obj);
        let entry_cur = ctx.read_native_pin(entry_pin, entry);
        let value = match ctx.invoke_virtual(entry_cur, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            _ => {
                ctx.unpin_native_roots(key_pin);
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let value_pin = match value {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let key_cur = ctx.read_native_pin(key_pin, key_obj);
        let kstr = ctx.read_string(key_cur);
        // cceres3 (unpin-ring provenance, base=6 prev_len=9): do NOT release
        // entry_pin here — key_pin/value_pin were pushed ABOVE it, so this
        // truncate dropped them both and every handle stored in `pinned`
        // dangled from this iteration on (read_native_pin then silently
        // returned the raw, possibly-stale snapshot refs — the stale
        // Properties pairs behind the domain sb_append/putAll captures).
        // The end-of-function unpin(it_pin) releases the whole range.
        if let Some(ref s) = kstr {
            if skip.contains(s) {
                if let Some((pin, _)) = value_pin {
                    ctx.unpin_native_roots(pin);
                }
                ctx.unpin_native_roots(key_pin);
                continue; // String entry already represented by the side-table
            }
        }
        pinned.push(PinnedExtraEntry {
            key_pin,
            key_fallback: key_obj,
            value,
            value_pin,
            key_string: kstr,
        });
        if pinned.len() >= MAX_PROPS_PER_OBJECT {
            break;
        }
    }
    // cceres3 (PIN-DANGLING live capture): do NOT unpin it_pin here.
    // `unpin_native_roots` TRUNCATES the pin stack, and every accumulated
    // entry/key/value pin sits ABOVE it_pin — the read-back below was
    // silently degrading to the raw, possibly-stale snapshot refs (the
    // stale Properties pairs behind the entrySet-view / putAll captures).
    // The single truncate after the read-back releases everything at once.

    let mut out = Vec::with_capacity(pinned.len());
    for entry in &pinned {
        let key_obj = ctx.read_native_pin(entry.key_pin, entry.key_fallback);
        let value = match (entry.value, entry.value_pin) {
            (Value::Object(Some(_)), Some((pin, fallback))) => {
                Value::Object(Some(ctx.read_native_pin(pin, fallback)))
            }
            _ => entry.value,
        };
        out.push((key_obj, value, entry.key_string.clone()));
    }
    let _ = pinned;
    ctx.unpin_native_roots(it_pin);
    out
}

/// After native `load` fills the side-table, mirror each (k,v) into the
/// JDK `Properties` backing store.  Since JDK 17+, entries live in a
/// `ConcurrentHashMap` field `map`; `stringPropertyNames()` (used by
/// Surefire `SystemPropertyManager.loadProperties`) enumerates via
/// `entrySet()` on that map — not the legacy Hashtable table.  If `map`
/// is absent (very old layout), fall back to `Hashtable.put` (invokespecial).
fn mirror_loaded_entries_to_properties_backend(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    parsed: &[(String, String)],
) {
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        Value::Object(None) | _ => {
            let Some(m) = (match ctx.new_object("java/util/concurrent/ConcurrentHashMap") {
                Ok(Some(Value::Object(Some(o)))) => Some(o),
                _ => None,
            }) else {
                for (k, v) in parsed {
                    let k_obj = ctx.create_string(k);
                    let v_obj = ctx.create_string(v);
                    let _ = ctx.invoke_special(
                        "java/util/Hashtable",
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[
                            Value::Object(Some(this)),
                            Value::Object(Some(k_obj)),
                            Value::Object(Some(v_obj)),
                        ],
                    );
                }
                return;
            };
            let _ = ctx.invoke(
                "java/util/concurrent/ConcurrentHashMap",
                "<init>",
                "()V",
                &[Value::Object(Some(m))],
            );
            ctx.set_field_by_name(this, "map", Value::Object(Some(m)));
            m
        }
    };

    for (k, v) in parsed {
        let k_obj = ctx.create_string(k);
        let v_obj = ctx.create_string(v);
        let _ = ctx.invoke_virtual(
            chm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(k_obj)), Value::Object(Some(v_obj))],
        );
    }
}

/// Store entries parsed by a native `load` into the receiver.
///
/// HotSpot's `Properties.load0` hands every parsed pair to the VIRTUAL
/// `put(Object,Object)`, so a subclass that overrides `put` observes each
/// entry — Spring Boot's buildSrc `AntoraAsciidocAttributes` loads its
/// attribute template through an anonymous Properties subclass whose
/// `put` collects into a separate ordered map and never stores into the
/// Properties object at all. The old tail (`put_kv` + backend mirror)
/// bypassed that dispatch entirely, so subclass overrides silently saw
/// zero entries. Keep the side-table fast path for receivers whose
/// runtime class IS `java/util/Properties`; for genuine subclasses,
/// dispatch each entry through `put` (a non-overriding subclass lands
/// back on the registered Properties.put native, same net effect).
fn store_parsed_entries(ctx: &mut dyn NativeContext, this: ObjectRef, parsed: &[(String, String)]) {
    let cid = ctx.class_id_of_object(this);
    let is_exact = ctx
        .class_name_of_id(cid)
        .is_none_or(|n| n == "java/util/Properties");
    // GC-safety (both branches): `put_kv` can force a collection and
    // `mirror_loaded_entries_to_properties_backend` runs real `put` bytecode,
    // so the receiver can be relocated MID-LOOP. The side-table is keyed off
    // the receiver's identity/address, so continuing with a stale reference
    // scatters the file's entries across two keys -- and the later
    // `getProperty` reads the live one and finds nothing. The exact-class
    // branch had no pin at all.
    let this_pin = ctx.pin_native_root(this);
    if is_exact {
        for (k, v) in parsed {
            let this_cur = ctx.read_native_pin(this_pin, this);
            put_kv(ctx, this_cur, k, v);
        }
        let this_cur = ctx.read_native_pin(this_pin, this);
        mirror_loaded_entries_to_properties_backend(ctx, this_cur, parsed);
        ctx.unpin_native_roots(this_pin);
        return;
    }
    // Subclass: every invoke below can trigger a moving GC, so re-read the
    // receiver (and the key string, which is allocated before the value
    // string) through pins on each iteration.
    for (k, v) in parsed {
        let k_obj = ctx.create_string(k);
        let k_pin = ctx.pin_native_root(k_obj);
        let v_obj = ctx.create_string(v);
        let k_obj = ctx.read_native_pin(k_pin, k_obj);
        let this_cur = ctx.read_native_pin(this_pin, this);
        let _ = ctx.invoke_virtual(
            this_cur,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(k_obj)), Value::Object(Some(v_obj))],
        );
        ctx.unpin_native_roots(k_pin);
    }
    ctx.unpin_native_roots(this_pin);
}

/// Native `Properties.load(InputStream)` — drains the stream, parses
/// the bytes as a Java `.properties` file, and populates the side-
/// table for `this`.
fn native_properties_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(None),
    };
    // Producer-#11 fix: `drain_input_stream` can re-enter Java and GC;
    // refresh `this` through a pin before keying the side-table store.
    let this_pin = ctx.pin_native_root(this);
    let bytes = match drain_input_stream(ctx, stream) {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    if bytes.len() > MAX_LOAD_BYTES {
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }
    let parsed = match parse_properties_strict(&bytes) {
        Ok(parsed) => parsed,
        Err(()) => {
            ctx.unpin_native_roots(this_pin);
            return Err(throw_malformed_unicode_escape(ctx));
        }
    };
    props_diag_eprintln!(
        "[PROPS-DBG] native_properties_load: parsed {} entries from {} bytes",
        parsed.len(),
        bytes.len()
    );
    for (k, v) in &parsed {
        if k.contains("ApplicationContext") || k.contains("ContextFactory") {
            let preview_len = v.len().min(80);
            props_diag_eprintln!(
                "[PROPS-DBG] KEY={} VALUE_LEN={} VALUE_START={}",
                k,
                v.len(),
                &v[..preview_len]
            );
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    store_parsed_entries(ctx, this, &parsed);
    let this = ctx.read_native_pin(this_pin, this);
    props_diag_eprintln!(
        "[PROPS-DBG] native_properties_load: side-table now has {} entries for obj {:?}",
        count_kv(ctx, this),
        this
    );
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// Downgrade a Rust `String` whose code points represent ISO-8859-1
/// characters (as produced by reading a Reader chunk-by-chunk) into
/// the `u8` byte sequence that `parse_properties` expects.  Each
/// `char` < 256 round-trips losslessly; anything above the Latin-1
/// range collapses to `b'?'`, mirroring the `b as char` decoding side
/// in `parse_properties` (which only emits chars in 0..=255).
///
/// Kept as a pure helper so it can be unit-tested without a full
/// `NativeContext`.
fn iso_8859_1_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp < 256 {
            out.push(cp as u8);
        } else {
            out.push(b'?');
        }
    }
    out
}

/// Native `Properties.load(Reader)` — RKC16N.1.  jboss-modules'
/// `org.jboss.modules.Main.<clinit>` reads `version.properties`
/// through a `BufferedReader` and calls this overload directly, so a
/// missing native here is a hard linkage error before `main` runs.
///
/// Strategy: pull the Reader's contents into a `String` 4 KiB at a
/// time via `Reader.read([CII)I` (the same loop shape every JDK
/// `BufferedReader` tolerates), then downgrade the accumulated text
/// to ISO-8859-1 bytes and reuse `parse_properties` / `put_kv` — the
/// same back-half as the InputStream overload.  We deliberately do
/// not call `Reader.close()` (the caller owns the stream lifecycle,
/// matching real JDK `Properties.load(Reader)`).
fn native_properties_load_reader(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let reader = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };

    // Scratch buffer for `Reader.read(char[], 0, len)`.  4 KiB is
    // the size every JDK BufferedReader uses internally, so this
    // matches the shape callers expect and avoids tiny chunked reads.
    //
    // Producer-#11 fix (stw-residual-close 20260722): the read loop below
    // re-enters Java (`Reader.read` — the real-JDK
    // InputStreamReader/StreamDecoder bytecode); any nested safepoint can
    // run a moving young collection, after which the raw
    // `this`/`reader`/`buf` copies captured before the call are from-space
    // addresses whose memory `Arena::reset` has already zeroed. This was
    // captured live as the fatal WildFly `MechanismDatabase.<init>`
    // `java/lang/Object.read([CII)I` NSME during parallel-extension-add
    // (elytron reads MechanismDatabase.properties through exactly this
    // loop while ~30 sibling threads run moving GCs). Pin all three and
    // re-read the pins after every re-entry.
    const CHUNK: usize = 4096;
    let this_pin = ctx.pin_native_root(this);
    let reader_pin = ctx.pin_native_root(reader);
    let buf = ctx.new_array(ArrayElementType::Char, CHUNK);
    let buf_pin = ctx.pin_native_root(buf);
    let mut reader = reader;
    let mut buf = buf;

    // Cap accumulated text at 2 * MAX_LOAD_BYTES chars.  In ISO-8859-1
    // each char re-encodes to one byte, so this matches the byte cap
    // applied to the InputStream path while leaving headroom for any
    // multi-byte chars that get downgraded to `?`.
    let char_cap = MAX_LOAD_BYTES.saturating_mul(2);
    let mut accumulated = String::new();

    loop {
        reader = ctx.read_native_pin(reader_pin, reader);
        buf = ctx.read_native_pin(buf_pin, buf);
        let res = match ctx.invoke_virtual(
            reader,
            "read",
            "([CII)I",
            &[
                Value::Object(Some(buf)),
                Value::Int(0),
                Value::Int(CHUNK as i32),
            ],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let n = match res {
            Some(Value::Int(n)) => n,
            // Anything else (None, non-int) means the read protocol
            // misbehaved — treat as EOF rather than looping forever.
            _ => break,
        };
        if n <= 0 {
            // n == -1 is EOF; n == 0 is also a valid early exit per
            // the Reader contract on a non-blocking but exhausted
            // source.  Either way, stop.
            break;
        }
        let n = n as usize;
        let n = n.min(CHUNK);
        // The nested read may have moved the chunk array — re-read the pin
        // before pulling elements out of it.
        buf = ctx.read_native_pin(buf_pin, buf);
        for i in 0..n {
            if accumulated.len() >= char_cap {
                break;
            }
            if let Value::Int(c) = ctx.get_array_element(buf, i) {
                // Java chars are unsigned 16-bit code units.  Mask
                // before widening so a sign-extended negative `int`
                // doesn't yield an out-of-range code point.
                let cu = (c as u32) & 0xFFFF;
                if let Some(ch) = char::from_u32(cu) {
                    accumulated.push(ch);
                }
            }
        }
        if accumulated.len() >= char_cap {
            break;
        }
    }

    if accumulated.len() > MAX_LOAD_BYTES {
        return Ok(None);
    }
    // SPR-TEST-PROPS-READER.1 (2026-07-08) — `Properties.load(Reader)`
    // parses the already-decoded character stream. Do not downgrade non-Latin-1
    // chars to `?`; intentionally wrong UTF-8 decoding must preserve U+FFFD.
    let parsed = match parse_properties_text_strict(&accumulated) {
        Ok(parsed) => parsed,
        Err(()) => {
            ctx.unpin_native_roots(this_pin);
            return Err(throw_malformed_unicode_escape(ctx));
        }
    };
    // Refresh `this` through its pin — the read loop's re-entries may have
    // relocated it — before keying the side-table store.
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    store_parsed_entries(ctx, this, &parsed);
    Ok(None)
}

/// Read the `defaults` Properties reference (the head of the fallback chain
/// set by `new Properties(Properties)`), resolving the field BY NAME so it is
/// robust to CratonVM's dual native/real `Properties` field layouts. Returns
/// `None` when the receiver has no defaults (the common case) or the field
/// can't be resolved.
fn props_defaults(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let idx = ctx.resolve_field_index("java/util/Properties", "defaults")?;
    match ctx.get_field(this, idx) {
        Value::Object(Some(d)) => Some(d),
        _ => None,
    }
}

/// Native `Properties.getProperty(String)` — checks the side-table first, then
/// the `defaults` chain, then falls back to the VM's system-property store.
/// Returns null when the key is unknown.
///
/// We deliberately do NOT call back into `this.get(key)` via
/// `invoke_virtual` because the underlying `Hashtable.get` is exactly
/// the path that's broken in our synthetic Properties model — that's
/// why this side-table exists.  Re-entering it would either NPE
/// (Hashtable's internal table-array is null) or NoSuchMethodError
/// (the synthetic Properties' class hierarchy doesn't expose
/// `Object.get`).  Both have been observed during KC26 boot.
///
/// The `defaults` fallback, by contrast, recurses through the *defaults*
/// Properties' own `getProperty` (`invoke_virtual`), so it naturally walks a
/// multi-level `new Properties(parentDefaults)` chain and bottoms out at a
/// Properties with no defaults. This matches `java.util.Properties.getProperty`
/// (JDK: `(sval == null && defaults != null) ? defaults.getProperty(key) : sval`)
/// and is checked BEFORE the system-property fallback so a Properties' own
/// defaults win over any same-named system property.
/// Read one key out of the receiver's REAL `java.util.Properties` backing --
/// the private `map` `ConcurrentHashMap` the JDK bytecode itself uses.
///
/// The side-table is not the only place an entry can live. Anything that
/// reached the object through real bytecode (a `Properties` SUBCLASS's own
/// `put`, `Hashtable` methods we do not override) lands only in that map, as
/// does a `load` whose side-table insert was refused at capacity. Returns the
/// value's `toString` form, or `None` when there is no backing map / no entry.
fn chm_get(ctx: &mut dyn NativeContext, this: ObjectRef, key_obj: ObjectRef) -> Option<String> {
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let chm_pin = ctx.pin_native_root(chm);
    let key_pin = ctx.pin_native_root(key_obj);
    let chm_cur = ctx.read_native_pin(chm_pin, chm);
    let key_cur = ctx.read_native_pin(key_pin, key_obj);
    let got = ctx.invoke_virtual(
        chm_cur,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(key_cur))],
    );
    let value = match got {
        Ok(Some(Value::Object(Some(v)))) => {
            let v_pin = ctx.pin_native_root(v);
            let v_cur = ctx.read_native_pin(v_pin, v);
            ctx.read_string(v_cur)
        }
        _ => None,
    };
    // `chm_pin` is the base of every pin taken here, so this releases them all.
    ctx.unpin_native_roots(chm_pin);
    value
}

fn native_properties_get_property_1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if let Some(v) = get_kv(ctx, this, &key) {
        tracing::debug!(
            target: "cratonvm_vm::props_sidetable",
            ?this, key = %key, bytes = v.len(),
            "PROPS-GET sidetable hit"
        );
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    // Then the object's REAL backing map, BEFORE the `defaults` chain -- the
    // same order the JDK uses (own entries, then defaults). Reading only the
    // side-table made every entry that reached the object through real
    // bytecode invisible. On the Keycloak 26.6.1 boot the side-table was
    // already at its object cap when Infinispan loaded
    // `META-INF/infinispan-version.properties`, so
    // `getProperty("infinispan.version", "0.0.0-SNAPSHOT")` answered the
    // DEFAULT -- and Infinispan then rejected its own
    // `urn:infinispan:config:16.0` namespace as unparseable.
    let pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key_obj);
    let this_cur = ctx.read_native_pin(pin, this);
    let key_cur = ctx.read_native_pin(key_pin, key_obj);
    let backing = chm_get(ctx, this_cur, key_cur);
    let this = ctx.read_native_pin(pin, this);
    let key_obj = ctx.read_native_pin(key_pin, key_obj);
    ctx.unpin_native_roots(pin);
    if let Some(v) = backing {
        tracing::debug!(
            target: "cratonvm_vm::props_sidetable",
            ?this, key = %key, bytes = v.len(),
            "PROPS-GET real-backing hit"
        );
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    // Fall through to the `defaults` chain (recursively, via the defaults
    // Properties' own getProperty). The receiver is no longer used after this,
    // and the VM roots `defs`/`key_obj` for the duration of the re-entrant call.
    if let Some(defs) = props_defaults(ctx, this) {
        let dv = ctx.invoke_virtual(
            defs,
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(key_obj))],
        )?;
        if let Some(Value::Object(Some(s))) = dv {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    // System-property fallback ONLY for the `System.getProperties()` view.
    // Real `java.util.Properties.getProperty` consults nothing but the object's
    // own entries and its `defaults` chain — it must NOT leak system properties.
    // A plain `new Properties()` that misses here returns null. Without this
    // gate, `new Properties().getProperty("user.dir")` returned the system value,
    // which (e.g.) made `PropertyPlaceholderConfigurer` with
    // SYSTEM_PROPERTIES_MODE_NEVER resolve `${user.dir}` instead of failing
    // (PropertyResourceConfigurerTests.propertyPlaceholderConfigurerWith
    // UnresolvableSystemProperty). The synthetic `System.getProperties()` object
    // is marked via `mark_system_props`, so it still resolves system keys.
    if !is_system_props(ctx, this) {
        if key == "jboss.home.dir" {
            if let Some(v) = ctx
                .get_system_property(&key)
                .or_else(|| super::system_property_fallback(ctx, &key))
            {
                return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
            }
        }
        tracing::debug!(
            target: "cratonvm_vm::props_sidetable",
            ?this, key = %key,
            "PROPS-GET sidetable MISS, non-system Properties -> null"
        );
        return Ok(Some(Value::Object(None)));
    }
    tracing::debug!(
        target: "cratonvm_vm::props_sidetable",
        ?this, key = %key,
        "PROPS-GET sidetable MISS, falling back to system"
    );
    match ctx
        .get_system_property(&key)
        .or_else(|| super::system_property_fallback(ctx, &key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.getProperty(String, String)` — `getProperty(key)` (which
/// includes the side-table, the `defaults` chain, and the system-property
/// fallback) and, only if that is null, the supplied default. Mirrors the JDK:
/// `String val = getProperty(key); return (val == null) ? defaultValue : val;`.
fn native_properties_get_property_2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    // Reuse the 1-arg path verbatim (it owns the side-table → defaults → system
    // lookup order), substituting the caller's default on a null result.
    let nargs = 2.min(args.len());
    match native_properties_get_property_1(ctx, &args[..nargs])? {
        Some(Value::Object(Some(s))) => Ok(Some(Value::Object(Some(s)))),
        _ => Ok(Some(default)),
    }
}

/// Native `Properties.setProperty(String, String)` — stores the
/// key/value pair both in the side-table (so subsequent `getProperty`
/// finds it) and in the VM's system-property store (matching the
/// historical behaviour of the previous override that mirrored to
/// `System.setProperty`).
fn native_properties_set_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val_obj = match args.get(2) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    let val = ctx.read_string(val_obj).unwrap_or_default();
    let old = get_kv(ctx, this, &key);
    put_kv(ctx, this, &key, &val);
    // Mirror into the real JDK Properties backing (`map` ConcurrentHashMap) so
    // generic Map walkers observe the entry — see native_properties_put's
    // fn-level note for the full rationale (Hibernate's PU-properties merge).
    mirror_loaded_entries_to_properties_backend(ctx, this, &[(key.clone(), val.clone())]);
    // Only the system-properties view propagates to the global store — see
    // `system_props_keys` for why a blanket mirror cross-contaminates.
    if is_system_props(ctx, this) {
        let _ = ctx.set_system_property(&key, &val);
    }
    match old {
        Some(prev) => Ok(Some(Value::Object(Some(ctx.create_string(&prev))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.put(Object, Object)` — when callers bypass
/// `setProperty` and call `Hashtable.put` directly, store the write in
/// the side-table so `getProperty`/`get` still find it.  This native
/// fully overrides the method, so the real `Properties.put` bytecode
/// does NOT run; we therefore also mirror the entry into the real JDK
/// Properties backing (the JDK-17+ `map` ConcurrentHashMap field) via
/// `mirror_loaded_entries_to_properties_backend`.  Without that mirror,
/// generic Map walkers that read the backing rather than the side-table
/// — `HashMap.putAll(props)`, `new HashMap<>(props)`, and the
/// `map_collect_entries`/`properties_backing_chm` helpers in
/// `native-collections` — see an empty map and copy nothing.  Hibernate's
/// `EntityManagerFactoryBuilderImpl$MergedSettings` merges every
/// persistence-unit property through exactly `configValues.putAll(
/// persistenceUnit.getProperties())`, so the unmirrored side-table left
/// the JDBC URL/dialect invisible and JPA bootstrap failed to find a
/// Dialect.  `load(InputStream)` already mirrors the same way.
///
/// Also mirror to the VM's system-property store, matching what
/// `Properties.setProperty` does. The JDK-semantic behaviour: real
/// `System.getProperties()` returns the `System.props` singleton, so
/// any `put(k,v)` on it is immediately visible via
/// `System.getProperty(k)`. CratonVM's `System.getProperties()` (see
/// `lib.rs` essential registration) returns a FRESH snapshot
/// Properties each call — so receiver-identity against `System.props`
/// would always fail, and constraining the mirror to such a check
/// would silently break the pattern.
///
/// Mirror unconditionally. The cost is that a `Properties` instance
/// used as a plain map pollutes the VM system-property store with its
/// (String,String) entries, which is harmless to readers that query
/// specific keys.
///
/// Reproducer: BC's `ASN1IntegerTest.testLooseValidEncoding_*` flips
/// `false → true` via `System.getProperties().put(...)` and then
/// `ASN1Integer`'s `Properties.isOverrideSet(...)` reads back stale
/// `false`, so the loose-validation bypass never engages and
/// "malformed integer" throws on inputs the test expects to accept.
fn native_properties_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let (Value::Object(Some(k)), Value::Object(Some(v))) = (key_v, val_v) else {
        return Ok(Some(Value::Object(None)));
    };
    // `Properties.put(Object,Object)` is inherited from `Hashtable` and accepts
    // ANY key/value (only `setProperty`/`getProperty` are String-typed). The
    // String→String fast path lives in the Rust side-table; everything else
    // (non-String value, OR non-String key — e.g. Spring's `<props>` parsed
    // into `TypedStringValue` keys, or an empty-String key) must be stored in
    // the real JDK CHM so size()/entrySet()/get() still observe it. Dropping a
    // non-String key here was the `mergeProperties` bug: a `ManagedProperties`
    // populated with `TypedStringValue` keys silently came back empty.
    let ks_opt = ctx.read_string(k);
    let vs_opt = ctx.read_string(v);
    if let (Some(ks), Some(vs)) = (ks_opt.as_ref(), vs_opt.as_ref()) {
        if !ks.is_empty() {
            // String→String: store in side-table AND CHM (existing path).
            let prev = get_kv(ctx, this, ks);
            put_kv(ctx, this, ks, vs);
            mirror_loaded_entries_to_properties_backend(ctx, this, &[(ks.clone(), vs.clone())]);
            if is_system_props(ctx, this) {
                let _ = ctx.set_system_property(ks, vs);
            }
            return Ok(Some(match prev {
                Some(p) => Value::Object(Some(ctx.create_string(&p))),
                None => Value::Object(None),
            }));
        }
    }
    // Non-String key and/or value (or an empty-String key): store directly in
    // the real JDK CHM so get()/size()/entrySet() retrieve the actual objects.
    // The Rust side-table is string-only — do not put a synthetic "" sentinel
    // that would shadow the real object.
    let prev = put_non_string_into_chm(ctx, this, key_v, val_v);
    Ok(Some(prev))
}

/// Put a non-String (key,value) pair directly into the Properties' real JDK
/// ConcurrentHashMap backing store (`map` field).  Returns the previous value.
fn put_non_string_into_chm(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_v: Value,
    val_v: Value,
) -> Value {
    // Ensure the CHM exists (create it if Properties was freshly allocated).
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        _ => {
            let Ok(Some(Value::Object(Some(m)))) =
                ctx.new_object("java/util/concurrent/ConcurrentHashMap")
            else {
                return Value::Object(None);
            };
            let _ = ctx.invoke(
                "java/util/concurrent/ConcurrentHashMap",
                "<init>",
                "()V",
                &[Value::Object(Some(m))],
            );
            ctx.set_field_by_name(this, "map", Value::Object(Some(m)));
            m
        }
    };
    let prev = ctx
        .invoke_virtual(
            chm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[key_v, val_v],
        )
        .ok()
        .flatten()
        .unwrap_or(Value::Object(None));
    prev
}

/// Native `Properties.putIfAbsent(Object, Object)Object` — `Map.putIfAbsent`
/// semantics: if `this` already maps `key` to a non-null value, return that
/// value unchanged; otherwise store `value` (via [`native_properties_put`],
/// so the side-table/CHM mirroring stays identical to a plain `put`) and
/// return `null`.
///
/// Existence is checked through the same side-table → CHM → system-property
/// read path [`native_properties_get`] already uses, so `putIfAbsent` agrees
/// with `get`/`getProperty`/`containsKey` about what's "present" instead of
/// consulting the real (and, for our synthetic Properties, often-broken)
/// `Hashtable`/`map` fields directly the way the uninterospected inherited
/// bytecode did.
fn native_properties_put_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if args.first().is_none() {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(Value::Object(Some(existing))) = native_properties_get(ctx, args)? {
        return Ok(Some(Value::Object(Some(existing))));
    }
    native_properties_put(ctx, args)?;
    Ok(Some(Value::Object(None)))
}

/// Native `Properties.computeIfAbsent(Object, Function)Object` with the same
/// side-table-aware storage semantics as `put` and `get`.
fn native_properties_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let function = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };

    // `get`, `Function.apply`, and `put` can all re-enter Java and collect.
    // Keep every live reference rooted, then refresh it before each use.
    let roots_base = ctx.pin_native_root(this);
    let key_pin = match key {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let function_pin = ctx.pin_native_root(function);
    let read_key = |ctx: &mut dyn NativeContext| match key_pin {
        Some((pin, original)) => Value::Object(Some(ctx.read_native_pin(pin, original))),
        None => key,
    };

    let this_cur = ctx.read_native_pin(roots_base, this);
    let key_cur = read_key(ctx);
    let existing = native_properties_get(ctx, &[Value::Object(Some(this_cur)), key_cur])?;
    if matches!(existing, Some(Value::Object(Some(_)))) {
        ctx.unpin_native_roots(roots_base);
        return Ok(existing);
    }

    let function_cur = ctx.read_native_pin(function_pin, function);
    let key_cur = read_key(ctx);
    let computed = ctx.invoke_virtual(
        function_cur,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key_cur],
    )?;
    let value = computed.unwrap_or(Value::Object(None));
    if matches!(value, Value::Object(None)) {
        ctx.unpin_native_roots(roots_base);
        return Ok(Some(Value::Object(None)));
    }
    let value_pin = match value {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let this_cur = ctx.read_native_pin(roots_base, this);
    let key_cur = read_key(ctx);
    let value_cur = match value_pin {
        Some((pin, original)) => Value::Object(Some(ctx.read_native_pin(pin, original))),
        None => value,
    };
    native_properties_put(ctx, &[Value::Object(Some(this_cur)), key_cur, value_cur])?;
    let result = match value_pin {
        Some((pin, original)) => Value::Object(Some(ctx.read_native_pin(pin, original))),
        None => value,
    };
    ctx.unpin_native_roots(roots_base);
    Ok(Some(result))
}

/// Native `Properties.remove(Object) Object` — symmetric with `put` /
/// `setProperty`.  JDK 25's `Properties.remove` (Properties.java:1348)
/// delegates to `map.remove(key)` where `map` is the private
/// `ConcurrentHashMap` field; our synthetic Properties has a null `map`,
/// so the JDK bytecode NPEs.  Route the remove through the side-table
/// (where `put`/`setProperty` actually stored the entry) and return the
/// previous value to honour the Map.remove contract.
///
/// Callers in the wild that need this semantics include H2's
/// `org.h2.engine.ConnectionInfo` (`removeProperty("USER", "")` strips
/// the JDBC USER setting before the engine iterates connection keys —
/// every H2 JDBC connect failed without it) and ModuleBootstrap's
/// `getAndRemoveProperty` (which previously got the same null-return
/// behaviour via the no-op stub this replaces — empty side-table for
/// jdk.module.* keys keeps that path unchanged).
fn native_properties_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    if key.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let removed = remove_kv(ctx, this, &key);
    // The system-properties view must propagate removal to the global store,
    // mirroring how `setProperty`/`put` propagate writes — otherwise
    // `System.getProperties().remove(k)` (keycloak ExportImportConfig.reset)
    // leaves `System.getProperty(k)` returning the stale value.
    if is_system_props(ctx, this) {
        let _ = ctx.remove_system_property(&key);
    }
    // Keep the real JDK `map` CHM backing in sync with the side-table: `put`/
    // `setProperty` mirror INTO it, so a `remove` that touched only the
    // side-table would let generic Map walkers (HashMap.putAll /
    // map_collect_entries, which read the CHM) resurrect the removed key.
    // No-op when the Properties has no CHM yet. Non-String values (arrays,
    // etc. — see `put_non_string_into_chm`) live ONLY here, never in the
    // String-only side-table, so this is the sole source of truth for them:
    // capture what it actually removed instead of discarding it.
    let chm_removed = remove_from_properties_backend(ctx, this, key_obj);
    match removed {
        Some(prev) => Ok(Some(Value::Object(Some(ctx.create_string(&prev))))),
        None => Ok(Some(chm_removed)),
    }
}

/// Remove `key_obj` from a Properties object's real JDK `map` ConcurrentHashMap
/// backing, mirroring a side-table removal, and return whatever the CHM had
/// stored under that key (or `Object(None)` if there was no CHM yet, or no
/// entry). Symmetric with `mirror_loaded_entries_to_properties_backend`.
/// Callers that already have a side-table hit ignore this value (the
/// side-table and CHM are mirrored for String entries, so either source
/// agrees); it matters only for non-String values that the side-table can't
/// represent at all (see `native_properties_remove`).
fn remove_from_properties_backend(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_obj: ObjectRef,
) -> Value {
    if let Value::Object(Some(chm)) = ctx.get_field_by_name(this, "map") {
        ctx.invoke_virtual(
            chm,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key_obj))],
        )
        .ok()
        .flatten()
        .unwrap_or(Value::Object(None))
    } else {
        Value::Object(None)
    }
}

/// Native `Properties.clear()V` — empties BOTH the side-table and the real
/// `map` CHM backing.  Without this override `clear()` ran the real bytecode
/// that empties only the CHM, leaving the side-table (which `getProperty`/
/// `get`/`size` read) stale — the same side-table-vs-backing asymmetry that
/// `remove` would otherwise have. Keeps every read surface consistent.
fn native_properties_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    table().lock().remove(&key_for(ctx, this));
    if let Value::Object(Some(chm)) = ctx.get_field_by_name(this, "map") {
        let _ = ctx.invoke_virtual(chm, "clear", "()V", &[]);
    }
    if is_system_props(ctx, this) {
        let old_keys: Vec<String> = ctx
            .list_system_properties()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        for key in old_keys {
            let _ = ctx.remove_system_property(&key);
        }
    }
    Ok(None)
}

/// Native `Properties.containsKey(Object)` — consults the side-table.
/// Symmetric with `getProperty`.
fn native_properties_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    if get_kv(ctx, this, &key).is_some() {
        return Ok(Some(Value::Int(1)));
    }
    // Non-String-valued entries live only in the CHM backing — consult it so
    // `containsKey` agrees with `get` (which already falls through to the CHM).
    if let Value::Object(Some(chm)) = ctx.get_field_by_name(this, "map") {
        if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(
            chm,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(key_obj))],
        ) {
            if n != 0 {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

/// Native `Properties.get(Object)Object` — Hashtable-style read path used
/// by callers that bypass `getProperty` (e.g. Spring's
/// `PropertySourcesPropertyResolver` calling `Properties.get(key)` on the
/// `MapPropertySource` backed by `System.getProperties()`, or H2 reading JDBC
/// connection credentials from an ordinary `Properties`).
///
/// JDK 25's `Properties.get` (Properties.java:1338) reads from a private
/// `ConcurrentHashMap<Object, Object> map` field that's only populated by
/// `Properties.<init>`'s body.  Our synthetic Properties allocations don't
/// run that body, so `map` is null and the bytecode NPEs.  Override the
/// method here to consult the side-table (and fall back to system
/// properties only for the System.getProperties() case), mirroring how
/// `getProperty` already routes around the broken bytecode path.
///
/// Returns `null` when the key is absent — matches `Hashtable.get`
/// semantics.
fn native_properties_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    // Check the Rust side-table (String→String only).
    if let Some(v) = get_kv(ctx, this, &key) {
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    // Non-String values (e.g. XProperty, MemberDetails) are stored only in
    // the real JDK CHM backing (`map` field).  Check it before falling through
    // to system properties so the caller receives the actual object.
    if let Value::Object(Some(chm)) = ctx.get_field_by_name(this, "map") {
        if let Ok(Some(v)) = ctx.invoke_virtual(
            chm,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key_obj))],
        ) {
            if v != Value::Object(None) {
                return Ok(Some(v));
            }
        }
    }
    // Match real `Properties`: ordinary instances do not consult global system
    // properties. The system-property fallback is only for the synthetic,
    // marked singleton returned by `System.getProperties()`. Without this gate,
    // `new Properties().get("password")` could return a global system property
    // and pollute H2/Hibernate credential lookups that use the Map-style API.
    if !is_system_props(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| super::system_property_fallback(ctx, &key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.size()I` — Hashtable-style count used by Spring's
/// `SpringConfigurationPropertySource.isFullEnumerable`, which probes
/// the underlying source via `Map.size()`.  JDK 25's `Properties.size`
/// (Properties.java:1302) reads the private
/// `ConcurrentHashMap<Object,Object> map` field that's null on our
/// synthetic Properties — the bytecode NPEs.  Route the read through
/// the side-table; objects we never wrote to report 0.
fn native_properties_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Side-table entries (String values) PLUS the CHM-exclusive entries
    // (non-String values such as a `Class` deserializer) — see
    // `chm_extra_entries`. Without the merge, a Properties whose values are
    // non-String (e.g. Kafka's ConsumerConfig key.deserializer) reports a
    // short size and breaks Map-size-driven enumeration.
    let side = side_key_set(ctx, this);
    let total = side.len() + chm_extra_entries(ctx, this, &side).len();
    Ok(Some(Value::Int(total as i32)))
}

/// Native `Properties.isEmpty()Z` — symmetric companion to `size()`.
fn native_properties_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    // Fast path: any String entry in the side-table means non-empty without
    // touching the CHM. Only when the side-table is empty do we pay for the
    // CHM scan to detect non-String-valued entries (which live only there).
    if count_kv(ctx, this) > 0 {
        return Ok(Some(Value::Int(0)));
    }
    let empty = chm_extra_entries(ctx, this, &std::collections::HashSet::new()).is_empty();
    Ok(Some(Value::Int(if empty { 1 } else { 0 })))
}

/// Build a real `java.util.Collection` of `class_name` (e.g.
/// `java/util/HashSet` or `java/util/ArrayList`) populated with `items` via the
/// collection's public `add(Object)`.  A real-JDK collection iterates and
/// `toArray()`s correctly, so copy-constructor consumers — `new TreeSet<>(props
/// .keySet())`, `new ArrayList<>(props.values())`, `Collections.list(...)` —
/// observe the entries.  The earlier raw-field path (`make_hashset_with_elements`
/// / a hand-built `ArrayList`) reported the right `size()`/iterator but its
/// `toArray()`/`addAll`-source view mis-aligned and silently yielded 0 (the same
/// defect `entrySet()` was already switched to real `new HashSet()` to dodge).
/// The fresh collection is pinned across the re-entrant `create_string`/`add`
/// calls so a moving GC cannot strand it.
fn build_string_collection(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    items: Vec<String>,
) -> ObjectRef {
    let coll = match ctx.new_object(class_name) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return crate::alloc_concurrent_synthetic(ctx, class_name, 2),
    };
    let pin = ctx.pin_native_root(coll);
    let _ = ctx.invoke(class_name, "<init>", "()V", &[Value::Object(Some(coll))]);
    for s in &items {
        let so = ctx.create_string(s);
        let coll = ctx.read_native_pin(pin, coll);
        let _ = ctx.invoke_virtual(
            coll,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(so))],
        );
    }
    let coll = ctx.read_native_pin(pin, coll);
    ctx.unpin_native_roots(pin);
    coll
}

/// Build a real `HashSet<String>` populated with the side-table keys for the
/// given Properties object.  Returns an empty HashSet if the object isn't
/// tracked.
fn build_key_set(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> ObjectRef {
    let keys: Vec<String> = ordered_snapshot_kv(ctx, this)
        .into_iter()
        .map(|(k, _v)| k)
        .collect();
    // `java/util/LinkedHashSet`, not the far more common `java/util/HashSet`:
    // this snapshot needs `retainAll`/`remove` to propagate back to the
    // source `Properties` (see `tag_properties_keyset_source` below, for the
    // common `props.keySet().retainAll(baseline)` test-cleanup idiom), which
    // requires a native override on the snapshot's exact class. Scoping that
    // override to `LinkedHashSet` instead of `HashSet` keeps the blast radius
    // to "objects this function creates" — every other `new HashSet<>()` in
    // the entire process (a MUCH more common class) is completely
    // unaffected. `LinkedHashSet extends HashSet`, so `instanceof HashSet`
    // and the `Set` contract are unchanged for callers.
    build_string_collection(ctx, "java/util/LinkedHashSet", keys)
}

/// Side table linking a `Properties.keySet()` snapshot `Set` (by identity
/// hash) back to the source `Properties` object it was built from. Read by
/// the `LinkedHashSet.retainAll`/`remove` overrides below so mutating the
/// snapshot also mutates the real, side-table-backed source — otherwise
/// `properties.keySet().retainAll(...)`/`.remove(...)` silently no-ops on
/// the disconnected snapshot alone (`build_key_set` above is a snapshot,
/// not a live view).
fn properties_keyset_source_table() -> &'static Mutex<FxHashMap<i32, usize>> {
    static T: OnceLock<Mutex<FxHashMap<i32, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn tag_properties_keyset_source(ctx: &mut dyn NativeContext, set: ObjectRef, source: ObjectRef) {
    let set_pin = ctx.pin_native_root(set);
    let handle = ctx.add_global_root(source);
    let set = ctx.read_native_pin(set_pin, set);
    let key = ctx.identity_hash_code(set);
    let mut table = properties_keyset_source_table().lock();
    // Retention cap: each entry roots one snapshot `Set` (and the source
    // `Properties`, usually already permanently alive) forever. Bound the
    // leak rather than grow unboundedly — mirrors the sibling cap in
    // `logmanager.rs`'s `log_record_messages`.
    if table.len() > 4096 {
        if let Some(&oldest) = table.keys().next() {
            if let Some(h) = table.remove(&oldest) {
                ctx.remove_global_root(h);
            }
        }
    }
    table.insert(key, handle);
    ctx.unpin_native_roots(set_pin);
}

fn properties_keyset_source(ctx: &mut dyn NativeContext, set: ObjectRef) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(set);
    let handle = *properties_keyset_source_table().lock().get(&key)?;
    ctx.resolve_global_root(handle)
}

/// Native `LinkedHashSet.retainAll(Collection)Z`, gated on
/// `properties_keyset_source`. For an ordinary `LinkedHashSet` (anything
/// not built by `build_key_set`) this is a plain pass-through to real
/// bytecode (`invoke_virtual_bytecode_only`, which skips re-entering this
/// same native — its `args` contract is PARAMS ONLY, receiver excluded,
/// hence `&args[1..]` below). For a `Properties.keySet()` snapshot, first
/// remove every currently side-table-tracked key NOT present in the
/// retain collection from the SOURCE `Properties` (whose own `remove` is a
/// real, working mutation), then let real bytecode finish the snapshot's
/// own (already-correct) in-memory `retainAll`.
fn native_linkedhashset_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(Value::Object(Some(retain_coll))) = args.get(1).copied() else {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            &args[1..],
        );
    };
    let Some(source) = properties_keyset_source(ctx, this) else {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            &args[1..],
        );
    };
    let keys = side_key_set(ctx, source);
    let this_pin = ctx.pin_native_root(this);
    let source_pin = ctx.pin_native_root(source);
    let retain_pin = ctx.pin_native_root(retain_coll);
    for key in keys {
        let key_obj = ctx.create_string(&key);
        let retain_coll = ctx.read_native_pin(retain_pin, retain_coll);
        let contained = matches!(
            ctx.invoke_virtual(
                retain_coll,
                "contains",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(key_obj))]
            ),
            Ok(Some(Value::Int(1)))
        );
        if !contained {
            let source = ctx.read_native_pin(source_pin, source);
            let key_obj = ctx.create_string(&key);
            let _ = ctx.invoke_virtual(
                source,
                "remove",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key_obj))],
            );
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    let retain_coll = ctx.read_native_pin(retain_pin, retain_coll);
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(source_pin);
    ctx.unpin_native_roots(retain_pin);
    ctx.invoke_virtual_bytecode_only(
        this,
        "retainAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(retain_coll))],
    )
}

/// Native `LinkedHashSet.remove(Object)Z` — same
/// disconnected-snapshot-vs-source-`Properties` gate as
/// `native_linkedhashset_retain_all`, for the single-key removal case
/// (`properties.keySet().remove(key)`).
fn native_linkedhashset_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(source) = properties_keyset_source(ctx, this) else {
        // An ordinary LinkedHashSet, not a `Properties.keySet()` snapshot.
        // It still must NOT go to real bytecode: `HashSet.remove` is
        // `return map.remove(o) == PRESENT;` and this VM's synthetic backing
        // map stores an `Int(1)` sentinel, never JDK `HashSet.PRESENT`, so the
        // identity comparison is always false — the element was removed but
        // `remove()` answered `false`. (`LinkedHashSet` inherits `remove`, so
        // this override is the only registration that sees such a call.)
        //
        // javac was the loudest victim: `Annotate.attributeAnnotation` puts an
        // annotation type's elements in a `LinkedHashSet` and reports
        // "duplicate element 'value' in annotation @X" when `members.remove`
        // returns false — making EVERY annotation with a `value` element
        // uncompilable by the in-process compiler that Spring's AOT
        // `TestCompiler` uses.
        if let Some(result) = cratonvm_native_collections::try_native_hashset_remove(ctx, args) {
            return result;
        }
        return ctx.invoke_virtual_bytecode_only(this, "remove", "(Ljava/lang/Object;)Z", &args[1..]);
    };
    if let Some(Value::Object(Some(elem))) = args.get(1).copied() {
        let this_pin = ctx.pin_native_root(this);
        let source_pin = ctx.pin_native_root(source);
        let elem_pin = ctx.pin_native_root(elem);
        let elem = ctx.read_native_pin(elem_pin, elem);
        let source = ctx.read_native_pin(source_pin, source);
        let _ = ctx.invoke_virtual(
            source,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(elem))],
        );
        let this = ctx.read_native_pin(this_pin, this);
        let elem = ctx.read_native_pin(elem_pin, elem);
        ctx.unpin_native_roots(this_pin);
        ctx.unpin_native_roots(source_pin);
        ctx.unpin_native_roots(elem_pin);
        // Same reason as the non-snapshot branch above: real `HashSet.remove`
        // bytecode compares the backing map's value against JDK `PRESENT`,
        // which this VM's synthetic map never stores.
        let fwd = [Value::Object(Some(this)), Value::Object(Some(elem))];
        if let Some(result) = cratonvm_native_collections::try_native_hashset_remove(ctx, &fwd) {
            return result;
        }
        return ctx.invoke_virtual_bytecode_only(
            this,
            "remove",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(elem))],
        );
    }
    if let Some(result) = cratonvm_native_collections::try_native_hashset_remove(ctx, args) {
        return result;
    }
    ctx.invoke_virtual_bytecode_only(this, "remove", "(Ljava/lang/Object;)Z", &args[1..])
}

/// Build a real `java.util.Enumeration` over `items` by populating a real
/// `java/util/Vector` (via its public `add`) and returning its `elements()`.
/// Used by `keys()` / `elements()`.  The previous implementation returned an
/// always-empty `Collections$EmptyEnumeration` regardless of contents, so
/// `Properties.keys()` / `elements()` / `propertyNames()` (which delegates to
/// `keys()`) and any `Collections.list(props.keys())` silently saw nothing.
/// Building a real Vector keeps the returned Enumeration walkable by real-JDK
/// callers (unlike the raw-field synthetic collections, whose `toArray`/copy
/// paths mis-aligned — see `entrySet`).  `this` is pinned across the
/// re-entrant `create_string`/`add` calls so a moving GC cannot leave a stale
/// `vec`.
fn build_enumeration(ctx: &mut dyn NativeContext, items: Vec<String>) -> ObjectRef {
    let empty = |ctx: &mut dyn NativeContext| {
        crate::alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)
    };
    let vec = match ctx.new_object("java/util/Vector") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return empty(ctx),
    };
    let pin = ctx.pin_native_root(vec);
    if ctx
        .invoke(
            "java/util/Vector",
            "<init>",
            "()V",
            &[Value::Object(Some(vec))],
        )
        .is_err()
    {
        ctx.unpin_native_roots(pin);
        return empty(ctx);
    }
    for s in &items {
        let so = ctx.create_string(s);
        let vec = ctx.read_native_pin(pin, vec);
        let _ = ctx.invoke_virtual(
            vec,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(so))],
        );
    }
    let vec = ctx.read_native_pin(pin, vec);
    let result = match ctx.invoke_virtual(vec, "elements", "()Ljava/util/Enumeration;", &[]) {
        Ok(Some(Value::Object(Some(e)))) => e,
        _ => empty(ctx),
    };
    ctx.unpin_native_roots(pin);
    result
}

/// Native `Properties.stringPropertyNames()Ljava/util/Set;` — Surefire
/// `SystemPropertyManager.loadProperties` copies loaded entries into a
/// `ConcurrentHashMap` via `p.stringPropertyNames()` then `p.getProperty(key)`.
/// JDK bytecode walks `entrySet()` on the internal CHM `map` field, but our
/// `Properties.<init>` native skips populating that CHM, so the bytecode
/// would yield an empty set even when `Properties.load` succeeded. Return
/// the side-table keys directly so the fork sees the loaded properties.
fn native_properties_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let mut this = this;
    let set = build_key_set(ctx, &mut this);
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.keySet()Ljava/util/Set;` — returns a synthetic
/// HashSet populated from the side-table.  Spring's
/// `SpringIterableConfigurationPropertySource` walks this once it
/// recognises the source as enumerable.
fn native_properties_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let mut this = this;
    let mut set = build_key_set(ctx, &mut this);
    let set_pin = ctx.pin_native_root(set);
    // Add keys for CHM-exclusive (non-String-valued) entries so the key view
    // matches the real map; `stringPropertyNames()` deliberately does NOT do
    // this (it is specified to return only String-keyed/String-valued names).
    let side = side_key_set(ctx, this);
    let extra = chm_extra_entries(ctx, this, &side);
    let extra_key_pins: Vec<(usize, ObjectRef)> = extra
        .iter()
        .map(|(key_obj, _value, _kstr)| (ctx.pin_native_root(*key_obj), *key_obj))
        .collect();
    for ((key_obj, _value, kstr), (key_pin, key_fallback)) in
        extra.iter().zip(extra_key_pins.iter())
    {
        let key_value = if let Some(s) = kstr {
            // String keys are common for Properties; rebuild a fresh Java
            // String after the CHM walk so the key cannot be a stale raw ref
            // from a previous iterator call.
            let fresh = ctx.create_string(s);
            let fresh_pin = ctx.pin_native_root(fresh);
            let fresh = ctx.read_native_pin(fresh_pin, fresh);
            let value = Value::Object(Some(fresh));
            set = ctx.read_native_pin(set_pin, set);
            let _ = ctx.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &[value]);
            ctx.unpin_native_roots(fresh_pin);
            continue;
        } else {
            let _ = key_obj;
            Value::Object(Some(ctx.read_native_pin(*key_pin, *key_fallback)))
        };
        set = ctx.read_native_pin(set_pin, set);
        let _ = ctx.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &[key_value]);
    }
    for (pin, _fallback) in extra_key_pins {
        ctx.unpin_native_roots(pin);
    }
    set = ctx.read_native_pin(set_pin, set);
    // Tag the snapshot so `LinkedHashSet.retainAll`/`remove` can propagate
    // mutations back to `this` (the source `Properties`) — see
    // `tag_properties_keyset_source`'s doc comment.
    tag_properties_keyset_source(ctx, set, this);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.values()Ljava/util/Collection;` — returns a **live**
/// view of the side-table backed by this Properties object, mirroring
/// `entrySet()`/the `LinkedHashSet`-scoped `keySet()` fix above: the previous
/// implementation built a plain, disconnected `ArrayList` snapshot, so
/// `values().remove(v)` / `.iterator().remove()` / `.clear()` silently never
/// touched the source `Properties` (the same "same applies to entrySet()/
/// values()" gap this doc originally called out for keySet()). The returned
/// list is tagged with the source Properties object in its trailing capacity
/// slot via `make_live_values_list`, reusing the ALREADY-hardened generic
/// `values_view_source`/`propagate_list_removal` machinery that
/// `native_map_values` (regular `HashMap`/`Hashtable`) relies on for the same
/// purpose — no new override on `java/util/ArrayList` itself, so this carries
/// none of the "global HashSet override" blast-radius risk the keySet retry
/// above had to work around.
///
/// Note: `retainAll` on a values()-view list does NOT currently propagate to
/// the source map — that is a pre-existing, Properties-independent gap in the
/// shared `native_al_retain_all` (it never consults `values_view_source` for
/// ANY `Map.values()`, not just Properties'), out of scope for this
/// Properties-specific doc.
fn native_properties_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let list = crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let arr = ctx.new_array(ArrayElementType::Reference, 0);
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(0));
            return Ok(Some(Value::Object(Some(list))));
        }
    };
    let mut this = this;
    let snapshot = ordered_snapshot_kv(ctx, &mut this);
    // cceres5-style GC safety (mirrors `native_properties_entry_set`): every
    // `create_string` below can trigger a moving GC that relocates `this` and
    // strings already accumulated in `vals`. Pin everything and refresh
    // through the pins immediately before building the live list.
    let this_pin = ctx.pin_native_root(this);
    let mut vals: Vec<Value> = Vec::with_capacity(snapshot.len());
    let mut val_pins: Vec<usize> = Vec::with_capacity(snapshot.len());
    for (_k, v) in &snapshot {
        let vs = ctx.create_string(v);
        let vs_pin = ctx.pin_native_root(vs);
        vals.push(Value::Object(Some(vs)));
        val_pins.push(vs_pin);
    }
    // Append CHM-exclusive (non-String) values so the value view matches the
    // real map. Skip-set is the side-table keys, so mirrored String values are
    // not duplicated.
    let side = side_key_set(ctx, this);
    let this_cur = ctx.read_native_pin(this_pin, this);
    for (_key_obj, value, _kstr) in chm_extra_entries(ctx, this_cur, &side) {
        let v_pin = match value {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        vals.push(value);
        val_pins.push(v_pin);
    }
    // Refresh every accumulated value to its current address.
    for (i, pin) in val_pins.iter().enumerate() {
        vals[i] = match vals[i] {
            Value::Object(Some(o)) if *pin != usize::MAX => {
                Value::Object(Some(ctx.read_native_pin(*pin, o)))
            }
            other => other,
        };
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let list = cratonvm_native_collections::make_live_values_list(ctx, this_cur, &vals);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// Native `Properties.entrySet()Ljava/util/Set;` — returns a **live** view of
/// the side-table backed by this Properties object.  Each element is a 3-field
/// `java/util/Map$Entry` (key, value, sourceMap=this), so `entry.setValue(v)`
/// writes through to the side-table (via the receiver's virtual `put`), and the
/// set's `iterator().remove()` deletes the key from the side-table (via the
/// receiver's virtual `remove`).  This matches the JDK live-entrySet contract
/// that Hibernate's `ConfigurationHelper.resolvePlaceHolders` relies on
/// (`entries.setValue(resolved)` / `entries.remove()` while interpolating
/// `${...}` placeholders).  The previous implementation returned detached
/// `SimpleImmutableEntry` objects whose `setValue` threw
/// `UnsupportedOperationException`.
///
/// Spring's `SpringFactoriesLoader` / binder still iterate the set the same way
/// — the returned `java/util/HashSet` is a synthetic view-backed set (via
/// `make_static_entry_set`), the same shape `HashMap.entrySet()` returns
/// natively, so enumeration is unchanged.
fn native_properties_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let mut this = this;
    let snapshot = ordered_snapshot_kv(ctx, &mut this);
    props_diag_eprintln!(
        "[PROPS-DBG] native_properties_entry_set: {} entries for obj {:?}",
        snapshot.len(),
        this
    );
    if snapshot.is_empty() {
        props_diag_eprintln!(
            "[PROPS-DBG] WARNING: entrySet() called on empty side-table obj {:?}",
            this
        );
    }
    for (k, _v) in snapshot.iter().take(5) {
        props_diag_eprintln!("[PROPS-DBG]   entry key={}", k);
    }
    // Collect (key, value) pairs: the side-table String entries first, then any
    // CHM-exclusive entries (non-String values, e.g. a `Class` deserializer) so
    // consumers that enumerate `entrySet()` — Kafka's `AbstractConfig`, Spring's
    // `SpringFactoriesLoader`, `HashMap.putAll(props)` via the generic Map
    // iterator — observe the full map. Real stored value objects are reused.
    //
    // cceres5 (metrics-registry `(String) entry.getKey()` CCE, live-captured
    // 2026-07-22): every `create_string` below can trigger a moving GC that
    // relocates the strings already accumulated in `pairs` (and
    // `chm_extra_entries` re-enters Java, which can move them all again).
    // Accumulating the raw refs handed `make_static_entry_set` pre-move
    // addresses. Pin every produced ref and refresh the whole vec through the
    // pins immediately before building the set.
    let this_pin = ctx.pin_native_root(this);
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(snapshot.len());
    let mut pair_pins: Vec<(usize, usize)> = Vec::with_capacity(snapshot.len());
    for (k, v) in &snapshot {
        let ks = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks);
        let vs = ctx.create_string(v);
        let vs_pin = ctx.pin_native_root(vs);
        pairs.push((Value::Object(Some(ks)), Value::Object(Some(vs))));
        pair_pins.push((ks_pin, vs_pin));
    }
    let side_keys: std::collections::HashSet<String> =
        snapshot.iter().map(|(k, _v)| k.clone()).collect();
    let this_cur = ctx.read_native_pin(this_pin, this);
    for (key_obj, value, _kstr) in chm_extra_entries(ctx, this_cur, &side_keys) {
        let k_pin = ctx.pin_native_root(key_obj);
        let v_pin = match value {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        pairs.push((Value::Object(Some(key_obj)), value));
        pair_pins.push((k_pin, v_pin));
    }
    // Refresh every accumulated pair to its current address. No allocation may
    // happen between this loop and `make_static_entry_set`'s own entry pins.
    for (i, (k_pin, v_pin)) in pair_pins.iter().enumerate() {
        let (k0, v0) = pairs[i];
        let k = match k0 {
            Value::Object(Some(o)) => Value::Object(Some(ctx.read_native_pin(*k_pin, o))),
            other => other,
        };
        let v = match (v0, *v_pin) {
            (Value::Object(Some(o)), pin) if pin != usize::MAX => {
                Value::Object(Some(ctx.read_native_pin(pin, o)))
            }
            (other, _) => other,
        };
        pairs[i] = (k, v);
    }
    // Build a STATIC entrySet view backed by this Properties object. Each
    // element is a 3-field live `java/util/Map$Entry` (key@0, value@1,
    // sourceMap@2=this): `entry.setValue(v)` writes through to the side-table
    // (`native_entry_set_value` dispatches the backing `put` virtually for a
    // Hashtable/Properties source) and the set's `iterator().remove()` /
    // `remove(entry)` delete the key from the side-table (the view backing's
    // `source_map_remove`). This is what Hibernate's
    // `ConfigurationHelper.resolvePlaceHolders` requires.
    //
    // The view is STATIC (never resynced from the source) on purpose: a normal
    // resyncing entrySet view rebuilds its contents on every `iterator()` /
    // `size()` by walking the source's `entrySet()`, which for a Properties
    // (side-table, not natively-readable buckets) would recurse straight back
    // into this method and collapse to an empty iterator. Tagging the backing —
    // rather than the entries — as the write-through carrier also keeps a later
    // `new HashSet<>(props.entrySet())` copy correctly detached on `remove`.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let set = cratonvm_native_collections::make_static_entry_set(ctx, this_cur, &pairs);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.keys()Ljava/util/Enumeration;` — JDK 17+ wraps
/// `map.keySet()` via `Collections.enumeration`.  We synthesize a real
/// `Enumeration` over the side-table keys.  The previous stub returned an
/// always-empty `Collections$EmptyEnumeration` even for populated
/// Properties, which silently dropped every key for callers that use the
/// legacy `keys()`/`elements()`/`propertyNames()` enumeration API rather
/// than `keySet().iterator()` (e.g. `Collections.list(props.keys())`,
/// `new TreeSet<>(props.keySet())`-style copies).
fn native_properties_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(build_enumeration(
                ctx,
                Vec::new(),
            )))))
        }
    };
    let mut this = this;
    let mut keys: Vec<String> = ordered_snapshot_kv(ctx, &mut this)
        .into_iter()
        .map(|(k, _v)| k)
        .collect();
    // Include String keys of CHM-exclusive (non-String-valued) entries.
    let side = side_key_set(ctx, this);
    for (_key_obj, _value, kstr) in chm_extra_entries(ctx, this, &side) {
        if let Some(s) = kstr {
            keys.push(s);
        }
    }
    Ok(Some(Value::Object(Some(build_enumeration(ctx, keys)))))
}

/// Collect this Properties object's own String keys (side-table + CHM-exclusive
/// non-String-valued entries), de-duplicating into `seen`/`out`. Mirrors the
/// key set `native_properties_keys` exposes for a single object.
fn collect_own_property_names(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) {
    let mut this = this;
    for (k, _v) in ordered_snapshot_kv(ctx, &mut this) {
        if seen.insert(k.clone()) {
            out.push(k);
        }
    }
    let side = side_key_set(ctx, this);
    for (_key_obj, _value, kstr) in chm_extra_entries(ctx, this, &side) {
        if let Some(s) = kstr {
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
}

/// Native `Properties.propertyNames()Ljava/util/Enumeration;` — unlike
/// `keys()` (Hashtable's own keys only), `propertyNames()` MUST also surface
/// the `defaults` chain (JDK `Properties.enumerate`: `if (defaults != null)
/// defaults.enumerate(h)` before the receiver's own entries). The real-JDK
/// bytecode reads the internal `map` CHM via `enumerate`, which our synthetic
/// Properties keeps in the side-table, so without this native a Properties
/// built via `new Properties(defaults)` enumerates only its own keys and drops
/// every inherited default (e.g. Spring's `CollectionUtils.mergePropertiesIntoMap`
/// lost `defaults`-supplied entries). Walk the receiver then recurse through
/// `defaults`, de-duplicating by name. `getProperty` already honours the same
/// chain via `props_defaults`.
fn native_properties_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(build_enumeration(
                ctx,
                Vec::new(),
            )))))
        }
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    // Walk the receiver and its defaults chain. A depth cap guards against a
    // pathological self-referential `defaults` field (the JDK chain is acyclic).
    let mut cur = Some(this);
    let mut depth = 0;
    while let Some(p) = cur {
        if depth > 64 {
            break;
        }
        collect_own_property_names(ctx, p, &mut seen, &mut out);
        cur = props_defaults(ctx, p);
        depth += 1;
    }
    Ok(Some(Value::Object(Some(build_enumeration(ctx, out)))))
}

/// Native `Properties.elements()Ljava/util/Enumeration;` — companion to
/// `keys()`, enumerating the side-table values.
fn native_properties_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vals: Vec<String> = match args.first() {
        Some(Value::Object(Some(o))) => ordered_snapshot_kv(ctx, &mut { *o })
            .into_iter()
            .map(|(_k, v)| v)
            .collect(),
        _ => Vec::new(),
    };
    Ok(Some(Value::Object(Some(build_enumeration(ctx, vals)))))
}

/// Native `Properties.contains(Object)Z` — Hashtable-style value lookup.
/// JDK 25 forwards to `map.contains(value)`.  Returns true iff the
/// side-table holds a string-equal value for any key.
fn native_properties_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val_obj = match args.get(1) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let needle = ctx.read_string(val_obj).unwrap_or_default();
    let snapshot = snapshot_kv(ctx, this);
    let hit = snapshot.iter().any(|(_k, v)| v == &needle);
    if hit {
        return Ok(Some(Value::Int(1)));
    }
    // Non-String values live only in the CHM backing — delegate the value
    // lookup so `contains`/`containsValue` agree with the real map.
    if let Value::Object(Some(chm)) = ctx.get_field_by_name(this, "map") {
        if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(
            chm,
            "containsValue",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(val_obj))],
        ) {
            if n != 0 {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

/// Native `Properties.containsValue(Object)Z` — alias for `contains`.
fn native_properties_contains_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_properties_contains(ctx, args)
}

/// Native `Properties.forEach(BiConsumer)V` — iterates the side-table
/// and invokes `action.accept(key, value)` for each entry.  JDK 25's
/// `Properties.forEach` (Properties.java:1464) forwards directly to
/// `map.forEach(action)`, but our synthetic Properties has a null
/// internal `map` field (see `System.getProperties` allocation), so the
/// real-JDK path NPEs with "Cannot invoke forEach on null".
///
/// log4j-api 2.23 `StatusLogger$PropertiesUtilsDouble.normalizeProperties`
/// calls `properties.forEach(BiConsumer)` once per Properties source
/// (System, env, .properties file) during `StatusLogger$Config.<clinit>`.
/// Without this override, WildFly fails to bootstrap the status logger.
fn native_properties_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let mut this = this;
    let snapshot = ordered_snapshot_kv(ctx, &mut this);
    for (k, v) in &snapshot {
        let ks = ctx.create_string(k);
        let vs = ctx.create_string(v);
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[Value::Object(Some(ks)), Value::Object(Some(vs))],
        )?;
    }
    // CHM-exclusive (non-String-valued) entries, with the real value object.
    let side: std::collections::HashSet<String> =
        snapshot.iter().map(|(k, _v)| k.clone()).collect();
    for (key_obj, value, _kstr) in chm_extra_entries(ctx, this, &side) {
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[Value::Object(Some(key_obj)), value],
        )?;
    }
    Ok(None)
}

/// `Properties.equals(Object)` — content comparison (the `Map.equals` contract).
/// `Properties` inherits `Hashtable.equals` but under CratonVM that resolved to
/// `Object.equals` (identity) for a `Properties` receiver, so two equal-content
/// Properties compared unequal (keycloak `LocaleUtilTest.mergeGroupedMessages`,
/// which prints byte-identical maps yet failed `assertThat(equalTo(...))`).
/// Because our `Properties` data lives in the side-table, we replicate
/// `Hashtable.equals` over the (working) `size`/`entrySet`/`get` natives via
/// virtual dispatch rather than reading the raw fields.
fn native_properties_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    let int_of = |r: MethodCallResult| -> i32 {
        match r {
            Ok(Some(Value::Int(n))) => n,
            _ => -1,
        }
    };
    let obj_of = |r: MethodCallResult| -> Option<ObjectRef> {
        match r {
            Ok(Some(Value::Object(o))) => o,
            _ => None,
        }
    };
    // `other` must be a Map of the same size. A non-Map `size()` call fails →
    // treated as not equal (the `instanceof Map` guard in Hashtable.equals).
    let this_size = int_of(ctx.invoke_virtual(this, "size", "()I", &[]));
    let other_size = int_of(ctx.invoke_virtual(other, "size", "()I", &[]));
    if this_size < 0 || other_size != this_size {
        return Ok(Some(Value::Int(0)));
    }
    let es = match obj_of(ctx.invoke_virtual(this, "entrySet", "()Ljava/util/Set;", &[])) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let it = match obj_of(ctx.invoke_virtual(es, "iterator", "()Ljava/util/Iterator;", &[])) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    loop {
        if int_of(ctx.invoke_virtual(it, "hasNext", "()Z", &[])) != 1 {
            break;
        }
        let entry = match obj_of(ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])) {
            Some(o) => o,
            None => return Ok(Some(Value::Int(0))),
        };
        let key = ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[])?;
        let value = obj_of(ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]));
        let key_arg = key.clone().unwrap_or(Value::Object(None));
        let other_val = obj_of(ctx.invoke_virtual(
            other,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key_arg],
        ));
        match value {
            None => {
                if other_val.is_some() {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Some(v) => {
                let eq = int_of(ctx.invoke_virtual(
                    v,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(other_val)],
                ));
                if eq != 1 {
                    return Ok(Some(Value::Int(0)));
                }
            }
        }
    }
    Ok(Some(Value::Int(1)))
}

/// Register the side-table-backed `Properties` natives.  Called from
/// `register_essential_natives` (real-JDK mode) so KeycloakMain's
/// `Version.<clinit>` finds a non-null `version` value.
/// JDK `Properties.saveConvert` — escape a key or value for `.properties`
/// output so it round-trips through `Properties.load` / our `parse_properties`.
///
/// `escape_space` escapes EVERY space (used for keys); for values only a leading
/// space is escaped (a space inside a value is left literal). `escape_unicode`
/// escapes every char outside `0x20..=0x7e` as `\uXXXX` — true for the
/// `store(OutputStream)` overload (the bytes are written ISO-8859-1, so any
/// non-Latin-1 char must be escaped), false for `store(Writer)` (the Writer's
/// charset encodes the raw char). Mirrors `java.util.Properties.saveConvert`
/// including the `c > 61 && c < 127` printable-ASCII fast path and the explicit
/// `=`/`:`/`#`/`!` escapes.
fn save_convert(s: &str, escape_space: bool, escape_unicode: bool) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for (i, ch) in s.chars().enumerate() {
        let c = ch as u32;
        // Fast path: printable ASCII above '=' (61) and below DEL (127).
        if c > 61 && c < 127 {
            if ch == '\\' {
                out.push_str("\\\\");
            } else {
                out.push(ch);
            }
            continue;
        }
        match ch {
            ' ' => {
                if i == 0 || escape_space {
                    out.push('\\');
                }
                out.push(' ');
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{000c}' => out.push_str("\\f"),
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(ch);
            }
            _ => {
                if (c < 0x20 || c > 0x7e) && escape_unicode {
                    // Emit one `\uXXXX` per UTF-16 code unit, so supplementary
                    // code points round-trip as the surrogate pair the JDK
                    // writes (our `parse_properties` recombines them on load).
                    let mut buf = [0u16; 2];
                    for unit in ch.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{:04x}", unit));
                    }
                } else {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// Mirror `java.util.Properties.writeComments`: prefix the comment block with
/// `#` and re-`#`-prefix after each embedded line break, terminating lines with
/// the platform separator `eol` (the JDK uses `bw.newLine()`). Comments are
/// ignored by `Properties.load`, so an approximate rendering is sufficient.
fn write_comments(out: &mut String, comments: &str, eol: &str) {
    out.push('#');
    let mut chars = comments.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                out.push_str(eol);
                out.push('#');
            }
            '\r' => {
                // Treat CRLF as a single break (swallow a following LF).
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(eol);
                out.push('#');
            }
            _ => out.push(ch),
        }
    }
    out.push_str(eol);
}

/// Best-effort `new java.util.Date().toString()` for the `#<date>` line the JDK
/// writes. Returns `None` (caller omits the line) if Date can't be built — the
/// line is a comment and never affects a `load` round-trip.
fn current_date_string(ctx: &mut dyn NativeContext) -> Option<String> {
    let date = match ctx.new_object_initialized("java/util/Date", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    match ctx.invoke_virtual(date, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Walk `this.entrySet()` through the VIRTUAL dispatch and collect the
/// `(key, value)` String pairs — faithfully mirroring what `Properties.store0`
/// does (it iterates `entrySet()`). This honors a subclass that overrides
/// `entrySet()`/`keySet()`: Spring's `SortedProperties.store(out, comments)`
/// calls `super.store(...)` and depends on the iteration order coming from its
/// overridden `entrySet()` (a sorted `TreeSet`). Reading the side-table directly
/// would drop that ordering. Non-String keys/values are skipped (real `store0`
/// would `ClassCastException`; the side-table model only carries Strings anyway).
fn collect_via_virtual_entryset(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Vec<(String, String)> {
    let this_pin = ctx.pin_native_root(this);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let set = match ctx.invoke_virtual(this_cur, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Vec::new();
        }
    };
    let it = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Vec::new();
        }
    };
    let it_pin = ctx.pin_native_root(it);
    let mut out = Vec::new();
    loop {
        let it_cur = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) if n != 0 => {}
            _ => break,
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let entry = match ctx.invoke_virtual(it_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let entry_cur = ctx.read_native_pin(entry_pin, entry);
        let key_v = ctx.invoke_virtual(entry_cur, "getKey", "()Ljava/lang/Object;", &[]);
        let entry_cur = ctx.read_native_pin(entry_pin, entry);
        let val_v = ctx.invoke_virtual(entry_cur, "getValue", "()Ljava/lang/Object;", &[]);
        ctx.unpin_native_roots(entry_pin);
        let k = match key_v {
            Ok(Some(Value::Object(Some(k)))) => ctx.read_string(k),
            _ => None,
        };
        let v = match val_v {
            Ok(Some(Value::Object(Some(v)))) => ctx.read_string(v),
            _ => None,
        };
        if let (Some(k), Some(v)) = (k, v) {
            out.push((k, v));
        }
        if out.len() >= MAX_PROPS_PER_OBJECT {
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    out
}

/// Collect the entries `store0` would serialize. For an exact `java/util/Properties`
/// use the side-table snapshot (the fast path — `entrySet()` would return the
/// same data). For a subclass, iterate the virtual `entrySet()` so overrides
/// (e.g. `SortedProperties`' sorted view) are honored.
fn collect_store_entries(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let mut this = this;
    let cid = ctx.class_id_of_object(this);
    let is_exact = ctx
        .class_name_of_id(cid)
        .is_none_or(|n| n == "java/util/Properties");
    if is_exact {
        return ordered_snapshot_kv(ctx, &mut this);
    }
    let entries = collect_via_virtual_entryset(ctx, this);
    // Fallback: if the virtual walk produced nothing (unexpected dispatch
    // failure) but the side-table has data, don't silently drop it.
    if entries.is_empty() {
        return ordered_snapshot_kv(ctx, &mut this);
    }
    entries
}

fn render_store_text(
    comments: Option<&str>,
    date: Option<&str>,
    entries: &[(String, String)],
    escape_unicode: bool,
    eol: &str,
) -> String {
    let mut text = String::new();
    if let Some(c) = comments {
        write_comments(&mut text, c, eol);
    }
    if let Some(d) = date {
        text.push('#');
        text.push_str(d);
        text.push_str(eol);
    }
    for (k, v) in entries {
        text.push_str(&save_convert(k, true, escape_unicode));
        text.push('=');
        text.push_str(&save_convert(v, false, escape_unicode));
        text.push_str(eol);
    }
    text
}

/// Build the full `.properties` text for `this`. This is what `Properties.store0`
/// would produce by iterating `entrySet()` — but our synthetic `Properties` (and
/// `System.getProperties()`) keep their entries in the side-table, not the
/// internal `map` ConcurrentHashMap the JDK bytecode reads, so the real `store0`
/// writes zero entries. Serializing here (via [`collect_store_entries`], which
/// honors subclass `entrySet()` overrides) keeps `store` consistent.
fn build_store_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    comments: Option<&str>,
    escape_unicode: bool,
) -> String {
    // The JDK's `store0` terminates every line with `bw.newLine()` =
    // `System.lineSeparator()`. Match it so callers that re-split the output on
    // the platform separator (e.g. Spring's `SortedProperties.store`, which does
    // `contents.split(System.lineSeparator())`) see the right line boundaries.
    let eol = ctx
        .get_system_property("line.separator")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "\n".to_string());
    // Pin `this` across the Date allocation so the entry walk below sees the
    // forwarded (post-GC) reference.
    let this_pin = ctx.pin_native_root(this);
    let date = current_date_string(ctx);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let entries = collect_store_entries(ctx, this_cur);
    ctx.unpin_native_roots(this_pin);
    render_store_text(comments, date.as_deref(), &entries, escape_unicode, &eol)
}

/// Native `Properties.store(OutputStream, String)` — serializes the side-table
/// as a `.properties` file (ISO-8859-1, `\uXXXX`-escaping non-Latin-1) and
/// writes it to the stream. The real JDK `store0` bytecode iterates the internal
/// `map` CHM, which our synthetic Properties never populates, so it emits 0
/// bytes; this native fixes `Properties.store`/`save` for `System.getProperties()`,
/// its `clone()`, and any side-table-backed Properties (Spring's
/// `ConcurrentBeanWrapperTests`, which stores a cloned system-properties snapshot
/// and reloads it, depends on this).
fn native_properties_store_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let out = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Read the (possibly-null) comments String before any allocation.
    let comments = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    // Pin both heap roots: building the text allocates (new Date, strings) and
    // filling the byte array allocates, either of which can move `this`/`out`.
    let this_pin = ctx.pin_native_root(this);
    let out_pin = ctx.pin_native_root(out);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let text = build_store_text(ctx, this_cur, comments.as_deref(), true);

    // ISO-8859-1 encode: with escape_unicode=true every entry char is ASCII;
    // comment/date chars are downgraded to one byte (>0xff collapses, matching
    // the JDK's lossy ISO-8859-1 comment write — comments don't affect `load`).
    let bytes: Vec<u8> = text.chars().map(|c| (c as u32 & 0xff) as u8).collect();
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    let out_cur = ctx.read_native_pin(out_pin, out);
    let write_res = ctx.invoke_virtual(out_cur, "write", "([B)V", &[Value::Object(Some(arr))]);
    let out_cur = ctx.read_native_pin(out_pin, out);
    let _ = ctx.invoke_virtual(out_cur, "flush", "()V", &[]);
    ctx.unpin_native_roots(this_pin);
    write_res?;
    Ok(None)
}

/// Native `Properties.store(Writer, String)` — same as the OutputStream overload
/// but writes the text straight to the `Writer` (no `\uXXXX` escaping; the
/// Writer's own charset encodes the characters).
fn native_properties_store_writer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let writer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let comments = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let this_pin = ctx.pin_native_root(this);
    let writer_pin = ctx.pin_native_root(writer);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let text = build_store_text(ctx, this_cur, comments.as_deref(), false);

    let str_obj = ctx.create_string(&text);
    let writer_cur = ctx.read_native_pin(writer_pin, writer);
    let write_res = ctx.invoke_virtual(
        writer_cur,
        "write",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(str_obj))],
    );
    let writer_cur = ctx.read_native_pin(writer_pin, writer);
    let _ = ctx.invoke_virtual(writer_cur, "flush", "()V", &[]);
    ctx.unpin_native_roots(this_pin);
    write_res?;
    Ok(None)
}

pub fn register_properties_sidetable(registry: &mut NativeMethodRegistry) {
    // FIX (2026-07-14, java.home/Locale bootstrap regression): every native
    // in this function is a permanent, correctness-critical BRIDGE, not an
    // approximation that real bytecode can substitute for. `System.
    // getProperties()` (see its registration in lib.rs) hands back a
    // "lightweight synthetic Properties object" whose inherited Hashtable/
    // ConcurrentHashMap backing is deliberately never populated — real JDK
    // 25 `Properties`/`Hashtable` bytecode dereferences a `map` field that is
    // permanently null on this object, so EVERY one of these overrides
    // (getProperty, get, put, size, keySet, forEach, ...) is the only thing
    // that makes the synthetic object behave like a real Map at all. Without
    // an explicit category, each of this function's ~3 call sites registers
    // under whatever `current_category` happens to be ambient there — bridge
    // in some, but (found 2026-07-14) `SyntheticStub` in real-JDK mode's own
    // `register_properties_sidetable` call sites in `vm/src/vm/vm_init.rs`.
    // Once commit `d8092acb` started actually enforcing
    // `set_drop_synthetic_stubs(true)` in real-JDK mode, EVERY registration
    // below was silently dropped, falling through to real bytecode's
    // null-`map` NPEs/no-ops for read paths that don't throw — including
    // `Properties.getProperty("java.home")` for the specific `Properties`
    // instance `jdk.internal.util.StaticProperty`'s bootstrap path reads,
    // which surfaced as `InternalError: null property: java.home` from
    // `java.util.Locale.<clinit>` (any real-JDK-mode program touching
    // `Locale` early). Explicitly pin `Bridge` here so this function's
    // behavior no longer depends on the caller's ambient category.
    registry.with_category(cratonvm_native_api::NativeKind::Bridge, |registry| {
    registry.register(
        "java/util/Properties",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_properties_equals,
    );
    // `Properties.store`/`save` run real JDK `store0` bytecode that iterates the
    // internal `map` ConcurrentHashMap. Our synthetic Properties keep entries in
    // the side-table, not that CHM, so the bytecode writes 0 bytes. Serialize
    // from the side-table instead (symmetric with the native `load`). Both the
    // OutputStream and Writer overloads, plus the deprecated `save` (which
    // delegates to store0 in the JDK), are covered.
    registry.register(
        "java/util/Properties",
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_properties_store_stream,
    );
    registry.register(
        "java/util/Properties",
        "store",
        "(Ljava/io/Writer;Ljava/lang/String;)V",
        native_properties_store_writer,
    );
    registry.register(
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_properties_store_stream,
    );
    registry.register(
        "java/util/Properties",
        "load",
        "(Ljava/io/InputStream;)V",
        native_properties_load,
    );
    // RKC16N.1 — jboss-modules' Main.<clinit> reads version.properties
    // through a BufferedReader and calls the Reader overload directly.
    registry.register(
        "java/util/Properties",
        "load",
        "(Ljava/io/Reader;)V",
        native_properties_load_reader,
    );
    registry.register(
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_properties_get_property_1,
    );
    registry.register(
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_properties_get_property_2,
    );
    registry.register(
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        native_properties_set_property,
    );
    registry.register(
        "java/util/Properties",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_put,
    );
    registry.register(
        "java/util/Properties",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_properties_contains_key,
    );
    // S111r11 SB3 (formerly a no-op stub registered in lib.rs):
    // ModuleBootstrap.<clinit> calls `getAndRemoveProperty(key)` which is
    // `(String) System.getProperties().remove(key)`; H2's ConnectionInfo
    // calls `prop.remove("USER")` to strip the JDBC USER setting before
    // Engine.openSession iterates connection keys.  JDK 25's
    // `Properties.remove(Object)` (Properties.java:1348) reads a private
    // `ConcurrentHashMap<Object,Object> map` field that's null on our
    // synthetic Properties, so the JDK bytecode NPEs.  Side-table-aware
    // remove returns the previous value (or null if absent) — H2's
    // ConnectionInfo.removeProperty now actually removes USER, and
    // ModuleBootstrap's getAndRemoveProperty still gets null for keys
    // that were never set.
    registry.register(
        "java/util/Properties",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_remove,
    );
    // clear() must empty the side-table too, not just the real `map` CHM the
    // bytecode clears — otherwise getProperty/get/size keep reading stale
    // side-table entries (asymmetric with put/setProperty/remove, which now
    // keep both in sync).
    registry.register(
        "java/util/Properties",
        "clear",
        "()V",
        native_properties_clear,
    );
    // Spring's PropertySourcesPropertyResolver reads through the
    // Hashtable.get(Object) interface rather than getProperty(String),
    // and the JDK 25 Properties.get override at Properties.java:1338
    // dereferences a `ConcurrentHashMap<Object,Object> map` field that's
    // null on our synthetic Properties.  Route the read through the
    // side-table so MapPropertySource gets a sensible result.
    registry.register(
        "java/util/Properties",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_get,
    );
    // Spring's `SpringConfigurationPropertySource.isFullEnumerable`
    // calls `Map.size()` on the underlying property source.  When that
    // source is our synthetic `System.getProperties()` Properties, the
    // JDK 25 `Properties.size` (Properties.java:1302) reads a private
    // `ConcurrentHashMap<Object,Object> map` field that's null, NPEing
    // before Spring's `Binder.get` gets a chance to enumerate.  Route
    // size/isEmpty/keySet/values/entrySet/keys/elements/contains
    // through the side-table so the synthetic Properties behaves as a
    // properly-empty (or populated) Map for real JDK callers.
    registry.register(
        "java/util/Properties",
        "size",
        "()I",
        native_properties_size,
    );
    registry.register(
        "java/util/Properties",
        "isEmpty",
        "()Z",
        native_properties_is_empty,
    );
    registry.register(
        "java/util/Properties",
        "keySet",
        "()Ljava/util/Set;",
        native_properties_key_set,
    );
    // `Properties.keySet()` returns a disconnected snapshot
    // (`native_properties_key_set`/`build_key_set`, now a `LinkedHashSet`
    // specifically), not a real live view — mutating it otherwise silently
    // never touched the source `Properties`. Gated on
    // `properties_keyset_source`: an ordinary `LinkedHashSet` (anything not
    // built by `build_key_set`) passes straight through to real bytecode
    // via `invoke_virtual_bytecode_only`. Scoped to `LinkedHashSet` rather
    // than the far more common `HashSet` specifically to keep this
    // override's blast radius to objects this module itself creates.
    registry.register(
        "java/util/LinkedHashSet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_linkedhashset_retain_all,
    );
    registry.register(
        "java/util/LinkedHashSet",
        "remove",
        "(Ljava/lang/Object;)Z",
        native_linkedhashset_remove,
    );
    registry.register(
        "java/util/Properties",
        "stringPropertyNames",
        "()Ljava/util/Set;",
        native_properties_string_property_names,
    );
    registry.register(
        "java/util/Properties",
        "values",
        "()Ljava/util/Collection;",
        native_properties_values,
    );
    registry.register(
        "java/util/Properties",
        "entrySet",
        "()Ljava/util/Set;",
        native_properties_entry_set,
    );
    registry.register(
        "java/util/Properties",
        "keys",
        "()Ljava/util/Enumeration;",
        native_properties_keys,
    );
    // `propertyNames()` differs from `keys()`: it also enumerates the
    // `defaults` chain (JDK contract). Distinct native — keep `keys()` own-only.
    registry.register(
        "java/util/Properties",
        "propertyNames",
        "()Ljava/util/Enumeration;",
        native_properties_property_names,
    );
    registry.register(
        "java/util/Properties",
        "elements",
        "()Ljava/util/Enumeration;",
        native_properties_elements,
    );
    registry.register(
        "java/util/Properties",
        "contains",
        "(Ljava/lang/Object;)Z",
        native_properties_contains,
    );
    registry.register(
        "java/util/Properties",
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_properties_contains_value,
    );
    // WildFly / log4j-api 2.23 StatusLogger$Config.<clinit> →
    // PropertiesUtilsDouble.normalizeProperties calls
    // `properties.forEach(BiConsumer)` on `System.getProperties()` (and
    // a freshly-built env/file Properties).  JDK 25's Properties.forEach
    // dereferences `map.forEach`; on our synthetic Properties `map` is
    // null, so route forEach through the side-table directly.
    registry.register(
        "java/util/Properties",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_properties_for_each,
    );
    // Spring Boot 4 `AutoConfigurationMetadataLoader.loadMetadata` aggregates
    // `META-INF/spring-autoconfigure-metadata.properties` from every classpath
    // jar by calling `aggregate.putAll(perJarProperties)` for each loaded
    // file.  Because Properties stores its entries in the side-table (not in
    // the inherited `HashMap` buckets), the generic `Map.putAll` walker in
    // `native_map_put_all` finds zero entries on the source Properties and
    // the aggregate stays empty.  Result: every `OnClassCondition`/
    // `OnWebApplicationCondition` filter sees an empty
    // `AutoConfigurationMetadata`, all `ConditionalOnClass`/`ConditionalOnWeb`
    // lookups return null, and downstream auto-config classes (e.g.
    // `TomcatServletWebServerAutoConfiguration`) get dropped — Spring then
    // fails with `MissingWebServerFactoryBeanException`.
    //
    // Side-table-aware putAll: snapshot the source's side-table and store
    // each (k,v) into `this`'s side-table directly.
    registry.register(
        "java/util/Properties",
        "putAll",
        "(Ljava/util/Map;)V",
        native_properties_put_all,
    );
    // Quartz's `StdSchedulerFactory.initialize(Properties)` (invoked via
    // Spring's `SchedulerFactoryBean.initSchedulerFactory`) does
    // `props.putIfAbsent("org.quartz.jobStore.class",
    // LocalDataSourceJobStore.class.getName())` when a DataSource is
    // configured. Without a native here, `putIfAbsent` (inherited from
    // `Hashtable`, never overridden by `Properties` itself) ran real
    // bytecode against the REAL `map`/`table` backing fields, bypassing
    // the side-table entirely. The subsequent read —
    // `PropertiesParser.getStringProperty("org.quartz.jobStore.class",
    // RAMJobStore.class.getName())`, which is our side-table-only
    // `getProperty(String,String)` native — never saw the write and fell
    // back to Quartz's own default, silently wiring up `RAMJobStore`
    // instead of `LocalDataSourceJobStore` even though the
    // `spring.quartz.job-store-type=jdbc` customizer ran successfully.
    // See fixed-suite-bugs/springboot/quartzautoconfigurationtests-jdbc-jobstore-not-applied-FIXED.md.
    registry.register(
        "java/util/Properties",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_put_if_absent,
    );
    // JDK 25's Properties.computeIfAbsent delegates directly to the private
    // ConcurrentHashMap `map`. Synthetic Properties keep String entries in
    // this side table, so route the functional update through the same get/put
    // bridges. Spring's MapBinder uses exactly this path.
    registry.register(
        "java/util/Properties",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_properties_compute_if_absent,
    );
    });
}

/// Native `Properties.putAll(Map)` — side-table-aware copy.
///
/// The generic `Map.putAll` walker in `native-collections` enumerates the
/// source by reading its `HashMap` buckets / `LinkedHashMap` insertion-order
/// list.  Properties keep their entries in the per-object side-table, so a
/// generic walk sees zero entries and the destination Properties stays
/// empty.  This override snapshots the source side-table and stores each
/// `(k,v)` into the destination via `put_kv`.
fn native_properties_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // 1) Side-table snapshot — covers Properties->Properties putAll (the
    //    dominant case that previously silently dropped all entries because
    //    Properties stores its data outside the inherited HashMap buckets).
    //    Also copy `other`'s CHM-only entries (non-String key/value pairs —
    //    e.g. a `ManagedProperties` whose `<props>` keys are `TypedStringValue`)
    //    so a Properties->Properties putAll/merge doesn't drop them.
    // Family-1 fix (cce0079 tree-key tail, T21_001 capture): the snapshot /
    // side-key / chm-extra helpers and every per-entry dispatch below can
    // move `this`/`other`/the extra-entry refs — `put_kv` then read the
    // receiver's identity hash through a STALE header (canary-caught in
    // `CorbaNamingService.<init>`). Pin the two receivers and refresh
    // before each use; pin the extra-entry refs per iteration.
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let mut this = this;
    let mut other = other;
    let snapshot = ordered_snapshot_kv(ctx, &mut other);
    this = ctx.read_native_pin(this_pin, this);
    other = ctx.read_native_pin(other_pin, other);
    let other_side_keys = side_key_set(ctx, other);
    this = ctx.read_native_pin(this_pin, this);
    other = ctx.read_native_pin(other_pin, other);
    let other_chm_extra = chm_extra_entries(ctx, other, &other_side_keys);
    this = ctx.read_native_pin(this_pin, this);
    if !snapshot.is_empty() || !other_chm_extra.is_empty() {
        for (k, v) in &snapshot {
            put_kv(ctx, this, k, v);
        }
        // Mirror into `this`'s real `map` CHM backing too, so the destination
        // stays consistent for generic Map walkers (cf. native_properties_put).
        mirror_loaded_entries_to_properties_backend(ctx, this, &snapshot);
        this = ctx.read_native_pin(this_pin, this);
        for (key_obj, value, _kstr) in other_chm_extra {
            let ko_pin = ctx.pin_native_root(key_obj);
            let vh = match value {
                Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                _ => None,
            };
            let key_obj = ctx.read_native_pin(ko_pin, key_obj);
            let value = match (value, vh) {
                (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
                (v, _) => v,
            };
            put_non_string_into_chm(ctx, this, Value::Object(Some(key_obj)), value);
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(ko_pin);
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }
    // 2) Fallback — source is a regular Map (HashMap/LinkedHashMap).  Walk
    //    its entries through the generic Map.entrySet() so we don't depend
    //    on internal field layouts, then store each (k,v) into `this`'s
    //    side-table and mirror them into the real `map` CHM backing.
    //    Non-String values (e.g. XProperty, MemberDetails passed through
    //    setTypeParameters) are stored ONLY in the CHM via
    //    put_non_string_into_chm — never in the side-table.  Storing a ""
    //    sentinel for non-String values would shadow the CHM in
    //    native_properties_get, causing a String→XProperty CCE downstream.
    let mut str_collected: Vec<(String, String)> = Vec::new();
    let entries_obj = match ctx.invoke(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(other))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    this = ctx.read_native_pin(this_pin, this);
    let it = match ctx.invoke(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(entries_obj))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    this = ctx.read_native_pin(this_pin, this);
    // Family-1 fix (cce0079): pin the iterator; refresh it and `this` after
    // every per-entry dispatch; pin `entry`/`key_obj` across the dispatches
    // within one iteration.
    let it_pin = ctx.pin_native_root(it);
    let mut it = it;
    loop {
        let has_next = match ctx.invoke(
            "java/util/Iterator",
            "hasNext",
            "()Z",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Int(n))) => n != 0,
            _ => false,
        };
        it = ctx.read_native_pin(it_pin, it);
        this = ctx.read_native_pin(this_pin, this);
        if !has_next {
            break;
        }
        let entry = match ctx.invoke(
            "java/util/Iterator",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        it = ctx.read_native_pin(it_pin, it);
        let entry_pin = ctx.pin_native_root(entry);
        let key_obj = match ctx.invoke(
            "java/util/Map$Entry",
            "getKey",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let entry = ctx.read_native_pin(entry_pin, entry);
        let key_pin = ctx.pin_native_root(key_obj);
        let val_v = match ctx.invoke(
            "java/util/Map$Entry",
            "getValue",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(v)) => v,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let key_obj = ctx.read_native_pin(key_pin, key_obj);
        it = ctx.read_native_pin(it_pin, it);
        this = ctx.read_native_pin(this_pin, this);
        let k_opt = ctx.read_string(key_obj);
        let v_str = match val_v {
            Value::Object(Some(val_obj)) => ctx.read_string(val_obj),
            _ => None,
        };
        match (k_opt, v_str) {
            (Some(k), Some(v_str)) if !k.is_empty() => {
                // String→String: collect for batched side-table + CHM mirror.
                str_collected.push((k, v_str));
            }
            _ => {
                // Non-String key and/or value: store directly in the real JDK
                // CHM so native_properties_get / size() / entrySet() observe it.
                // (A non-String key — e.g. Spring's `TypedStringValue` — was
                // previously dropped by the `k.is_empty()` skip.)
                put_non_string_into_chm(ctx, this, Value::Object(Some(key_obj)), val_v);
                this = ctx.read_native_pin(this_pin, this);
                it = ctx.read_native_pin(it_pin, it);
            }
        }
        ctx.unpin_native_roots(entry_pin);
    }
    for (k, v) in &str_collected {
        put_kv(ctx, this, k, v);
    }
    mirror_loaded_entries_to_properties_backend(ctx, this, &str_collected);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The gh-11892 shape, with the exact key order HotSpot's
    /// `Properties`/`ConcurrentHashMap` produces for these four keys (verified
    /// against Temurin 25 on 2026-08-01). Spring Boot's `MapBinder` binds
    /// `commit.id` as a nested `Map` only if `commit.id` is enumerated AFTER at
    /// least one of its descendants — which is precisely what this ordering
    /// delivers and what a raw hash-bucket order destroyed.
    #[test]
    fn reorder_follows_the_chm_order_for_the_git_properties_shape() {
        let side = kv(&[
            ("branch", "master"),
            ("commit.id", "1b3cec34f7ca0a021244452f2cae07a80497a7c7"),
            ("commit.id.abbrev", "1b3cec3"),
            ("commit.id.full", "1b3cec34f7ca0a021244452f2cae07a80497a7c7"),
        ]);
        let order: Vec<String> = ["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let reordered = reorder_by(&side, &order);
        let got: Vec<&str> = reordered.iter().map(|(k, _v)| k.as_str()).collect();
        assert_eq!(
            got,
            vec!["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]
        );
        assert!(
            got.iter().position(|k| *k == "commit.id").unwrap()
                > got.iter().position(|k| *k == "commit.id.abbrev").unwrap(),
            "an ancestor key that is also a leaf value must enumerate AFTER its \
             descendants, or MapBinder binds it as a scalar and drops the nest"
        );
    }

    /// `order` permutes; it must never filter, duplicate, or invent.
    #[test]
    fn reorder_preserves_every_entry() {
        let side = kv(&[("a", "1"), ("b", "2"), ("c", "3")]);
        // `z` is in the CHM but not the side-table; `b` is not named by `order`.
        let order: Vec<String> = ["z", "c", "a"].iter().map(|s| s.to_string()).collect();
        let got = reorder_by(&side, &order);
        assert_eq!(got, kv(&[("c", "3"), ("a", "1"), ("b", "2")]));
        assert_eq!(got.len(), side.len());
    }

    #[test]
    fn reorder_tolerates_a_duplicated_order_key() {
        let side = kv(&[("a", "1"), ("b", "2")]);
        let order: Vec<String> = ["b", "b", "a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(reorder_by(&side, &order), kv(&[("b", "2"), ("a", "1")]));
    }

    /// No CHM backing: the side-table's own order passes through untouched.
    #[test]
    fn reorder_with_no_chm_order_is_a_passthrough() {
        let side = kv(&[("a", "1"), ("b", "2")]);
        assert_eq!(reorder_by(&side, &[]), side);
    }

    /// The side-table is the order every enumeration native falls back to when
    /// the receiver has no CHM backing, so it must be insertion-ordered rather
    /// than hash-bucket-ordered, and a removal must not disturb the survivors.
    #[test]
    fn props_map_is_insertion_ordered_across_removal() {
        let mut m = PropsMap::default();
        for k in ["branch", "commit.id", "commit.id.abbrev", "commit.id.full"] {
            m.insert(k.to_string(), "v".to_string());
        }
        let keys: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["branch", "commit.id", "commit.id.abbrev", "commit.id.full"]
        );
        // `shift_remove` (what `remove_kv` uses) keeps the survivors in order;
        // `swap_remove` would teleport the last key into the hole.
        m.shift_remove("commit.id");
        let keys: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["branch", "commit.id.abbrev", "commit.id.full"]);
        // Re-inserting an existing key must NOT move it to the back.
        m.insert("branch".to_string(), "other".to_string());
        assert_eq!(m.keys().next().map(|k| k.as_str()), Some("branch"));
    }

    /// `reorder_by` must not depend on the side-table's own iteration order for
    /// any key the CHM names — that is the whole point of deferring to the CHM,
    /// and it is what makes the JDK-order guarantee independent of however the
    /// side-table happens to be stored. Feed the same entries in two different
    /// orders and require the same answer.
    #[test]
    fn reorder_is_independent_of_side_table_order_for_chm_named_keys() {
        let order: Vec<String> = ["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = kv(&[
            ("branch", "b"),
            ("commit.id", "i"),
            ("commit.id.abbrev", "a"),
            ("commit.id.full", "f"),
        ]);
        let b = kv(&[
            ("commit.id", "i"),
            ("commit.id.full", "f"),
            ("branch", "b"),
            ("commit.id.abbrev", "a"),
        ]);
        assert_eq!(reorder_by(&a, &order), reorder_by(&b, &order));
        assert_eq!(
            reorder_by(&a, &order),
            kv(&[
                ("commit.id.full", "f"),
                ("branch", "b"),
                ("commit.id.abbrev", "a"),
                ("commit.id", "i"),
            ])
        );
    }

    /// Source-level guard. Every native that hands an *enumeration order* to
    /// Java must read `ordered_snapshot_kv`, never the raw, insertion-ordered
    /// `snapshot_kv` — the whole gh-11892 defect was one such site reading an
    /// order the JDK would never produce. New enumeration natives are easy to
    /// add and easy to wire to the wrong helper, and no end-to-end test would
    /// notice until some binder somewhere silently picked the wrong branch. So
    /// pin the call sites: a bare `snapshot_kv(` is allowed only in the
    /// functions listed here, each of which is order-insensitive or is the
    /// ordering machinery itself.
    #[test]
    fn only_order_insensitive_functions_read_the_unordered_snapshot() {
        const ALLOWED: &[&str] = &[
            // The definition itself, and its `&dyn`-context public re-export
            // (surefire copies entries into a CHM — order-insensitive).
            "snapshot_kv",
            "snapshot_sidetable",
            // The ordering machinery: reads the raw snapshot, then orders it.
            "ordered_snapshot_kv",
            // A value-membership test: `Properties.contains(Object)`.
            "native_properties_contains",
        ];
        let src = include_str!("properties_sidetable.rs");
        let mut current = String::new();
        let mut offenders: Vec<String> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed
                .strip_prefix("fn ")
                .or_else(|| trimmed.strip_prefix("pub fn "))
                .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            {
                current = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            // A bare `snapshot_kv(` — i.e. not the `ordered_` variant, and not
            // this test's own literals.
            let mut idx = 0;
            while let Some(hit) = line[idx..].find("snapshot_kv(") {
                let at = idx + hit;
                let ordered = line[..at].ends_with("ordered_");
                let quoted = line.trim_start().starts_with("//") || line.contains('"');
                if !ordered && !quoted && current != "snapshot_kv" {
                    offenders.push(current.clone());
                }
                idx = at + "snapshot_kv(".len();
            }
        }
        offenders.sort();
        offenders.dedup();
        let unexpected: Vec<&String> = offenders
            .iter()
            .filter(|f| !ALLOWED.contains(&f.as_str()))
            .collect();
        assert!(
            unexpected.is_empty(),
            "these functions read the UNORDERED side-table snapshot: {unexpected:?}. \
             If the result is handed to Java as an iteration order, call \
             `ordered_snapshot_kv` instead; if it genuinely cannot be, add it to ALLOWED \
             with a note saying why."
        );
    }

    #[test]
    fn parse_simple_kv() {
        let p = parse_properties(b"key=value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_multiple_kv() {
        let p = parse_properties(b"a=1\nb=2\nc=3\n");
        assert_eq!(
            p,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_comments_skipped() {
        let p = parse_properties(b"#comment\n!exclamation\nkey=value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_blank_lines_skipped() {
        let p = parse_properties(b"\n\nkey=value\n\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_whitespace_separator() {
        let p = parse_properties(b"key value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn save_convert_escapes_specials() {
        // Keys escape every space; `=`,`:`,`#`,`!` and `\` always escape.
        assert_eq!(save_convert("a b", true, false), "a\\ b");
        assert_eq!(save_convert("a=b:c#d!e", true, false), "a\\=b\\:c\\#d\\!e");
        assert_eq!(save_convert("c:\\path", false, false), "c\\:\\\\path");
        // Values only escape a LEADING space, not interior ones.
        assert_eq!(save_convert(" lead mid", false, false), "\\ lead mid");
        assert_eq!(save_convert("a\tb\nc", false, false), "a\\tb\\nc");
    }

    #[test]
    fn save_convert_unicode_escaping() {
        // escape_unicode=true escapes non-Latin chars as one `\u` per code unit.
        assert_eq!(save_convert("\u{00e9}", false, true), "\\u00e9"); // é
                                                                      // Supplementary code point -> surrogate pair (two \u units).
        assert_eq!(save_convert("\u{1F600}", false, true), "\\ud83d\\ude00");
        // escape_unicode=false leaves the char literal (Writer charset encodes it).
        assert_eq!(save_convert("\u{00e9}", false, false), "\u{00e9}");
    }

    #[test]
    fn save_convert_then_parse_roundtrips() {
        // Representative of a real system-property snapshot: paths, `=`/`:`,
        // spaces and backslashes must survive store(save_convert) -> load(parse).
        let pairs = [
            ("java.class.path", "C:\\a;C:/b:dir with space"),
            ("line.separator", "\r\n"),
            ("key with space", "v=a:l#u!e"),
            ("plain", "value"),
        ];
        let mut text = String::new();
        for (k, v) in &pairs {
            text.push_str(&save_convert(k, true, true));
            text.push('=');
            text.push_str(&save_convert(v, false, true));
            text.push('\n');
        }
        let parsed = parse_properties(text.as_bytes());
        let expected: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn render_store_text_includes_date_before_first_entry() {
        let entries = vec![
            ("code2".to_string(), "message2".to_string()),
            ("code1".to_string(), "message1".to_string()),
        ];
        let text = render_store_text(
            None,
            Some("Thu Jan 01 00:00:00 UTC 1970"),
            &entries,
            true,
            "\n",
        );
        assert!(text.starts_with("#Thu Jan 01 00:00:00 UTC 1970\n"));
        assert!(
            text.contains("\ncode2=message2\n"),
            "first entry must be newline-prefixed by the date comment: {text:?}"
        );
    }

    #[test]
    fn render_store_text_keeps_user_comment_before_date() {
        let entries = vec![("key".to_string(), "value".to_string())];
        let text = render_store_text(Some("header"), Some("DATE"), &entries, true, "\r\n");
        assert_eq!(text, "#header\r\n#DATE\r\nkey=value\r\n");
    }

    #[test]
    fn parse_colon_separator() {
        let p = parse_properties(b"key:value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_continuation_line() {
        let p = parse_properties(b"key=long\\\n    value\n");
        assert_eq!(p, vec![("key".to_string(), "longvalue".to_string())]);
    }

    #[test]
    fn parse_unicode_escape() {
        let p = parse_properties(b"key=\\u00e9\n");
        assert_eq!(p[0].0, "key");
        assert!(p[0].1.starts_with('\u{00e9}'));
    }

    #[test]
    fn parse_keycloak_version_shape() {
        let bytes = b"version=26.2.4\nbuild-time=2025-04-26T13:00:00Z\nresources-version=26.2.4\n";
        let p = parse_properties(bytes);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], ("version".to_string(), "26.2.4".to_string()));
        assert_eq!(
            p[1],
            ("build-time".to_string(), "2025-04-26T13:00:00Z".to_string())
        );
        assert_eq!(
            p[2],
            ("resources-version".to_string(), "26.2.4".to_string())
        );
    }

    #[test]
    fn parse_caps_at_max_props() {
        // Generate 12_000 lines; we should accept exactly MAX_PROPS_PER_OBJECT.
        let mut bytes = Vec::new();
        for i in 0..12_000 {
            bytes.extend_from_slice(format!("k{}=v{}\n", i, i).as_bytes());
        }
        let p = parse_properties(&bytes);
        assert_eq!(p.len(), MAX_PROPS_PER_OBJECT);
    }

    #[test]
    fn parse_rejects_oversized_key() {
        let mut bytes = b"k".to_vec();
        bytes.extend(std::iter::repeat(b'a').take(MAX_KV_LEN + 1));
        bytes.extend_from_slice(b"=v\n");
        let p = parse_properties(&bytes);
        assert!(p.is_empty(), "oversized key must be rejected");
    }

    #[test]
    fn parse_no_separator_yields_empty_value() {
        let p = parse_properties(b"keyonly\n");
        assert_eq!(p, vec![("keyonly".to_string(), String::new())]);
    }

    #[test]
    fn unescape_backslash_n() {
        assert_eq!(unescape(r"\n"), "\n");
        assert_eq!(unescape(r"\t"), "\t");
        assert_eq!(unescape(r"\f"), "\u{000c}");
        assert_eq!(unescape(r"\\"), "\\");
        assert_eq!(unescape(r"\:"), ":");
        assert_eq!(unescape(r"\="), "=");
    }

    #[test]
    fn unescape_unicode() {
        assert_eq!(unescape("\\u0041"), "A");
        assert_eq!(unescape("\\u00e9"), "\u{00e9}");
    }

    #[test]
    fn unescape_surrogate_pair_combines_to_supplementary() {
        // U+1F600 GRINNING FACE is written as the surrogate pair
        // 😀 in a .properties file. The two halves must combine
        // into the single supplementary code point (previously BOTH halves
        // were dropped, yielding an empty string).
        assert_eq!(unescape("\\uD83D\\uDE00"), "\u{1F600}");
        // Surrounded by ordinary text, and with lowercase hex.
        assert_eq!(unescape("a\\ud83d\\ude00b"), "a\u{1F600}b");
    }

    #[test]
    fn unescape_lone_surrogate_becomes_replacement_not_dropped() {
        // A lone surrogate can't live in a Rust String; we substitute U+FFFD
        // rather than silently dropping the unit (exact preservation needs
        // the cross-file wide-unit path). The key assertion is that *some*
        // character survives — the value is not silently lost.
        assert_eq!(unescape("\\uD800"), "\u{FFFD}"); // lone high
        assert_eq!(unescape("\\uDC00"), "\u{FFFD}"); // lone low
                                                     // High surrogate followed by a NON-low escape: the high is replaced,
                                                     // and the trailing 'A' (A) is preserved.
        assert_eq!(unescape("\\uD800\\u0041"), "\u{FFFD}A");
    }

    #[test]
    fn unescape_short_u_decodes_digits_present_not_dropped() {
        // Fewer than 4 hex digits is malformed per the JDK. We decode the
        // digits actually present rather than silently swallowing them.
        assert_eq!(unescape("\\u41"), "A"); // 0x41 from 2 digits
        assert_eq!(unescape("\\u41Z"), "AZ"); // stops at non-hex 'Z'
                                              // `\u` with no hex digit at all: preserve the 'u' literally.
        assert_eq!(unescape("\\u"), "u");
        assert_eq!(unescape("\\uZ"), "uZ");
    }

    #[test]
    fn iso_8859_1_bytes_round_trips_latin1_and_collapses_above() {
        // ASCII: identity.
        assert_eq!(iso_8859_1_bytes("abc=1"), b"abc=1".to_vec());
        // 'é' is U+00E9 — fits in Latin-1 and survives as 0xE9.
        assert_eq!(iso_8859_1_bytes("é"), vec![0xE9]);
        // 'Ω' is U+03A9 — outside Latin-1, must collapse to '?'.
        assert_eq!(iso_8859_1_bytes("Ω"), vec![b'?']);
        // Mixed: ASCII + Latin-1 + above-Latin-1 in one string.
        assert_eq!(iso_8859_1_bytes("kéΩ"), vec![b'k', 0xE9, b'?']);
        // The downgraded byte sequence for a Latin-1 line must
        // round-trip through `parse_properties` cleanly.
        let mut bytes = iso_8859_1_bytes("name=café\n");
        // Trailing newline preserved.
        assert_eq!(bytes.last(), Some(&b'\n'));
        bytes.push(0); // sanity: ensure Vec is mutable / well-formed
        bytes.pop();
        let parsed = parse_properties(&iso_8859_1_bytes("name=café\n"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "name");
        // `parse_properties` decodes 0xE9 back to U+00E9, so the
        // value reads as "café" again.
        assert_eq!(parsed[0].1, "café");
    }

    #[test]
    fn split_kv_separator_priority() {
        assert_eq!(
            split_key_value("key=value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key:value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key  =  value"),
            ("key".to_string(), "value".to_string())
        );
    }
}
