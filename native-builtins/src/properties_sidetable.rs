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
//! a `Mutex<FxHashMap<usize, IndexMap<JavaText, JavaText>>>` — see
//! [`JavaText`] for why the inner text is UTF-16 units and not `String`;
//! both layers
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
/// The stored form of a `Properties` key or value: raw UTF-16 code units.
///
/// # Why not `String`
///
/// A Rust `str` cannot hold an *unpaired* surrogate, and `Properties` text is
/// arbitrary Java `String` content, which can. Storing `String` here meant two
/// distinct defects, both MEASURED against HotSpot 25 (see
/// `G55-1-the-key-the-map-could-not-find-again-20260817.md`):
///
///   * every read-back — `getProperty`, `get`, `keySet`, `propertyNames`,
///     `elements`, `stringPropertyNames`, `store`, the `defaults` chain —
///     substituted `U+FFFD` for the surrogate, so the value came back visibly
///     wrong; and
///   * two *distinct* keys that differ only in which unpaired surrogate they
///     carry both collapsed to the same `U+FFFD` text, so the second
///     `setProperty` silently overwrote the first and `size()` answered 1 for
///     two keys. That one is worse: it is a lookup that cannot fail loudly.
///
/// Ordering and equality are by code unit, which is what Java's
/// `String.equals`/`compareTo`/`hashCode` use, so a derived `Ord`/`Eq`/`Hash`
/// is exactly the Java contract — unlike `String`'s, which orders by code
/// *point* and therefore sorts a supplementary character after `U+FFFF`
/// where Java sorts it before.
///
/// [`JavaText::to_lossy`] is the deliberate one-way door back to Rust text,
/// for the places whose destination genuinely is a Rust `&str` (diagnostics,
/// `System.setProperty`, the cross-module `&str` API below). It is never on
/// the path back to a Java `String` — that is [`create_property_string`].
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct JavaText(Vec<u16>);

impl JavaText {
    fn from_units(units: Vec<u16>) -> Self {
        JavaText(units)
    }

    fn units(&self) -> &[u16] {
        &self.0
    }

    /// Number of UTF-16 code units — the same number `String.length()` answers,
    /// which is what `MAX_KV_LEN` and the `Properties` size caps mean to Java.
    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Rust text for this content, with any unpaired surrogate replaced by
    /// `U+FFFD`. Lossy BY CONSTRUCTION — call it only where the destination is
    /// Rust text and never where it is a Java `String`.
    fn to_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
}

impl From<&str> for JavaText {
    fn from(s: &str) -> Self {
        JavaText(s.encode_utf16().collect())
    }
}

/// Read a Java `String` receiver as code units, or `None` when `obj` is not a
/// `String` at all.
///
/// The guard is the TYPE TEST and nothing else — it decides "is this a
/// `java.lang.String`", which is exactly the question every caller below used
/// to put to `ctx.read_string`. The units then come from
/// `lang_string::read_string_chars`, this crate's units-preserving reader,
/// which duck-types any object with an array in slot 0 and so cannot be the
/// guard itself. There is deliberately no third spelling of a String decode.
///
/// `java_string_hash_code` is consulted as a second opinion because it is
/// answered VM-side from the receiver's class identity, so it recognises a
/// `String` whose content a `str`-returning reader has to refuse. On the VM
/// `read_string` is lossy rather than refusing, so this pair accepts exactly
/// what the old `ctx.read_string(...)` call accepted and nothing more.
fn read_java_text(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<JavaText> {
    if ctx.read_string(obj).is_none() && ctx.java_string_hash_code(obj).is_none() {
        return None;
    }
    Some(JavaText::from_units(crate::lang_string::read_string_chars(
        ctx, obj,
    )))
}

/// Materialise `text` as a Java `String`.
///
/// Well-formed content takes the *unchanged* `create_string` path — same
/// interning, same object identity behaviour every green `Properties` vector
/// already measured. Only content a `&str` cannot carry goes the long way
/// round, through `lang_string::sb_string_from_units` (`new_object` +
/// `NativeContext::init_string_from_units`), which is the crate's one
/// units-preserving String constructor.
fn create_property_string(ctx: &mut dyn NativeContext, text: &JavaText) -> ObjectRef {
    if !crate::lang_string::has_unpaired_surrogate(text.units()) {
        return ctx.create_string(&text.to_lossy());
    }
    match crate::lang_string::sb_string_from_units(ctx, text.units()) {
        Ok(obj) => obj,
        // Only reachable on heap exhaustion. A lossy String beats handing a
        // null out of a String-typed method.
        Err(_) => ctx.create_string(&text.to_lossy()),
    }
}

type PropsMap = indexmap::IndexMap<JavaText, JavaText, BuildHasherDefault<FxHasher>>;

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
    //
    // The slot COUNT is checked before any of those slots is read. Without it
    // this is a shape test that reads past a shorter receiver: Tomcat's
    // `CoyoteInputStream` declares one field, so slots 1 and 3 land on whatever
    // follows the object -- and the values found there decide whether the
    // caller goes on to trust `buf`/`pos`/`count`. Same defect, and the same
    // fix, as `has_byte_array_stream_layout` in `phases_late/zip_streams.rs`.
    // A receiver too short for the layout leaves all three `None` and falls
    // through to strategy 3, which works for any real `InputStream`.
    let (buf_by_idx, count_by_idx, pos_by_idx) = if ctx.object_num_fields(stream) > 3 {
        (
            match ctx.get_field(stream, 0) {
                Value::Object(Some(arr)) => Some(arr),
                _ => None,
            },
            match ctx.get_field(stream, 3) {
                Value::Int(n) => Some(n as usize),
                _ => None,
            },
            match ctx.get_field(stream, 1) {
                Value::Int(n) => Some(n as usize),
                _ => None,
            },
        )
    } else {
        (None, None, None)
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
fn parse_properties(bytes: &[u8]) -> Vec<(JavaText, JavaText)> {
    // Decode as ISO-8859-1 (Java spec for `Properties.load(InputStream)`).
    // Each byte maps to one Unicode code point in 0..=255.
    let raw: String = bytes.iter().map(|&b| b as char).collect();
    parse_properties_text(&raw)
}

fn parse_properties_strict(bytes: &[u8]) -> Result<Vec<(JavaText, JavaText)>, ()> {
    // Decode as ISO-8859-1 (Java spec for `Properties.load(InputStream)`).
    // Each byte maps to one Unicode code point in 0..=255.
    let raw: String = bytes.iter().map(|&b| b as char).collect();
    parse_properties_text_strict(&raw)
}

fn parse_properties_text(raw: &str) -> Vec<(JavaText, JavaText)> {
    parse_properties_text_inner(raw, false).unwrap_or_default()
}

fn parse_properties_text_strict(raw: &str) -> Result<Vec<(JavaText, JavaText)>, ()> {
    parse_properties_text_inner(raw, true)
}

fn parse_properties_text_inner(
    raw: &str,
    strict_unicode: bool,
) -> Result<Vec<(JavaText, JavaText)>, ()> {
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
/// `\'`, `\<space>`, `\:`, `\=`, `\uXXXX`) to Rust text. Unknown escapes
/// degrade to literal characters.
///
/// Lossy form of [`unescape_inner`], for the callers whose destination really
/// is Rust text: this file's own escape-grammar tests and any diagnostic. An
/// *unpaired* `\uXXXX` surrogate reads back here as U+FFFD BY CONSTRUCTION.
/// The `Properties.load` natives do NOT come through here — they take
/// `unescape_inner`'s units and keep the unit exactly as the file wrote it.
/// The former "LIMITATION / CROSS-FILE FOLLOW-UP" note that stood here (a
/// lone surrogate cannot survive a `String`-typed pipeline) is discharged:
/// the side-table stores [`JavaText`], not `String`.
fn unescape(s: &str) -> String {
    unescape_inner(s, false).unwrap_or_default().to_lossy()
}

/// Decode one logical `.properties` field into UTF-16 code units.
///
/// Each `\uXXXX` contributes exactly ONE code unit, which is what the JDK's
/// `Properties.loadConvert` does: it appends the parsed `char` and never looks
/// at the next escape. The previous version had to greedily pair a high escape
/// with the low one after it and fold the two into a single Rust `char`,
/// because a `String` was the only thing it could return — and a `\uD800` with
/// no partner then had nowhere to go and became U+FFFD. Emitting units deletes
/// both the pairing special case and the loss: a well-formed pair is two units
/// that re-encode to the same supplementary character, and an unpaired half is
/// simply the unit the file asked for.
fn unescape_inner(s: &str, strict_unicode: bool) -> Result<JavaText, ()> {
    fn push_char(out: &mut Vec<u16>, c: char) {
        let mut buf = [0u16; 2];
        out.extend_from_slice(c.encode_utf16(&mut buf));
    }
    let mut out: Vec<u16> = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            push_char(&mut out, c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n' as u16),
            Some('t') => out.push(b'\t' as u16),
            Some('r') => out.push(b'\r' as u16),
            Some('f') => out.push(0x000c),
            Some('\\') => out.push(b'\\' as u16),
            Some('"') => out.push(b'"' as u16),
            Some('\'') => out.push(b'\'' as u16),
            Some(' ') => out.push(b' ' as u16),
            Some(':') => out.push(b':' as u16),
            Some('=') => out.push(b'=' as u16),
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
                    out.push(b'u' as u16);
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
                // `read_u_escape` reads at most four hex digits, so `code` is
                // always one code unit and this cast cannot truncate.
                out.push(code as u16);
            }
            Some(other) => push_char(&mut out, other),
            None => break,
        }
    }
    Ok(JavaText::from_units(out))
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

/// Insert (or overwrite) a key/value pair in the side-table for a
/// given Properties object.  Enforces per-object and global caps.
///
/// The global cap is now a *concurrently alive* cap, not a *lifetime* one:
/// hitting it triggers an opportunistic reclaim of already-GC'd entries
/// (`drain_reclaimed`), and if that's not enough, an explicit GC cycle to
/// give truly-dead entries a chance to be discovered before falling back to
/// the (now extremely rare) old silent-drop behavior. See
/// `register_weak_track` / `drain_reclaimed` above.
/// Rust-text adapter for [`put_kv_units`].
///
/// Kept for the callers whose key/value genuinely IS Rust text — the
/// cross-module `&str` API, `System.setProperty` mirroring, diagnostics. A
/// caller that started from a Java `String` must NOT come through here: the
/// `&str` it holds has already lost any unpaired surrogate.
fn put_kv(ctx: &mut dyn NativeContext, obj: ObjectRef, key: &str, value: &str) {
    put_kv_units(ctx, obj, &JavaText::from(key), &JavaText::from(value));
}

fn put_kv_units(ctx: &mut dyn NativeContext, obj: ObjectRef, key: &JavaText, value: &JavaText) {
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
            "[PROPS-DBG] put_kv DROPPED key={} — side-table at capacity ({} objects)",
            key.to_lossy(),
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
            entry.insert(key.clone(), value.clone());
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
    get_kv_units(ctx, obj, &JavaText::from(key)).map(|v| v.to_lossy())
}

fn get_kv_units(ctx: &dyn NativeContext, obj: ObjectRef, key: &JavaText) -> Option<JavaText> {
    let k = key_for(ctx, obj);
    table().lock().get(&k)?.get(key).cloned()
}

/// Remove a key from the side-table.  Returns the previous value if it
/// was present, or `None` if either the object isn't tracked or the key
/// was absent.  Used by `native_properties_remove` to back the JDK
/// `Properties.remove(Object) Object` semantics.
fn remove_kv(ctx: &dyn NativeContext, obj: ObjectRef, key: &str) -> Option<String> {
    remove_kv_units(ctx, obj, &JavaText::from(key)).map(|v| v.to_lossy())
}

fn remove_kv_units(ctx: &dyn NativeContext, obj: ObjectRef, key: &JavaText) -> Option<JavaText> {
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
        m.insert(JavaText::from(key.as_str()), JavaText::from(value.as_str()));
    }
    table().lock().insert(k, m);
}

/// Public snapshot of side-table entries for a given object, used by
/// surefire `setAsSystemProperties` etc. to iterate entries without
/// going through the inner Map field.
pub fn snapshot_sidetable(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<(String, String)> {
    snapshot_kv(ctx, obj)
        .into_iter()
        .map(|(k, v)| (k.to_lossy(), v.to_lossy()))
        .collect()
}

/// Public re-export of `drain_input_stream` for use from `lib.rs`.
pub fn drain_input_stream_pub(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<Vec<u8>> {
    drain_input_stream(ctx, stream)
}

/// Public re-export of `parse_properties` for use from `lib.rs`.
///
/// Rust-text shaped because every caller (`logmanager`, `test_frameworks`)
/// feeds the result straight into `&str` APIs. A `\uD800` with no matching
/// low half degrades to `U+FFFD` here, exactly as it did before the store
/// became units-typed; the `Properties.load` natives in this file take
/// [`parse_properties_units`] instead and keep it.
pub fn parse_properties_pub(bytes: &[u8]) -> Vec<(String, String)> {
    parse_properties(bytes)
        .into_iter()
        .map(|(k, v)| (k.to_lossy(), v.to_lossy()))
        .collect()
}

/// Snapshot the side-table entries for a Properties object.  Returns
/// an empty vector if the object isn't tracked.  Used by `keySet`,
/// `entrySet`, `values`, `keys`, `elements` natives so the iteration
/// view is decoupled from the live mutable side-table.
fn snapshot_kv(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<(JavaText, JavaText)> {
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
fn ordered_snapshot_kv(
    ctx: &mut dyn NativeContext,
    obj: &mut ObjectRef,
) -> Vec<(JavaText, JavaText)> {
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
fn reorder_by(side: &[(JavaText, JavaText)], order: &[JavaText]) -> Vec<(JavaText, JavaText)> {
    let index: FxHashMap<&JavaText, &JavaText> = side.iter().map(|(k, v)| (k, v)).collect();
    let mut out: Vec<(JavaText, JavaText)> = Vec::with_capacity(side.len());
    let mut used: std::collections::HashSet<&JavaText> =
        std::collections::HashSet::with_capacity(side.len());
    for key in order {
        if let Some(value) = index.get(key) {
            if used.insert(key) {
                out.push((key.clone(), (*value).clone()));
            }
        }
    }
    for (k, v) in side {
        if !used.contains(k) {
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
fn chm_key_order(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<JavaText> {
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
fn side_key_set(ctx: &dyn NativeContext, obj: ObjectRef) -> std::collections::HashSet<JavaText> {
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
    skip: &std::collections::HashSet<JavaText>,
) -> Vec<(ObjectRef, Value, Option<JavaText>)> {
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
        key_string: Option<JavaText>,
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
        let kstr = read_java_text(ctx, key_cur);
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
    parsed: &[(JavaText, JavaText)],
) {
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        Value::Object(None) | _ => {
            let Some(m) = (match ctx.new_object("java/util/concurrent/ConcurrentHashMap") {
                Ok(Some(Value::Object(Some(o)))) => Some(o),
                _ => None,
            }) else {
                for (k, v) in parsed {
                    let k_obj = create_property_string(ctx, k);
                    let v_obj = create_property_string(ctx, v);
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
        let k_obj = create_property_string(ctx, k);
        let v_obj = create_property_string(ctx, v);
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
fn store_parsed_entries(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    parsed: &[(JavaText, JavaText)],
) {
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
            put_kv_units(ctx, this_cur, k, v);
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
        let k_obj = create_property_string(ctx, k);
        let k_pin = ctx.pin_native_root(k_obj);
        let v_obj = create_property_string(ctx, v);
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
    // Null-argument contract MEASURED 2026-08-13 (scratchpad/orch/Three.java).
    // The JDK names the PARAMETER here rather than using the helpful-NPE
    // dereference text, so the message cannot be derived -- it is transcribed.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("inStream parameter is null".into()),
        }
        .into());
    }
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
        let k_text = k.to_lossy();
        if k_text.contains("ApplicationContext") || k_text.contains("ContextFactory") {
            let v_text = v.to_lossy();
            let preview: String = v_text.chars().take(80).collect();
            props_diag_eprintln!(
                "[PROPS-DBG] KEY={} VALUE_LEN={} VALUE_START={}",
                k_text,
                v.len(),
                preview
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
    // Null-argument contract MEASURED 2026-08-13 (scratchpad/orch/Three.java).
    // The JDK names the PARAMETER here rather than using the helpful-NPE
    // dereference text, so the message cannot be derived -- it is transcribed.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("reader parameter is null".into()),
        }
        .into());
    }
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
fn chm_get(ctx: &mut dyn NativeContext, this: ObjectRef, key_obj: ObjectRef) -> Option<JavaText> {
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
            read_java_text(ctx, v_cur)
        }
        _ => None,
    };
    // `chm_pin` is the base of every pin taken here, so this releases them all.
    ctx.unpin_native_roots(chm_pin);
    value
}

/// HotSpot's null contract for the `Properties` family, MEASURED 2026-08-13 on
/// 25.0.3+9-LTS (`scratchpad/orch/PropNull.java`). Every lookup entry point
/// (`getProperty`, `getProperty(String,String)`, `get`, `containsKey`,
/// `getOrDefault`) reaches `Hashtable.get`, which dereferences the key for its
/// hash; `setProperty` reaches `Hashtable.put`, whose own null check carries NO
/// message:
///
///   getProperty(null)      !! NPE: Cannot invoke "Object.hashCode()" because "key" is null
///   setProperty(null, "v") !! NPE (getMessage() == null)
///   setProperty("k", null) !! NPE (getMessage() == null)
///
/// Before this, EVERY one of those arms returned a benign default, so all eight
/// entry points silently SUCCEEDED where HotSpot throws -- 8 of 8 divergent,
/// the same total-miss rate the `SecureRandom` sweep found on this axis. A
/// defaulting reader that answers plausibly is invisible to any test that only
/// compares successful answers.
fn props_null_key_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some("Cannot invoke \"Object.hashCode()\" because \"key\" is null".into()),
    }
    .into()
}

fn props_null_map_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(
            "Cannot invoke \"java.util.Map.size()\" because \"m\" is null".into(),
        ),
    }
    .into()
}


fn props_null_put_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

/// The units-preserving twin of `lib.rs`'s `property_key_from_java_string`.
///
/// Same normalisation (`normalize_java_property_key` trims ASCII control
/// characters and NUL from both ends) applied to code units instead of `char`s,
/// so a key whose only difference from another is which unpaired surrogate it
/// carries stays a DIFFERENT key. With the `String` form, two such keys were
/// both `U+FFFD` and the second `setProperty` silently overwrote the first.
///
/// Falls back to the `String` reader when the units path yields nothing: that
/// reader has a second, field-shaped decode for `String` receivers
/// `read_string` refuses, and dropping it here would narrow what `getProperty`
/// accepts.
fn property_key_units(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> JavaText {
    if let Some(text) = read_java_text(ctx, key_obj) {
        let units = text.units();
        let trim = |u: u16| u <= 0x1f || u == 0x7f;
        let mut start = 0usize;
        while start < units.len() && trim(units[start]) {
            start += 1;
        }
        let mut end = units.len();
        while end > start && trim(units[end - 1]) {
            end -= 1;
        }
        if end > start {
            return JavaText::from_units(units[start..end].to_vec());
        }
    }
    JavaText::from(crate::property_key_from_java_string(ctx, key_obj).as_str())
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
        _ => return Err(props_null_key_npe()),
    };
    let key_units = property_key_units(ctx, key_obj);
    // Rust-text form of the SAME key, for the system-property store and the
    // diagnostics — both of which are `&str` APIs.
    let key = key_units.to_lossy();
    if let Some(v) = get_kv_units(ctx, this, &key_units) {
        tracing::debug!(
            target: "cratonvm_vm::props_sidetable",
            ?this, key = %key, units = v.len(),
            "PROPS-GET sidetable hit"
        );
        let s = create_property_string(ctx, &v);
        return Ok(Some(Value::Object(Some(s))));
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
            ?this, key = %key, units = v.len(),
            "PROPS-GET real-backing hit"
        );
        let s = create_property_string(ctx, &v);
        return Ok(Some(Value::Object(Some(s))));
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
        _ => return Err(props_null_put_npe()),
    };
    let val_obj = match args.get(2) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Err(props_null_put_npe()),
    };
    let key = read_java_text(ctx, key_obj).unwrap_or_default();
    let val = read_java_text(ctx, val_obj).unwrap_or_default();
    // The previous value comes from `native_properties_get`, not from
    // `get_kv_units`. The side table is String->String, so reading the return
    // value out of it answered null for exactly the case a caller cares about:
    // the key was mapped to something that is NOT a String, and this call is
    // about to overwrite it. MEASURED: HotSpot 42, CratonVM null
    // (probes/PropertiesShadowSweep 43).
    //
    // GC-SAFETY: `native_properties_get` re-enters Java (the CHM lookup), so
    // the receiver and both argument objects are pinned across it and re-read.
    let this_pin = ctx.pin_native_root(this);
    let key_obj_pin = ctx.pin_native_root(key_obj);
    let old_obj = native_properties_get(
        ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key_obj))],
    )?;
    let old_obj_pin = match old_obj {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let this = ctx.read_native_pin(this_pin, this);
    let _ = ctx.read_native_pin(key_obj_pin, key_obj);
    put_kv_units(ctx, this, &key, &val);
    // Mirror into the real JDK Properties backing (`map` ConcurrentHashMap) so
    // generic Map walkers observe the entry — see native_properties_put's
    // fn-level note for the full rationale (Hibernate's PU-properties merge).
    mirror_loaded_entries_to_properties_backend(ctx, this, &[(key.clone(), val.clone())]);
    // Only the system-properties view propagates to the global store — see
    // `system_props_keys` for why a blanket mirror cross-contaminates. That
    // store is a `&str` API, so this arm alone is lossy; it is the VM's own
    // configuration namespace, whose keys and values are never Java text with
    // an unpaired surrogate in it.
    if is_system_props(ctx, this) {
        let _ = ctx.set_system_property(&key.to_lossy(), &val.to_lossy());
    }
    let answer = match old_obj_pin {
        Some((pin, orig)) => Value::Object(Some(ctx.read_native_pin(pin, orig))),
        None => Value::Object(None),
    };
    ctx.unpin_native_roots(this_pin);
    Ok(Some(answer))
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(2), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
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
    let ks_opt = read_java_text(ctx, k);
    let vs_opt = read_java_text(ctx, v);
    if let (Some(ks), Some(vs)) = (ks_opt.as_ref(), vs_opt.as_ref()) {
        if !ks.is_empty() {
            // String→String: store in side-table AND CHM (existing path).
            let prev = get_kv_units(ctx, this, ks);
            put_kv_units(ctx, this, ks, vs);
            mirror_loaded_entries_to_properties_backend(ctx, this, &[(ks.clone(), vs.clone())]);
            if is_system_props(ctx, this) {
                let _ = ctx.set_system_property(&ks.to_lossy(), &vs.to_lossy());
            }
            return Ok(Some(match prev {
                Some(p) => {
                    let s = create_property_string(ctx, &p);
                    Value::Object(Some(s))
                }
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(2), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    if args.is_empty() {
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    // ... and the FUNCTION's, which that pass did not take.
    // `ConcurrentHashMap.computeIfAbsent` opens
    // `if (key == null || mappingFunction == null) throw new NullPointerException();`
    // -- one line, both arguments. MEASURED no-throw
    // (probes/PropertiesShadowSweep 74).
    if matches!(args.get(2), Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_key_npe());
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = read_java_text(ctx, key_obj).unwrap_or_default();
    if key.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let removed = remove_kv_units(ctx, this, &key);
    // The system-properties view must propagate removal to the global store,
    // mirroring how `setProperty`/`put` propagate writes — otherwise
    // `System.getProperties().remove(k)` (keycloak ExportImportConfig.reset)
    // leaves `System.getProperty(k)` returning the stale value.
    if is_system_props(ctx, this) {
        let _ = ctx.remove_system_property(&key.to_lossy());
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
        Some(prev) => {
            let s = create_property_string(ctx, &prev);
            Ok(Some(Value::Object(Some(s))))
        }
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
        _ => return Err(props_null_key_npe()),
    };
    let key = read_java_text(ctx, key_obj).unwrap_or_default();
    if get_kv_units(ctx, this, &key).is_some() {
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
fn native_properties_get_or_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Properties` extends `Hashtable`, which REJECTS a null key; the generic
    // `native_map_get_or_default` in native-collections must not, because
    // `HashMap` ACCEPTS one. MEASURED 2026-08-13 (`scratchpad/orch/MapNull.java`):
    //   HashMap.getOrDefault(null, d)   = ok
    //   Hashtable.getOrDefault(null, d) !! NullPointerException
    // So this cannot be fixed in the shared native without breaking the other
    // half of the family -- ask what serves the class, per class.
    match args.get(1) {
        Some(Value::Object(Some(_))) => {}
        _ => return Err(props_null_key_npe()),
    }
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    match native_properties_get(ctx, args)? {
        Some(Value::Object(Some(v))) => Ok(Some(Value::Object(Some(v)))),
        // Hashtable stores no null VALUES, so "absent" and "mapped to null"
        // cannot be distinguished here and do not need to be.
        _ => Ok(Some(default)),
    }
}


fn native_properties_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Err(props_null_key_npe()),
    };
    let key_units = property_key_units(ctx, key_obj);
    let key = key_units.to_lossy();
    // Check the Rust side-table (String→String only).
    if let Some(v) = get_kv_units(ctx, this, &key_units) {
        let s = create_property_string(ctx, &v);
        return Ok(Some(Value::Object(Some(s))));
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
    items: Vec<JavaText>,
) -> Result<ObjectRef, MethodCallFailed> {
    let coll = match ctx.new_object(class_name) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return crate::try_alloc_concurrent_synthetic(ctx, class_name, 2),
    };
    let pin = ctx.pin_native_root(coll);
    let _ = ctx.invoke(class_name, "<init>", "()V", &[Value::Object(Some(coll))]);
    for s in &items {
        let so = create_property_string(ctx, s);
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
    Ok(coll)
}

/// Native `LinkedHashSet.retainAll(Collection)Z`.
///
/// **This no longer has a `Properties` arm.** It used to: `Properties.keySet()`
/// handed back a disconnected `LinkedHashSet` snapshot, so `retainAll` on it had
/// to look the source `Properties` up in a side table (keyed by the snapshot's
/// identity hash) and delete the dropped keys from it by hand. Since 2026-08-13
/// `native_properties_key_set` returns a real view — `make_static_key_set`, a
/// keySet carrier whose backing names the source — and the shared
/// `native_hs_retain_all`/`native_hs_remove` write through for every view,
/// `Properties`' included. The bespoke arm, the side table it read and the
/// global root it held per snapshot are all gone.
///
/// What is left is the registration itself, which is load-bearing for ORDINARY
/// `LinkedHashSet`s and must stay: it routes them to
/// `try_native_hashset_remove`/real bytecode rather than to the plain
/// `native_hs_*` body. See `native_linkedhashset_remove` for the `PRESENT`
/// sentinel mismatch that makes the difference, and the record at
/// `fixed-suite-bugs/suppresswarnings-annotation-duplicate-value-bug-20260726.md`.
fn native_linkedhashset_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    ctx.invoke_virtual_bytecode_only(this, "retainAll", "(Ljava/util/Collection;)Z", &args[1..])
}

/// Native `LinkedHashSet.remove(Object)Z`.
///
/// Like `native_linkedhashset_retain_all`, the `Properties.keySet()` arm is
/// gone; what remains is the ordinary-`LinkedHashSet` path, unchanged.
///
/// It must NOT go straight to real bytecode: `HashSet.remove` is
/// `return map.remove(o) == PRESENT;` and this VM's synthetic backing map stores
/// an `Int(1)` sentinel, never JDK `HashSet.PRESENT`, so that identity
/// comparison is always false — the element was removed but `remove()` answered
/// `false`. (`LinkedHashSet` inherits `remove`, so this override is the only
/// registration that sees such a call.)
///
/// javac was the loudest victim: `Annotate.attributeAnnotation` puts an
/// annotation type's elements in a `LinkedHashSet` and reports "duplicate
/// element 'value' in annotation @X" when `members.remove` returns false —
/// making EVERY annotation with a `value` element uncompilable by the in-process
/// compiler that Spring's AOT `TestCompiler` uses.
fn native_linkedhashset_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
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
/// Pin a slice of `Value`s, returning the base handle and one handle per slot.
///
/// A local mirror of `native-collections`' `pin_value_slice`: this crate cannot
/// reach that one, and the alternative -- accumulating raw `ObjectRef`s across
/// a Java re-entry -- is the Family-1 shape both crates have paid for.
fn pin_props_values(ctx: &mut dyn NativeContext, vals: &[Value]) -> (usize, Vec<usize>) {
    let mut base = usize::MAX;
    let mut handles = Vec::with_capacity(vals.len());
    for v in vals {
        let h = match v {
            Value::Object(Some(o)) => ctx.pin_native_root(*o),
            _ => usize::MAX,
        };
        if base == usize::MAX {
            base = h;
        }
        handles.push(h);
    }
    (base, handles)
}

/// The class name of this Properties' first OWN key that is not a `String`, if
/// it has one. Used by `propertyNames()`, whose JDK body casts every key.
fn props_first_non_string_key(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    let side = side_key_set(ctx, this);
    for (key_obj, _value, kstr) in chm_extra_entries(ctx, this, &side) {
        if kstr.is_none() {
            let cid = ctx.class_id_of_object(key_obj);
            return Some(
                ctx.class_name_of_id(cid)
                    .unwrap_or_else(|| "java/lang/Object".to_string())
                    .replace('/', "."),
            );
        }
    }
    None
}

/// The `ClassCastException` `(String) e.getKey()` raises, spelled the way
/// HotSpot spells it.
fn props_key_cast_failure(ctx: &mut dyn NativeContext, cname: &str) -> MethodCallFailed {
    let _ = ctx;
    RuntimeError::ClassCastException {
        message: format!("class {cname} cannot be cast to class java.lang.String"),
    }
    .into()
}

/// [`build_enumeration`] over arbitrary values rather than text.
///
/// `Hashtable.keys()` / `elements()` enumerate OBJECTS; only `propertyNames`
/// and `stringPropertyNames` are text-typed. Keeping the text-typed builder as
/// the common case and this one for the enumerations is what stops a non-String
/// key being silently dropped on its way out.
fn build_enumeration_of(
    ctx: &mut dyn NativeContext,
    items: Vec<Value>,
) -> Result<ObjectRef, MethodCallFailed> {
    let empty = |ctx: &mut dyn NativeContext| -> Result<ObjectRef, MethodCallFailed> {
        crate::try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)
    };
    let vec = match ctx.new_object("java/util/Vector") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return empty(ctx),
    };
    let pin = ctx.pin_native_root(vec);
    let (_, item_pins) = pin_props_values(ctx, &items);
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
    for (i, item) in items.iter().enumerate() {
        let vec = ctx.read_native_pin(pin, vec);
        let cur = match (item, item_pins[i]) {
            (Value::Object(Some(o)), h) if h != usize::MAX => {
                Value::Object(Some(ctx.read_native_pin(h, *o)))
            }
            _ => *item,
        };
        let _ = ctx.invoke_virtual(vec, "add", "(Ljava/lang/Object;)Z", &[cur]);
    }
    let vec = ctx.read_native_pin(pin, vec);
    let result = match ctx.invoke_virtual(vec, "elements", "()Ljava/util/Enumeration;", &[]) {
        Ok(Some(Value::Object(Some(e)))) => e,
        _ => empty(ctx)?,
    };
    ctx.unpin_native_roots(pin);
    Ok(result)
}

fn build_enumeration(
    ctx: &mut dyn NativeContext,
    items: Vec<JavaText>,
) -> Result<ObjectRef, MethodCallFailed> {
    let empty = |ctx: &mut dyn NativeContext| -> Result<ObjectRef, MethodCallFailed> {
        crate::try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)
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
        let so = create_property_string(ctx, s);
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
        _ => empty(ctx)?,
    };
    ctx.unpin_native_roots(pin);
    Ok(result)
}

/// Native `Properties.stringPropertyNames()Ljava/util/Set;` — Surefire
/// `SystemPropertyManager.loadProperties` copies loaded entries into a
/// `ConcurrentHashMap` via `p.stringPropertyNames()` then `p.getProperty(key)`.
/// JDK bytecode walks `entrySet()` on the internal CHM `map` field, but our
/// `Properties.<init>` native skips populating that CHM, so the bytecode
/// would yield an empty set even when `Properties.load` succeeded. Return
/// the side-table keys directly so the fork sees the loaded properties.
///
/// Like `propertyNames()` — and unlike `keySet()` — this MUST also surface the
/// `defaults` chain: the JDK's `stringPropertyNames` calls the same
/// `enumerateStringProperties` walk that recurses into `defaults` first. This
/// native did not, so `new Properties(base).stringPropertyNames()` returned
/// only the child's own names while `propertyNames()` (which does walk, since
/// 2026-07) returned all of them — two accessors of the same chain
/// disagreeing. Measured against HotSpot 25 with `probes/MapLayoutMatrixProbe`
/// on 2026-08-04: `[only-child, shared]` where the real JDK answers
/// `[only-base, only-child, shared]`.
///
/// Two properties this must NOT break, both of which the probe pins:
///
/// * the Map view stays unchanged — `child.size()`, `child.get(k)` and
///   `child.containsKey(k)` must still see only the child's own entries. That
///   is why this collects names rather than copying entries;
/// * only String-keyed AND String-valued entries qualify. That is exactly what
///   `ordered_snapshot_kv` yields (the CHM-exclusive entries `keySet()` adds
///   are the non-String-valued ones), so the chain walk deliberately uses it
///   rather than `collect_own_property_names`, whose extra pass would let a
///   `put("k", Integer)` in a defaults object surface as a string property
///   name.
fn native_properties_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let mut seen: std::collections::HashSet<JavaText> = std::collections::HashSet::new();
    let mut names: Vec<JavaText> = Vec::new();
    // Receiver first, so a shadowed name keeps the receiver's position; a
    // depth cap guards a pathological self-referential `defaults` (the JDK
    // chain is acyclic), mirroring `native_properties_property_names`.
    let mut cur = Some(this);
    let mut depth = 0;
    while let Some(p) = cur {
        if depth > 64 {
            break;
        }
        let mut p_local = p;
        for (k, _v) in ordered_snapshot_kv(ctx, &mut p_local) {
            if seen.insert(k.clone()) {
                names.push(k);
            }
        }
        cur = props_defaults(ctx, p_local);
        depth += 1;
    }
    // Same `LinkedHashSet` this returned before (see `build_key_set` for why
    // that class and not `HashSet`).
    let set = build_string_collection(ctx, "java/util/LinkedHashSet", names);
    Ok(Some(Value::Object(Some(set?))))
}

/// Native `Properties.keySet()Ljava/util/Set;` — returns a **live** keySet view
/// backed by this `Properties`.  Spring's
/// `SpringIterableConfigurationPropertySource` walks this once it
/// recognises the source as enumerable.
///
/// Until 2026-08-13 this built a disconnected `LinkedHashSet` snapshot and
/// bought write-through separately, with a native override on `LinkedHashSet`
/// gated on a side table keyed by the snapshot's identity hash. Reads were
/// never live at all — `probes/MapViewBehaviourProbe` measured
/// `props.keys.afterPut.size` as 2 where HotSpot says 3, because a `keySet()`
/// taken before a `put` never saw it.
///
/// `make_static_key_set` is the standard keySet-view carrier, tagged
/// `VIEW_KIND_KEYSET_STATIC` because `Properties`' keys live half in the Rust
/// side-table and half in the CHM field, so a resync cannot collect them by
/// walking the receiver's fields the way every other map's keySet view does —
/// it re-enters this native instead and adopts the fresh view's backing.
/// Write-through (`remove`/`retainAll`/`iterator().remove()`/`clear`) now comes
/// from the shared `native_hs_*` view machinery.
fn native_properties_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let mut this = this;
    let snapshot = ordered_snapshot_kv(ctx, &mut this);
    // cceres5-style GC safety (mirrors `native_properties_values`): every
    // `create_string` below, and `chm_extra_entries`' re-entry into Java, can
    // move `this` and every key already accumulated in `keys`. Pin each as it
    // is produced and refresh the whole vector before handing it over.
    let this_pin = ctx.pin_native_root(this);
    let mut keys: Vec<Value> = Vec::with_capacity(snapshot.len());
    let mut key_pins: Vec<usize> = Vec::with_capacity(snapshot.len());
    for (k, _v) in &snapshot {
        let ks = create_property_string(ctx, k);
        key_pins.push(ctx.pin_native_root(ks));
        keys.push(Value::Object(Some(ks)));
    }
    // Add keys for CHM-exclusive (non-String-valued) entries so the key view
    // matches the real map; `stringPropertyNames()` deliberately does NOT do
    // this (it is specified to return only String-keyed/String-valued names).
    let this_cur = ctx.read_native_pin(this_pin, this);
    let side = side_key_set(ctx, this_cur);
    let this_cur = ctx.read_native_pin(this_pin, this);
    for (key_obj, _value, kstr) in chm_extra_entries(ctx, this_cur, &side) {
        // String keys are common for Properties; rebuild a fresh Java String
        // after the CHM walk so the key cannot be a stale raw ref from a
        // previous iterator call.
        let k = match kstr {
            Some(s) => create_property_string(ctx, &s),
            None => key_obj,
        };
        key_pins.push(ctx.pin_native_root(k));
        keys.push(Value::Object(Some(k)));
    }
    for (i, pin) in key_pins.iter().enumerate() {
        keys[i] = match keys[i] {
            Value::Object(Some(o)) => Value::Object(Some(ctx.read_native_pin(*pin, o))),
            other => other,
        };
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let set = cratonvm_native_collections::make_static_key_set(ctx, this_cur, &keys)?;
    ctx.unpin_native_roots(this_pin);
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
            let list = crate::try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
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
        let vs = create_property_string(ctx, v);
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
    let list = cratonvm_native_collections::make_live_values_list(ctx, this_cur, &vals)?;
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
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
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
        props_diag_eprintln!("[PROPS-DBG]   entry key={}", k.to_lossy());
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
        let ks = create_property_string(ctx, k);
        let ks_pin = ctx.pin_native_root(ks);
        let vs = create_property_string(ctx, v);
        let vs_pin = ctx.pin_native_root(vs);
        pairs.push((Value::Object(Some(ks)), Value::Object(Some(vs))));
        pair_pins.push((ks_pin, vs_pin));
    }
    let side_keys: std::collections::HashSet<JavaText> =
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
    let set = cratonvm_native_collections::make_static_entry_set(ctx, this_cur, &pairs)?;
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
            )?))))
        }
    };
    let mut this = this;
    let text_keys: Vec<JavaText> = ordered_snapshot_kv(ctx, &mut this)
        .into_iter()
        .map(|(k, _v)| k)
        .collect();
    // `Hashtable.keys()` enumerates the KEY OBJECTS, whatever their class.
    // Filtering to the ones this file can read as text dropped every
    // non-String key while `size()` and `containsKey()` still counted it:
    // MEASURED `[intval, strkey]` against HotSpot's `[7, 8, intval, strkey]`
    // (probes/PropertiesShadowSweep 38). A container that reports four entries
    // and enumerates two is worse than one that reports two.
    let this_pin = ctx.pin_native_root(this);
    let mut items: Vec<Value> = Vec::with_capacity(text_keys.len());
    for k in &text_keys {
        let s = create_property_string(ctx, k);
        items.push(Value::Object(Some(s)));
    }
    let (_, item_pins) = pin_props_values(ctx, &items);
    let this = ctx.read_native_pin(this_pin, this);
    let side = side_key_set(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    for (key_obj, _value, _kstr) in chm_extra_entries(ctx, this, &side) {
        items.push(Value::Object(Some(key_obj)));
    }
    // Everything collected before `chm_extra_entries` needs refreshing: it
    // re-enters Java for the whole entry-set walk. The refs it RETURNS are
    // already current.
    for (i, pin) in item_pins.iter().enumerate() {
        if let Value::Object(Some(o)) = items[i] {
            items[i] = Value::Object(Some(ctx.read_native_pin(*pin, o)));
        }
    }
    let e = build_enumeration_of(ctx, items)?;
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(e))))
}

/// Collect this Properties object's own String keys (side-table + CHM-exclusive
/// non-String-valued entries), de-duplicating into `seen`/`out`. Mirrors the
/// key set `native_properties_keys` exposes for a single object.
fn collect_own_property_names(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    seen: &mut std::collections::HashSet<JavaText>,
    out: &mut Vec<JavaText>,
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
            )?))))
        }
    };
    let mut seen: std::collections::HashSet<JavaText> = std::collections::HashSet::new();
    let mut out: Vec<JavaText> = Vec::new();
    // Walk the receiver and its defaults chain. A depth cap guards against a
    // pathological self-referential `defaults` field (the JDK chain is acyclic).
    let mut cur = Some(this);
    let mut depth = 0;
    while let Some(p) = cur {
        if depth > 64 {
            break;
        }
        // `Properties.enumerate` is `h.put((String) e.getKey(), e.getValue())`,
        // and that cast is the contract, not a formality: a `Properties`
        // holding a non-String key is already outside the class's invariant,
        // and the JDK reports it at the first enumeration rather than handing
        // back a filtered view no later reader can tell from a complete one.
        // MEASURED `ok [intval, strkey]` against HotSpot's
        // `ClassCastException` (probes/PropertiesShadowSweep 39).
        //
        // NOT the same rule as `stringPropertyNames`, which filters BY DESIGN
        // (`enumerateStringProperties` skips a non-String key AND a non-String
        // value) and which this file already gets right.
        if let Some(cname) = props_first_non_string_key(ctx, p) {
            return Err(props_key_cast_failure(ctx, &cname));
        }
        collect_own_property_names(ctx, p, &mut seen, &mut out);
        cur = props_defaults(ctx, p);
        depth += 1;
    }
    Ok(Some(Value::Object(Some(build_enumeration(ctx, out)?))))
}

/// Native `Properties.elements()Ljava/util/Enumeration;` — companion to
/// `keys()`, enumerating the side-table values.
fn native_properties_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vals: Vec<JavaText> = match args.first() {
        Some(Value::Object(Some(o))) => ordered_snapshot_kv(ctx, &mut { *o })
            .into_iter()
            .map(|(_k, v)| v)
            .collect(),
        _ => Vec::new(),
    };
    Ok(Some(Value::Object(Some(build_enumeration(ctx, vals)?))))
}

/// Native `Properties.contains(Object)Z` — Hashtable-style value lookup.
/// JDK 25 forwards to `map.contains(value)`.  Returns true iff the
/// side-table holds a string-equal value for any key.
fn native_properties_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val_obj = match args.get(1) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let needle = read_java_text(ctx, val_obj).unwrap_or_default();
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
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
        let ks = create_property_string(ctx, k);
        let vs = create_property_string(ctx, v);
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[Value::Object(Some(ks)), Value::Object(Some(vs))],
        )?;
    }
    // CHM-exclusive (non-String-valued) entries, with the real value object.
    let side: std::collections::HashSet<JavaText> =
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
    // Both receivers are held across the calls below, and each call is real
    // Java that can collect.
    //
    // MEASURED, and this function is the netty witness for the whole family: a
    // `ParameterizedSslHandlerTest` capture (2026-08-26, 600m heap) named it as
    // the only application-level holder in all sixteen dead-base dereferences
    // of that run —
    //
    //   site="class_id_of" obj="0x2002c100180" moved_to="0x2002ba00260"
    //   was_vacated=true  …  3: native_properties_equals
    //
    // — with `this` reaching `entrySet()` after these two calls had collected
    // under it, which surfaced in Java as
    // `NoSuchMethodError java/lang/Object.entrySet()Ljava/util/Set;` from
    // `JdkSslContext.<init>` (`java.security.Provider extends Properties`).
    let this_pin0 = ctx.pin_native_root(this);
    let other_pin0 = ctx.pin_native_root(other);
    let this_size = int_of(ctx.invoke_virtual(this, "size", "()I", &[]));
    let this = ctx.read_native_pin(this_pin0, this);
    let other = ctx.read_native_pin(other_pin0, other);
    let other_size = int_of(ctx.invoke_virtual(other, "size", "()I", &[]));
    let this = ctx.read_native_pin(this_pin0, this);
    let other = ctx.read_native_pin(other_pin0, other);
    if this_size < 0 || other_size != this_size {
        return Ok(Some(Value::Int(0)));
    }
    let es = match obj_of(ctx.invoke_virtual(this, "entrySet", "()Ljava/util/Set;", &[])) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    // `es` only has to survive `iterator()` — but that call is real Java and
    // allocates, which is the whole of the window.
    let es_pin = ctx.pin_native_root(es);
    let es = ctx.read_native_pin(es_pin, es);
    let it = match obj_of(ctx.invoke_virtual(es, "iterator", "()Ljava/util/Iterator;", &[])) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    // Every handle below is a raw `ObjectRef` held across Java calls that can
    // allocate and collect: `it` and `other` live for the whole loop, `entry`
    // across its own two accessors. Pin the long-lived pair once and re-derive
    // them each iteration; pin `entry` per iteration. See
    // the retired `unpinned-native-locals-audit` write-up.
    let it_pin = ctx.pin_native_root(it);
    let other_pin = ctx.pin_native_root(other);
    let mut it = it;
    let mut other = other;
    loop {
        it = ctx.read_native_pin(it_pin, it);
        if int_of(ctx.invoke_virtual(it, "hasNext", "()Z", &[])) != 1 {
            break;
        }
        it = ctx.read_native_pin(it_pin, it);
        let entry = match obj_of(ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])) {
            Some(o) => o,
            None => return Ok(Some(Value::Int(0))),
        };
        // Per-iteration pins. `entry` was already covered; `key` and `value`
        // are the same shape and were not: `key` is produced by `getKey()` and
        // consumed by `get()` AFTER `getValue()` runs, and `value` is produced
        // by `getValue()` and used as a RECEIVER after `get()` runs. Released
        // at the bottom of the loop so a large map does not grow
        // `native_pin_roots` by three entries per entry; an early return needs
        // no release, because `safe_native_call` truncates the pin stack to
        // its entry floor.
        let iter_pin_base = ctx.pin_native_root(entry);
        let key = ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[])?;
        let key_pin = match key {
            Some(Value::Object(Some(k))) => Some((ctx.pin_native_root(k), k)),
            _ => None,
        };
        let entry = ctx.read_native_pin(iter_pin_base, entry);
        let value = obj_of(ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]));
        let value_pin = value.map(|v| (ctx.pin_native_root(v), v));
        let key_arg = match key_pin {
            Some((pin, k)) => Value::Object(Some(ctx.read_native_pin(pin, k))),
            None => key.clone().unwrap_or(Value::Object(None)),
        };
        other = ctx.read_native_pin(other_pin, other);
        let other_val = obj_of(ctx.invoke_virtual(
            other,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key_arg],
        ));
        match value_pin {
            None => {
                if other_val.is_some() {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Some((pin, v)) => {
                let v = ctx.read_native_pin(pin, v);
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
        ctx.unpin_native_roots(iter_pin_base);
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
    save_convert_units(
        &JavaText::from(s).units().to_vec(),
        escape_space,
        escape_unicode,
    )
}

/// Units-typed body of [`save_convert`].
///
/// The JDK's `saveConvert` walks `char`s — i.e. UTF-16 code units — so this is
/// the shape the algorithm was always written in; the `&str` form above merely
/// re-encodes first. Working on units is also what lets an unpaired surrogate
/// be *written* at all: it has no `char` in Rust to push into an output
/// `String`.
///
/// One deliberate divergence, for content no `&str` can carry: an unpaired
/// surrogate is emitted as `\uXXXX` even when `escape_unicode` is false (the
/// `store(Writer)` overload, where HotSpot writes the raw unit through the
/// Writer's encoder). The escape re-loads to exactly the same unit, so the
/// round-trip this function exists to protect is intact; the alternative is
/// U+FFFD, which is not.
fn save_convert_units(units: &[u16], escape_space: bool, escape_unicode: bool) -> String {
    let mut out = String::with_capacity(units.len() * 2);
    for (i, &unit) in units.iter().enumerate() {
        let c = unit as u32;
        // Fast path: printable ASCII above '=' (61) and below DEL (127).
        if c > 61 && c < 127 {
            if unit == b'\\' as u16 {
                out.push_str("\\\\");
            } else {
                out.push(unit as u8 as char);
            }
            continue;
        }
        match unit {
            0x20 => {
                if i == 0 || escape_space {
                    out.push('\\');
                }
                out.push(' ');
            }
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x0c => out.push_str("\\f"),
            0x3d | 0x3a | 0x23 | 0x21 => {
                out.push('\\');
                out.push(unit as u8 as char);
            }
            _ => {
                let is_surrogate = (0xD800..=0xDFFF).contains(&unit);
                if ((c < 0x20 || c > 0x7e) && escape_unicode) || is_surrogate {
                    // One `\uXXXX` per code unit, so a supplementary code point
                    // round-trips as the surrogate PAIR the JDK writes and a
                    // lone half round-trips as itself.
                    // UPPER-case hex: `Properties.saveConvert` indexes
                    // `hexDigit[]`, which is `'0'..'9','A'..'F'`. `load`
                    // accepts either case, so a round trip cannot see this and
                    // a byte comparison against a JDK-written file can.
                    out.push_str(&format!("\\u{:04X}", unit));
                } else if let Some(ch) = char::from_u32(c) {
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
) -> Vec<(JavaText, JavaText)> {
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
            Ok(Some(Value::Object(Some(k)))) => read_java_text(ctx, k),
            _ => None,
        };
        let v = match val_v {
            Ok(Some(Value::Object(Some(v)))) => read_java_text(ctx, v),
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
fn collect_store_entries(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Vec<(JavaText, JavaText)> {
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
    entries: &[(JavaText, JavaText)],
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
        text.push_str(&save_convert_units(k.units(), true, escape_unicode));
        text.push('=');
        text.push_str(&save_convert_units(v.units(), false, escape_unicode));
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
    // The fallback is the platform default, not LF: an unseeded property here
    // used to make `store` write LF-terminated lines that
    // `split(System.lineSeparator())` then failed to split on Windows.
    let eol = ctx
        .get_system_property("line.separator")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
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
    // Null-argument contract MEASURED 2026-08-13 (scratchpad/orch/Three.java).
    // The JDK names the PARAMETER here rather than using the helpful-NPE
    // dereference text, so the message cannot be derived -- it is transcribed.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: None,
        }
        .into());
    }
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
    // The flush PROPAGATES. `Properties.store(OutputStream, String)` is
    // `store0(new BufferedWriter(new OutputStreamWriter(out, ISO_8859_1)), …)`
    // and `store0` ends in a bare `bw.flush()` under `throws IOException` with
    // no `catch`. On a BufferedWriter that flush IS the byte delivery, so
    // dropping its failure made `store` report success over a file that was
    // never written. W7-57-close-flush-swallow-sweep.md
    let out_cur = ctx.read_native_pin(out_pin, out);
    let flush_res = ctx.invoke_virtual(out_cur, "flush", "()V", &[]);
    ctx.unpin_native_roots(this_pin);
    write_res?;
    flush_res?;
    Ok(None)
}

/// Native `Properties.store(Writer, String)` — same as the OutputStream overload
/// but writes the text straight to the `Writer` (no `\uXXXX` escaping; the
/// Writer's own charset encodes the characters).
fn native_properties_store_writer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null-argument contract MEASURED 2026-08-13 (scratchpad/orch/Three.java).
    // The JDK names the PARAMETER here rather than using the helpful-NPE
    // dereference text, so the message cannot be derived -- it is transcribed.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: None,
        }
        .into());
    }
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
    // The flush PROPAGATES, exactly as in the `OutputStream` overload above:
    // `store(Writer, String)` wraps the writer in a `BufferedWriter` when it
    // is not already one and calls the same `store0`, whose last statement is
    // a bare `bw.flush()` under `throws IOException`.
    // W7-57-close-flush-swallow-sweep.md
    let writer_cur = ctx.read_native_pin(writer_pin, writer);
    let flush_res = ctx.invoke_virtual(writer_cur, "flush", "()V", &[]);
    ctx.unpin_native_roots(this_pin);
    write_res?;
    flush_res?;
    Ok(None)
}

/// # The Properties/Hashtable ownership cluster, mapped (H4-1, 2026-08-20)
///
/// `G88-1` §6 stopped the `Wholesale Bridge over-tagging` row here, on the
/// ground that the cluster is "58 % outside the crate" and that the larger half
/// lives in `native-builtins/src/lib.rs`, which contract §8 forbade editing.
/// **The crate half is real; the file claim is not.** Registrations whose
/// TARGET class is `java/util/Properties` or `java/util/Hashtable`, by file
/// (`grep -rn '"java/util/Properties",' --include=*.rs`, 2026-08-20):
///
/// | file | rows |
/// |---|---:|
/// | `native-builtins/src/properties_sidetable.rs` (this one) | 32 |
/// | `native-builtins/src/deprecated_util.rs` | 6 + the `ht` loop |
/// | `native-builtins/src/deprecated_io_util.rs` | 4 + the `ht` loop |
/// | `native-builtins/src/wildfly_naming.rs` | 3 |
/// | **`native-builtins/src/lib.rs`** | **0** |
/// | `native-collections/src/lib.rs` (`register_properties_natives`) | 43 + 6 |
///
/// The 49 on the `native-collections` side reproduce exactly; the 67 on this
/// side are spread over four files and **none of them is the file the contract
/// named**. Contract §8 was therefore never the blocker for this cluster, and
/// `HANDOFF-20260819.md` §1 says as much in general terms ("It names **one
/// file**, not the crate").
///
/// ## What the blocker actually is
///
/// `System.getProperties()` (registered in `native-builtins/src/lib.rs`) hands
/// back a *synthetic* `Properties` singleton whose inherited `map`
/// `ConcurrentHashMap` is deliberately never populated — see the note below,
/// which records what happened on 2026-07-14 when these registrations were
/// dropped by accident: `InternalError: null property: java.home` out of
/// `java.util.Locale.<clinit>`, i.e. any real-JDK-mode program touching
/// `Locale`. Retagging this registrar `SyntheticStub` reproduces that failure
/// under `--jdk-only` by design, because `register_inner`'s `JdkOnly` arm drops
/// the registration outright.
///
/// So the cluster's entry condition is not a tag but a *producer* migration,
/// and the producers are:
///
/// 1. `System.getProperties()`'s synthetic singleton (`native-builtins/src/lib.rs`);
/// 2. the 3 `try_alloc_concurrent_synthetic(_, "java/util/Properties", n)` and
///    4 `"java/util/Hashtable"` sites outside `native-collections`;
/// 3. `java.security.Provider extends Properties` — `jca/provider_chain.rs`
///    reads `Hashtable.table:[Ljava/util/Hashtable$Entry;` and `Hashtable.count`
///    directly, so the JCA registry is a second consumer of this state;
/// 4. this file's own 4 direct calls into `native-collections`' map natives.
///
/// ## And one edge that runs the other way
///
/// This module's enumeration natives mint their views
/// through `cratonvm_native_collections::make_live_values_list` /
/// `make_static_entry_set`, which pick a carrier via `set_view_carrier_for` —
/// `java/util/Hashtable$KeySet` / `$EntrySet` / `$ValueCollection`. Those three
/// carriers are registered in `native-collections`'
/// `register_{map,set}_view_carrier_natives`, and their `iterator` row mints a
/// `java/util/HashMap$KeyIterator`. **So the Properties cluster reaches into
/// the HashMap cluster's iterator carriers**, which is why the two cannot be
/// sequenced independently. See the cluster note on
/// `register_set_view_carrier_natives`.
///
/// Full map and verification plan:
/// `docs/known-issues/jdk-only/H4-1-the-cluster-that-is-not-a-tag-20260820.md`.
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
    registry.register(
        "java/util/Properties",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_get_or_default,
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
    // THE CONDITIONAL MUTATORS. Three of these were served by
    // `native-collections`' `register_map_conditional_mutators` -- generic
    // bodies that walk HashMap buckets, where a `Properties` keeps nothing --
    // and the other three were not registered at all, so real bytecode edited
    // the `map` CHM mirror while the side table kept the old entry and the two
    // stores disagreed from then on. MEASURED (probes/PropertiesShadowSweep
    // 137, 141, 144-147): `replace` answered null for a present key,
    // `replace(k,old,new)` and `remove(k,v)` answered false for a matching
    // pair, and `computeIfPresent(k, ->null)` left the key behind.
    //
    // All six are written over `get`/`put`/`remove`, which is both how the JDK
    // writes them (as defaults over the map's own three operations) and the
    // only way to keep ONE authority: those three natives already mirror
    // side table and CHM in step.
    registry.register(
        "java/util/Properties",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_replace,
    );
    registry.register(
        "java/util/Properties",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_properties_replace_kvv,
    );
    registry.register(
        "java/util/Properties",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_properties_remove_kv,
    );
    registry.register(
        "java/util/Properties",
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_properties_compute_if_present,
    );
    registry.register(
        "java/util/Properties",
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_properties_compute,
    );
    registry.register(
        "java/util/Properties",
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_properties_merge,
    );
    });
}

/// The three primitives every conditional mutator below is written over, with
/// the receiver and every argument kept rooted across each of them -- all three
/// re-enter Java (the CHM mirror) and are therefore collection points.
struct PropsCell {
    this: ObjectRef,
    key: ObjectRef,
    pin: usize,
    key_pin: usize,
}

impl PropsCell {
    fn open(ctx: &mut dyn NativeContext, args: &[Value]) -> Result<Option<PropsCell>, MethodCallFailed> {
        // `key == null` is an NPE on every one of these: they all delegate to
        // `ConcurrentHashMap`, whose first line it is.
        let key = match args.get(1) {
            Some(Value::Object(Some(k))) => *k,
            _ => return Err(props_null_key_npe()),
        };
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let pin = ctx.pin_native_root(this);
        let key_pin = ctx.pin_native_root(key);
        Ok(Some(PropsCell { this, key, pin, key_pin }))
    }
    fn this(&self, ctx: &mut dyn NativeContext) -> Value {
        Value::Object(Some(ctx.read_native_pin(self.pin, self.this)))
    }
    fn key(&self, ctx: &mut dyn NativeContext) -> Value {
        Value::Object(Some(ctx.read_native_pin(self.key_pin, self.key)))
    }
    fn get(&self, ctx: &mut dyn NativeContext) -> Result<Value, MethodCallFailed> {
        let t = self.this(ctx);
        let k = self.key(ctx);
        Ok(native_properties_get(ctx, &[t, k])?.unwrap_or(Value::Object(None)))
    }
    fn put(&self, ctx: &mut dyn NativeContext, v: Value) -> Result<(), MethodCallFailed> {
        let t = self.this(ctx);
        let k = self.key(ctx);
        native_properties_put(ctx, &[t, k, v])?;
        Ok(())
    }
    fn remove(&self, ctx: &mut dyn NativeContext) -> Result<(), MethodCallFailed> {
        let t = self.this(ctx);
        let k = self.key(ctx);
        native_properties_remove(ctx, &[t, k])?;
        Ok(())
    }
    fn close(self, ctx: &mut dyn NativeContext) {
        ctx.unpin_native_roots(self.pin);
    }
}

/// `Properties.replace(k, v)` — `map.replace`, i.e. only when present, and NPE
/// on a null value as well as a null key.
fn native_properties_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if matches!(args.get(2), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_pin = match new_val {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        if matches!(current, Value::Object(None)) {
            return Ok(Some(Value::Object(None)));
        }
        let current_pin = match current {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let nv = match new_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => new_val,
        };
        cell.put(ctx, nv)?;
        let answer = match current_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => current,
        };
        Ok(Some(answer))
    })();
    cell.close(ctx);
    result
}

/// `Properties.replace(k, expected, v)` — all three arguments are checked for
/// null before anything happens (`ConcurrentHashMap.replace`'s first line).
fn native_properties_replace_kvv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if matches!(args.get(2), None | Some(Value::Object(None)))
        || matches!(args.get(3), None | Some(Value::Object(None)))
    {
        return Err(props_null_put_npe());
    }
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    let exp_pin = match expected {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let new_pin = match new_val {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        let exp = match exp_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => expected,
        };
        if !props_values_equal(ctx, current, exp)? {
            return Ok(Some(Value::Int(0)));
        }
        let nv = match new_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => new_val,
        };
        cell.put(ctx, nv)?;
        Ok(Some(Value::Int(1)))
    })();
    cell.close(ctx);
    result
}

/// `Properties.remove(k, v)` — a null VALUE is not an error here, it is simply
/// "no match": `ConcurrentHashMap.remove(k,v)` is
/// `if (key == null) throw NPE; return value != null && ...`.
fn native_properties_remove_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = args.get(2).copied().unwrap_or(Value::Object(None));
    if matches!(expected, Value::Object(None)) {
        cell.close(ctx);
        return Ok(Some(Value::Int(0)));
    }
    let exp_pin = match expected {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        let exp = match exp_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => expected,
        };
        if !props_values_equal(ctx, current, exp)? {
            return Ok(Some(Value::Int(0)));
        }
        cell.remove(ctx)?;
        Ok(Some(Value::Int(1)))
    })();
    cell.close(ctx);
    result
}

/// `Properties.computeIfPresent(k, f)` — a null result REMOVES the entry.
fn native_properties_compute_if_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if matches!(args.get(2), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let f = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            cell.close(ctx);
            return Ok(Some(Value::Object(None)));
        }
    };
    let f_pin = ctx.pin_native_root(f);
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        if matches!(current, Value::Object(None)) {
            return Ok(Some(Value::Object(None)));
        }
        let cur_pin = match current {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let f_cur = ctx.read_native_pin(f_pin, f);
        let k = cell.key(ctx);
        let cur = match cur_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => current,
        };
        let produced = ctx
            .invoke_virtual(
                f_cur,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[k, cur],
            )?
            .unwrap_or(Value::Object(None));
        props_store_or_remove(ctx, &cell, produced)
    })();
    cell.close(ctx);
    result
}

/// `Properties.compute(k, f)` — the mapper sees `null` for an absent key, and a
/// null result removes (or leaves absent).
fn native_properties_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if matches!(args.get(2), None | Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let f = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            cell.close(ctx);
            return Ok(Some(Value::Object(None)));
        }
    };
    let f_pin = ctx.pin_native_root(f);
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        let cur_pin = match current {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let f_cur = ctx.read_native_pin(f_pin, f);
        let k = cell.key(ctx);
        let cur = match cur_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => current,
        };
        let produced = ctx
            .invoke_virtual(
                f_cur,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[k, cur],
            )?
            .unwrap_or(Value::Object(None));
        props_store_or_remove(ctx, &cell, produced)
    })();
    cell.close(ctx);
    result
}

/// `Properties.merge(k, v, f)` — `v` is stored as-is when the key is absent and
/// the remapping function is not called at all.
fn native_properties_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if matches!(args.get(2), None | Some(Value::Object(None)))
        || matches!(args.get(3), None | Some(Value::Object(None)))
    {
        return Err(props_null_put_npe());
    }
    let cell = match PropsCell::open(ctx, args)? {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let supplied = args.get(2).copied().unwrap_or(Value::Object(None));
    let f = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            cell.close(ctx);
            return Ok(Some(Value::Object(None)));
        }
    };
    let sup_pin = match supplied {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let f_pin = ctx.pin_native_root(f);
    let result = (|| -> MethodCallResult {
        let current = cell.get(ctx)?;
        let sup = match sup_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => supplied,
        };
        if matches!(current, Value::Object(None)) {
            cell.put(ctx, sup)?;
            let answer = match sup_pin {
                Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
                None => supplied,
            };
            return Ok(Some(answer));
        }
        let cur_pin = match current {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let f_cur = ctx.read_native_pin(f_pin, f);
        let cur = match cur_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => current,
        };
        let sup = match sup_pin {
            Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
            None => supplied,
        };
        let produced = ctx
            .invoke_virtual(
                f_cur,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[cur, sup],
            )?
            .unwrap_or(Value::Object(None));
        props_store_or_remove(ctx, &cell, produced)
    })();
    cell.close(ctx);
    result
}

/// The shared tail of `compute`/`computeIfPresent`/`merge`: a null result is a
/// REMOVAL, anything else is stored, and the stored value is what is returned.
fn props_store_or_remove(
    ctx: &mut dyn NativeContext,
    cell: &PropsCell,
    produced: Value,
) -> MethodCallResult {
    if matches!(produced, Value::Object(None)) {
        cell.remove(ctx)?;
        return Ok(Some(Value::Object(None)));
    }
    let pin = match produced {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let v = match pin {
        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        None => produced,
    };
    cell.put(ctx, v)?;
    let answer = match pin {
        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        None => produced,
    };
    Ok(Some(answer))
}

/// `Objects.equals` over two `Value`s, dispatching the real `equals`.
fn props_values_equal(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(true),
        (Value::Object(None), _) | (_, Value::Object(None)) => Ok(false),
        (Value::Object(Some(ao)), bv) => {
            let r = ctx.invoke_virtual(ao, "equals", "(Ljava/lang/Object;)Z", &[bv])?;
            Ok(matches!(r, Some(Value::Int(1))))
        }
        (av, bv) => Ok(av == bv),
    }
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
    // Null contract MEASURED 2026-08-13 (scratchpad/orch/PropAxis.java).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(props_null_map_npe());
    }
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
            put_kv_units(ctx, this, k, v);
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
    let mut str_collected: Vec<(JavaText, JavaText)> = Vec::new();
    // Use `invoke_virtual` (dispatch on the receiver's actual runtime class),
    // NOT `invoke` (which resolves against the literal interface/class name
    // passed in). `invoke`'s C25 interface retarget correctly redirects e.g.
    // `java/util/Iterator` onto the receiver's concrete class (verified via
    // `is_subclass_of`), but the dispatch that follows only prefers a
    // registered native over inherited bytecode for synthetic-stub/interface
    // classes (`prefer_exact_class_native`). Since 2026-08-13, `HashMap$
    // EntryIterator`/`KeyIterator` are minted as the REAL declared JDK class
    // (see `alloc_key_itr`), which has real bytecode and so no longer counts
    // as a stub — `invoke`'s dispatch then finds that real `HashIterator`
    // bytecode instead of `native_map_key_itr_has_next`/`_next`, and that
    // bytecode reads `next`/`current`/`index` fields this VM's snapshot
    // iterator never populates, so `hasNext()` silently reports `false` on
    // a non-empty iterator. This was invisible for a `Properties`-shaped
    // source (which never reaches this generic-Map fallback at all) and for
    // ordinary bytecode `invokeinterface` (whose interpreter loop checks the
    // native registry before falling back to inherited bytecode, unlike this
    // native-initiated entry point) — only a native calling through `invoke`
    // with an interface-typed class name hit it. Concretely: `Properties.
    // putAll(new HashMap<>(Map.of("k", "v")))` silently dropped every entry,
    // which is what broke Hibernate's `foreign` id-generator `@Parameter`
    // reading (`GeneratorParameters.collectParameters`'s `params.putAll
    // (configuration)`, `configuration` being a plain `HashMap` built from
    // the `@Parameter` array) — `ForeignGenerator.configure()` then saw no
    // "property" entry and threw `MappingException: param named "property"
    // is required for foreign id generation strategy`.
    let entries_obj = match ctx.invoke_virtual(other, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    this = ctx.read_native_pin(this_pin, this);
    let it = match ctx.invoke_virtual(entries_obj, "iterator", "()Ljava/util/Iterator;", &[]) {
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
        let has_next = match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) => n != 0,
            _ => false,
        };
        it = ctx.read_native_pin(it_pin, it);
        this = ctx.read_native_pin(this_pin, this);
        if !has_next {
            break;
        }
        let entry = match ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        it = ctx.read_native_pin(it_pin, it);
        let entry_pin = ctx.pin_native_root(entry);
        let key_obj = match ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let entry = ctx.read_native_pin(entry_pin, entry);
        let key_pin = ctx.pin_native_root(key_obj);
        let val_v = match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let key_obj = ctx.read_native_pin(key_pin, key_obj);
        it = ctx.read_native_pin(it_pin, it);
        this = ctx.read_native_pin(this_pin, this);
        let k_opt = read_java_text(ctx, key_obj);
        let v_str = match val_v {
            Value::Object(Some(val_obj)) => read_java_text(ctx, val_obj),
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
        put_kv_units(ctx, this, k, v);
    }
    mirror_loaded_entries_to_properties_backend(ctx, this, &str_collected);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::FieldMetadata;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use cratonvm_types::ClassId;

    // -----------------------------------------------------------------------
    // G49-1 — the second-largest coercion cluster, and why it is benign.
    //
    // MEASURED, 19 vectors under `CRATONVM_DBG_COERCION=1` on
    // `target-rel3/release/cratonvm.exe`: 337 `primitive-into-reference` events
    // at `props_defaults`, descriptor `L`, and every single one is a READ
    // (frame `VmHeap::get_field_as`) carrying `value=Int(0)`. `Int(0)` is what
    // a never-written slot decodes to under `gen_heap.rs`'s R-niche rule —
    // `java.util.Properties.defaults` on a `Properties` built without a parent.
    // The coercion answers `null`, which is what the field means, and a
    // 22-check differential against HotSpot 25.0.3+9-LTS (including the
    // non-String-own-value fall-through and a three-level chain) is identical.
    //
    // So the cluster is noise, not damage — but it is only noise while
    // `props_defaults` treats a primitive as "no defaults". These pin that,
    // and pin that it still finds a real parent when there is one, so nobody
    // can close the noise by making the function refuse.
    // -----------------------------------------------------------------------

    /// Real-JDK flat slot of `java.util.Properties.defaults`: `Hashtable`
    /// contributes `table`/`count`/`threshold`/`loadFactor`/`modCount`/
    /// `keySet`/`entrySet`/`values` first. Cross-checked against
    /// `javap -p java.util.Hashtable` on HotSpot 25.0.3+9-LTS and against
    /// `native-collections`' own `define_field(PROPERTIES_CID, "defaults", 8)`.
    const PROPERTIES_DEFAULTS_SLOT: usize = 8;

    fn properties_class(ctx: &mut MockNativeContext) -> ClassId {
        let cid = ctx
            .ensure_class_initialized("java/util/Properties")
            .expect("mock could not initialize java/util/Properties");
        ctx.set_declared_fields(
            cid,
            vec![FieldMetadata {
                name: "defaults".to_string(),
                descriptor: "Ljava/util/Properties;".to_string(),
                access_flags: 0,
                slot_index: PROPERTIES_DEFAULTS_SLOT,
                declaring_class_id: cid,
                is_static: false,
            }],
        );
        cid
    }

    /// **The 245/95-event shape.** A `Properties` with no parent: the slot was
    /// never written, so it reads back as the raw `Int(0)` the allocator left,
    /// the descriptor-aware read coerces it to `null`, and the answer is "no
    /// defaults". Correct, and the reason G49-1 files this cluster benign.
    #[test]
    fn a_never_written_defaults_slot_means_no_defaults_not_a_lost_parent() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let props = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        assert_eq!(
            ctx.get_field(props, PROPERTIES_DEFAULTS_SLOT),
            Value::Int(0),
            "fixture must reproduce the never-initialised slot, not a null"
        );
        assert_eq!(props_defaults(&ctx, props), None);
    }

    /// The other half, so the test above cannot be satisfied by a function that
    /// always answers `None` — which would silently break
    /// `getProperty`'s fall-through chain and is exactly the failure G45-1
    /// suspected this cluster of being.
    #[test]
    fn a_real_parent_is_found_so_the_fall_through_chain_survives() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let parent = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        let child = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        ctx.set_field(child, PROPERTIES_DEFAULTS_SLOT, Value::Object(Some(parent)));
        assert_eq!(props_defaults(&ctx, child), Some(parent));
    }

    /// An explicit `null` parent — what a `putfield` of `null` leaves — is
    /// indistinguishable from "no parent", and must not be mistaken for one.
    #[test]
    fn an_explicitly_null_defaults_slot_is_also_no_defaults() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let props = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        ctx.set_field(props, PROPERTIES_DEFAULTS_SLOT, Value::Object(None));
        assert_eq!(props_defaults(&ctx, props), None);
    }

    /// When the field cannot be resolved at all the function answers `None`
    /// rather than reading slot 0 — which on the real flat layout is
    /// `Hashtable.table`, an `Entry[]`, and would hand `getProperty` an array
    /// to recurse into.
    #[test]
    fn an_unresolvable_defaults_field_answers_none_rather_than_slot_zero() {
        let mut ctx = mock_ctx();
        let props = ctx.alloc_object(ClassId::new(0), PROPERTIES_DEFAULTS_SLOT + 2);
        let table = ctx.alloc_object(ClassId::new(0), 1);
        ctx.set_field(props, 0, Value::Object(Some(table)));
        assert_eq!(props_defaults(&ctx, props), None);
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(JavaText, JavaText)> {
        pairs
            .iter()
            .map(|(k, v)| (JavaText::from(*k), JavaText::from(*v)))
            .collect()
    }

    fn names(items: &[&str]) -> Vec<JavaText> {
        items.iter().map(|s| JavaText::from(*s)).collect()
    }

    /// Rust-text view of a parse result, so the escape-grammar tests below can
    /// keep asserting plain `&str` expectations. Anything ABOUT surrogates must
    /// assert units instead — see `parse_keeps_a_lone_surrogate_escape_as_its_unit`.
    fn lossy(pairs: Vec<(JavaText, JavaText)>) -> Vec<(String, String)> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_lossy(), v.to_lossy()))
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
        let order = names(&["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]);
        let reordered = reorder_by(&side, &order);
        let got: Vec<String> = reordered.iter().map(|(k, _v)| k.to_lossy()).collect();
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
        let order = names(&["z", "c", "a"]);
        let got = reorder_by(&side, &order);
        assert_eq!(got, kv(&[("c", "3"), ("a", "1"), ("b", "2")]));
        assert_eq!(got.len(), side.len());
    }

    #[test]
    fn reorder_tolerates_a_duplicated_order_key() {
        let side = kv(&[("a", "1"), ("b", "2")]);
        let order = names(&["b", "b", "a"]);
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
            m.insert(JavaText::from(k), JavaText::from("v"));
        }
        let keys: Vec<String> = m.keys().map(|k| k.to_lossy()).collect();
        assert_eq!(
            keys,
            vec!["branch", "commit.id", "commit.id.abbrev", "commit.id.full"]
        );
        // `shift_remove` (what `remove_kv` uses) keeps the survivors in order;
        // `swap_remove` would teleport the last key into the hole.
        m.shift_remove(&JavaText::from("commit.id"));
        let keys: Vec<String> = m.keys().map(|k| k.to_lossy()).collect();
        assert_eq!(keys, vec!["branch", "commit.id.abbrev", "commit.id.full"]);
        // Re-inserting an existing key must NOT move it to the back.
        m.insert(JavaText::from("branch"), JavaText::from("other"));
        assert_eq!(
            m.keys().next().map(|k| k.to_lossy()).as_deref(),
            Some("branch")
        );
    }

    /// `reorder_by` must not depend on the side-table's own iteration order for
    /// any key the CHM names — that is the whole point of deferring to the CHM,
    /// and it is what makes the JDK-order guarantee independent of however the
    /// side-table happens to be stored. Feed the same entries in two different
    /// orders and require the same answer.
    #[test]
    fn reorder_is_independent_of_side_table_order_for_chm_named_keys() {
        let order = names(&["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]);
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
        let p = lossy(parse_properties(b"key=value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_multiple_kv() {
        let p = lossy(parse_properties(b"a=1\nb=2\nc=3\n"));
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
        let p = lossy(parse_properties(b"#comment\n!exclamation\nkey=value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_blank_lines_skipped() {
        let p = lossy(parse_properties(b"\n\nkey=value\n\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_whitespace_separator() {
        let p = lossy(parse_properties(b"key value\n"));
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
        // escape_unicode=true escapes non-Latin chars as one `\u` per code unit,
        // in UPPER-case hex: `Properties.saveConvert` indexes a `hexDigit[]` of
        // `'0'..'9','A'..'F'`. These three assertions were written from THIS
        // implementation rather than from the spec and pinned the lower-case
        // form for as long as it was wrong; `load` accepts either case, so
        // nothing but a byte comparison against a JDK-written file could see it
        // (MEASURED, probes/PropertiesShadowSweep 118-119).
        assert_eq!(save_convert("\u{00e9}", false, true), "\\u00E9"); // é
                                                                      // Supplementary code point -> surrogate pair (two \u units).
        assert_eq!(save_convert("\u{1F600}", false, true), "\\uD83D\\uDE00");
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
        let parsed = lossy(parse_properties(text.as_bytes()));
        let expected: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn render_store_text_includes_date_before_first_entry() {
        let entries = kv(&[("code2", "message2"), ("code1", "message1")]);
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
        let entries = kv(&[("key", "value")]);
        let text = render_store_text(Some("header"), Some("DATE"), &entries, true, "\r\n");
        assert_eq!(text, "#header\r\n#DATE\r\nkey=value\r\n");
    }

    #[test]
    fn parse_colon_separator() {
        let p = lossy(parse_properties(b"key:value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_continuation_line() {
        let p = lossy(parse_properties(b"key=long\\\n    value\n"));
        assert_eq!(p, vec![("key".to_string(), "longvalue".to_string())]);
    }

    #[test]
    fn parse_unicode_escape() {
        let p = lossy(parse_properties(b"key=\\u00e9\n"));
        assert_eq!(p[0].0, "key");
        assert!(p[0].1.starts_with('\u{00e9}'));
    }

    #[test]
    fn parse_keycloak_version_shape() {
        let bytes = b"version=26.2.4\nbuild-time=2025-04-26T13:00:00Z\nresources-version=26.2.4\n";
        let p = lossy(parse_properties(bytes));
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
        let p = lossy(parse_properties(b"keyonly\n"));
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
        // `unescape` is the LOSSY Rust-text view, so U+FFFD is the right answer
        // HERE and only here: a Rust `String` cannot hold the unit. The store
        // keeps the unit — `unescape_units_keeps_a_lone_surrogate_as_itself`
        // below is the assertion that matters, and it is written in units
        // precisely because this one passes on the broken code too.
        assert_eq!(unescape("\\uD800"), "\u{FFFD}"); // lone high
        assert_eq!(unescape("\\uDC00"), "\u{FFFD}"); // lone low
                                                     // High surrogate followed by a NON-low escape: the high is replaced,
                                                     // and the trailing 'A' (A) is preserved.
        assert_eq!(unescape("\\uD800\\u0041"), "\u{FFFD}A");
    }

    // -----------------------------------------------------------------------
    // G55-1 — the surrogate the store could not hold, and the key it could not
    // find again. MEASURED against Adoptium 25.0.3+9-hotspot on 2026-08-17
    // (`scratchpad/g55/G55Probe.java`, 158 rows): 21 `Properties` rows diverged,
    // and they were two distinct defects with one cause — a `String`-typed
    // store.
    //
    // Every assertion below is written in UTF-16 UNITS. A test written through
    // `unescape`/`read_string`/`to_lossy` PASSES ON THE BROKEN CODE, which is
    // how this survived: `U+FFFD` is a perfectly good `char`, so a decoded
    // comparison of two mangled strings agrees with itself.
    // -----------------------------------------------------------------------

    const HI: u16 = 0xD800;
    const LO: u16 = 0xDC00;

    #[test]
    fn unescape_units_keeps_a_lone_surrogate_as_itself() {
        assert_eq!(
            unescape_inner("\\uD800", false).unwrap().units(),
            &[HI],
            "a lone high surrogate escape is ONE code unit, not U+FFFD"
        );
        assert_eq!(unescape_inner("\\uDC00", false).unwrap().units(), &[LO]);
        assert_eq!(
            unescape_inner("a\\uD800b", false).unwrap().units(),
            &[b'a' as u16, HI, b'b' as u16]
        );
        // A well-formed pair stays two units — and re-encodes to the single
        // supplementary character, which is what the lossy view still shows.
        let pair = unescape_inner("\\uD83D\\uDE00", false).unwrap();
        assert_eq!(pair.units(), &[0xD83D, 0xDE00]);
        assert_eq!(pair.to_lossy(), "\u{1F600}");
        // A high surrogate followed by a NON-low escape keeps both units.
        assert_eq!(
            unescape_inner("\\uD800\\u0041", false).unwrap().units(),
            &[HI, 0x41]
        );
    }

    #[test]
    fn parse_keeps_a_lone_surrogate_escape_as_its_unit() {
        let parsed = parse_properties(b"k\\ud800=v\\udc00\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.units(), &[b'k' as u16, HI]);
        assert_eq!(parsed[0].1.units(), &[b'v' as u16, LO]);
    }

    /// **The silent one.** Two keys that differ only in WHICH unpaired
    /// surrogate they carry are two different Java keys — `equals` is false and
    /// the hashes differ. Under the `String` store both decoded to `U+FFFD`, so
    /// the second `setProperty` overwrote the first: MEASURED `size()` = 1 where
    /// HotSpot says 2, and `getProperty(HI)` answered the LO key's value. No
    /// exception, no wrong-looking string — just an entry that was never there.
    #[test]
    fn two_keys_differing_only_in_their_unpaired_surrogate_stay_two_keys() {
        let hi = JavaText::from_units(vec![HI]);
        let lo = JavaText::from_units(vec![LO]);
        assert_ne!(hi, lo, "distinct code units are distinct keys");
        assert_eq!(
            hi.to_lossy(),
            lo.to_lossy(),
            "...and the lossy views are EQUAL, which is exactly why a \
             `String`-keyed store conflated them"
        );

        let mut m = PropsMap::default();
        m.insert(hi.clone(), JavaText::from("high"));
        m.insert(lo.clone(), JavaText::from("low"));
        assert_eq!(m.len(), 2, "two keys, two entries");
        assert_eq!(m.get(&hi).map(|v| v.to_lossy()).as_deref(), Some("high"));
        assert_eq!(m.get(&lo).map(|v| v.to_lossy()).as_deref(), Some("low"));
        assert_eq!(
            m.shift_remove(&hi).map(|v| v.to_lossy()).as_deref(),
            Some("high"),
            "removing one must not take the other with it"
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&lo).map(|v| v.to_lossy()).as_deref(), Some("low"));
    }

    /// `reorder_by` indexes the side-table by key. With `&str` keys the same
    /// conflation reached the enumeration order — one key would have shadowed
    /// the other's position and the survivor would have been emitted twice.
    #[test]
    fn reorder_does_not_conflate_two_lone_surrogate_keys() {
        let hi = JavaText::from_units(vec![HI]);
        let lo = JavaText::from_units(vec![LO]);
        let side = vec![
            (hi.clone(), JavaText::from("high")),
            (lo.clone(), JavaText::from("low")),
        ];
        let got = reorder_by(&side, &[lo.clone(), hi.clone()]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.units(), &[LO]);
        assert_eq!(got[0].1.to_lossy(), "low");
        assert_eq!(got[1].0.units(), &[HI]);
        assert_eq!(got[1].1.to_lossy(), "high");
    }

    /// `JavaText` orders by code UNIT, which is what `String.compareTo` does.
    /// Rust's `String` orders by code POINT, which puts a supplementary
    /// character AFTER `U+FFFF` where Java puts it before — so a `String`-keyed
    /// sorted view of the same content is in a different order than the JDK's.
    /// MEASURED on both VMs (`scratchpad/g55/G55Ord.java`).
    #[test]
    fn java_text_orders_by_code_unit_where_a_rust_string_orders_by_code_point() {
        let supplementary = JavaText::from("\u{10000}"); // == [D800, DC00]
        let ffff = JavaText::from_units(vec![0xFFFF]);
        assert_eq!(supplementary.units(), &[0xD800, 0xDC00]);
        assert!(
            supplementary < ffff,
            "Java: a surrogate unit (D800) is below U+FFFF, so the \
             supplementary character sorts FIRST"
        );
        assert!(
            "\u{10000}".to_string() > "\u{FFFF}".to_string(),
            "Rust `String` sorts it LAST — the divergence this type removes"
        );
    }

    #[test]
    fn save_convert_units_writes_a_lone_surrogate_as_a_reloadable_escape() {
        // `escape_unicode = false` is the `store(Writer)` overload, where an
        // ordinary non-ASCII char is written literally — but a lone surrogate
        // still has to be escaped, because there is no `char` to write.
        assert_eq!(save_convert_units(&[HI], false, false), "\\uD800");
        assert_eq!(save_convert_units(&[LO], false, true), "\\uDC00");
        // Round-trip: what `store` writes, `load` reads back as the same units.
        let mut text = String::new();
        text.push_str(&save_convert_units(&[b'k' as u16, HI], true, true));
        text.push('=');
        text.push_str(&save_convert_units(&[b'v' as u16, LO], false, true));
        text.push('\n');
        let parsed = parse_properties(text.as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.units(), &[b'k' as u16, HI]);
        assert_eq!(parsed[0].1.units(), &[b'v' as u16, LO]);
    }

    #[test]
    fn save_convert_still_writes_a_well_formed_pair_as_two_escapes() {
        // The supplementary path must not change: two `\u` escapes, not one.
        assert_eq!(
            save_convert_units(&[0xD83D, 0xDE00], false, true),
            "\\uD83D\\uDE00"
        );
    }

    /// The writer half. `create_property_string` sends well-formed text down
    /// the unchanged `create_string` path and only reroutes what a `&str`
    /// cannot carry — so this asserts the reroute actually preserves the unit,
    /// read back through `lang_string::read_string_chars` rather than through
    /// any `String`.
    #[test]
    fn create_property_string_materialises_an_unpaired_surrogate_intact() {
        let mut ctx = mock_ctx();
        let value = JavaText::from_units(vec![b'v' as u16, b'a' as u16, LO, b'b' as u16]);
        let obj = create_property_string(&mut ctx, &value);
        assert_eq!(
            crate::lang_string::read_string_chars(&ctx, obj),
            vec![b'v' as u16, b'a' as u16, LO, b'b' as u16],
            "the value must come back with its unpaired low surrogate intact"
        );
    }

    #[test]
    fn create_property_string_leaves_well_formed_text_on_the_unchanged_path() {
        let mut ctx = mock_ctx();
        let obj = create_property_string(&mut ctx, &JavaText::from("v\u{1F600}b"));
        assert_eq!(
            crate::lang_string::read_string_chars(&ctx, obj),
            vec![b'v' as u16, 0xD83D, 0xDE00, b'b' as u16]
        );
        // Well-formed content stays on `ctx.create_string`, so whatever
        // identity/interning behaviour the green `Properties` vectors already
        // measured is unchanged — the reroute is reached only by content a
        // `&str` cannot carry.
        assert!(!crate::lang_string::has_unpaired_surrogate(
            JavaText::from("v\u{1F600}b").units()
        ));
    }

    /// The reader half, on content a `&str` CAN carry — the mock's
    /// `read_string` is stricter than the VM's (it refuses an unpaired
    /// surrogate outright where the VM substitutes U+FFFD), so the guard, not
    /// the decode, is what this pins.
    #[test]
    fn read_java_text_reads_units_and_refuses_a_non_string() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("k\u{00e9}");
        assert_eq!(
            read_java_text(&ctx, s).map(|t| t.units().to_vec()),
            Some(vec![b'k' as u16, 0x00e9])
        );
        let not_a_string = ctx.alloc_object(ClassId::new(0), 2);
        assert_eq!(read_java_text(&ctx, not_a_string), None);
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
        let parsed = lossy(parse_properties(&iso_8859_1_bytes("name=café\n")));
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
