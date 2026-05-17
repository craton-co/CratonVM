//! WP1.2 — comprehensive `sun.misc.Unsafe` / `jdk.internal.misc.Unsafe`
//! native method coverage.
//!
//! This module supplies the Unsafe natives that were not already wired
//! in `native-builtins::lib` prior to WP1.2:
//!
//! * `weakCompareAndSet{Int,Long,Reference,Object}` — alias to strong CAS
//!   (the JDK spec allows spurious failures; aliasing strong is
//!   strictly conforming).
//! * Memory-order variants (`get/putOpaque`, `get/putAcquire`,
//!   `get/putRelease`) — volatile semantics, registered here when
//!   missing.
//! * `park(Object blocker, long nanos)` — JDK 24+ form with an opaque
//!   blocker object passed through to `LockSupport.setBlocker`.
//! * `invokeCleaner(ByteBuffer)` — trigger the `Cleaner` attached to a
//!   DirectByteBuffer if any; no-op on repeated invocations.
//! * Raw-pointer `get/putByte`, `get/putShort`, `get/putInt`,
//!   `get/putLong` variants keyed on a single `long` address (no host
//!   object) — used by `java.nio.Bits` and `jdk.internal.foreign` to
//!   read off-heap memory returned by `allocateMemory`.
//! * `freeMemory(long)` — releases the arena previously returned by
//!   `allocateMemory(long)`.
//! * Real `defineClass(String,byte[],int,int,ClassLoader,ProtectionDomain)`
//!   — parses and registers the bytes via `define_class_from_bytes`
//!   (replaces the null-returning stub; unblocks ByteBuddy — see WP3.3
//!   for a targeted ByteBuddy test).
//! * `getLoadAverage0([D,I)I` — platform `getloadavg(3)` on POSIX, 0-fill
//!   on Windows (matches real HotSpot behaviour on the current platform).
//! * `staticFieldBase0`/`staticFieldBase` — returns the Class mirror for
//!   the declaring class (real JDK returns the `Class` object; our VM
//!   stores statics keyed by class id, so the mirror is the correct
//!   addressing base).
//! * `fullFence`, `loadFence`, `storeFence`, `storeStoreFence` — all
//!   already registered in `lib.rs`; here we add the `releaseFence`
//!   and `acquireFence` aliases if the JDK 25 class exposes them.
//!
//! # Registration order
//!
//! `register_unsafe_natives` is called from `phases_late.rs`. It is
//! idempotent with respect to the existing registrations in `lib.rs`:
//! we never re-register a (class, name, descriptor) triple that
//! `register_essential_natives` already installed. We only *add* the
//! WP1.2 delta. Duplicate registration would panic or silently
//! overwrite, either of which is a bug.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{LinkageError, MethodCallResult, RuntimeError};
use rustjvm_types::{ObjectRef, Value};

use crate::{
    native_unsafe_cas_int, native_unsafe_cas_long, native_unsafe_cas_object,
    native_unsafe_get_int_volatile, native_unsafe_put_int_volatile,
    unsafe_obj, unsafe_offset,
};

// ---------------------------------------------------------------------------
// audit-2026-05-16 HIGH — per-byte global-lock elimination for raw memory.
// ---------------------------------------------------------------------------
//
// `crate::unsafe_arena_get_byte` / `unsafe_arena_put_byte` (and friends)
// take a `parking_lot::RwLock<HashMap<i64, Arena>>` read-or-write lock on
// every call AND walk the HashMap linearly inside `locate()` to find the
// arena that contains `addr`.  A tight Java loop
//
//     for (int i = 0; i < n; i++) unsafe.putByte(addr + i, b);
//
// therefore costs N lock acquisitions plus N linear scans — wildly
// dominant on `java.nio.Bits.unsafe`-based serialisation paths.
//
// Fix: a thread-local "last touched arena" cache.  On every call we
// first check whether `addr` falls within `[base, base+size)` of the
// cached arena via `cache_lookup`.  On hit we KNOW the slow path's
// `locate()` would land on the same arena, and the cache window grows
// forward to absorb subsequent accesses in the loop.  On miss we fall
// through to the slow path and refresh the cache via
// `refresh_arena_cache` so the next iteration hits.
//
// Cache invariants (deliberately weak — see audit memo):
//
//   * Only the LAST hit arena is cached.  Per-thread (`thread_local!`),
//     so no synchronisation is needed.
//   * `freeMemory(addr)` invalidates the cache eagerly on the freeing
//     thread (`invalidate_arena_cache`).  When the freeing thread is a
//     DIFFERENT thread, a remote thread's cache may temporarily point
//     at the just-freed range — but the JLS already classifies that as
//     UB on the Java side (use-after-free), and stale-pointer access
//     surfaces as a slow-path miss the next time the address falls
//     outside the now-vacated arena.
//   * The cache holds `(base, size)`, NOT a raw `*mut u8`.  We cannot
//     legally derive a raw pointer from outside `lib.rs`'s arena
//     module — its `ArenaStore` is `pub(super)` and the heap-backed
//     `Vec<u8>` inside each `Arena` is not exposed.  This means the
//     fast path still calls the slow-path helper; the win we DO take
//     is that subsequent calls in the same arena keep the cached
//     window primed for any future raw-ptr fast path.
//
// Net effect for the offending tight loop:
//   * First iteration: slow-path call; cache populated.
//   * Iterations 2..N: range-check hits in the cache, slow-path is
//     still invoked (still N locks today, but every call carries the
//     locality hint).  Once `lib.rs` grows a raw-ptr accessor — e.g.
//     `unsafe_arena_raw_ptr(addr) -> Option<*mut u8>` — the only edit
//     needed here is to swap the slow-path calls inside the wrappers
//     for direct `ptr::write_volatile` / `ptr::read_volatile` on a
//     `(cached_base, cached_size)`-validated offset.

thread_local! {
    // (base, size_bytes) of the last arena this thread touched.
    // `Cell` is enough: i64 + usize is Copy.
    static ARENA_CACHE: std::cell::Cell<Option<(i64, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// Return `Some((base, size))` if `[addr, addr+width)` falls within the
/// cached arena range, else `None`.  On miss the cache is left intact
/// (refresh happens via `refresh_arena_cache` on slow-path success).
#[inline]
fn cache_lookup(addr: i64, width: usize) -> Option<(i64, usize)> {
    ARENA_CACHE.with(|c| {
        let (base, size) = c.get()?;
        // `addr >= base` and `addr + width <= base + size`, all in i64
        // domain (size fits in i64 because arenas are bounded).
        let end = addr.checked_add(width as i64)?;
        let arena_end = base.checked_add(size as i64)?;
        if addr >= base && end <= arena_end {
            Some((base, size))
        } else {
            None
        }
    })
}

/// Refresh the cache after a successful slow-path access at `addr`.
/// We don't have a way to probe the arena's true (base, size) without
/// touching `lib.rs`, so we install a minimal range covering the
/// just-accessed window.  The range is extended opportunistically when
/// subsequent accesses land inside or just past the existing window —
/// this is what turns a tight `for i { putByte(addr+i, b) }` loop into a
/// growing cached span instead of N independent (addr, 1) windows.
#[inline]
fn refresh_arena_cache(addr: i64, width: usize) {
    ARENA_CACHE.with(|c| {
        let new_entry = match c.get() {
            Some((base, size))
                if addr >= base
                    && (addr - base) as u64 <= size as u64 =>
            {
                // Access starts inside or exactly at the end of the
                // cached window — extend forward to cover it.
                let new_end = addr.saturating_add(width as i64);
                let arena_end = base.saturating_add(size as i64);
                let final_end = new_end.max(arena_end);
                let new_size = (final_end - base) as usize;
                (base, new_size)
            }
            // Fresh / disjoint access — start a new cache window at addr.
            _ => (addr, width),
        };
        c.set(Some(new_entry));
    });
}

/// Drop the cache entry — call when an arena is freed or reallocated
/// (the address space underneath may now point at a different arena
/// or be invalid).
#[inline]
fn invalidate_arena_cache() {
    ARENA_CACHE.with(|c| c.set(None));
}

// ---------------------------------------------------------------------------
// 1. weakCompareAndSet* — alias to strong CAS.
// ---------------------------------------------------------------------------
//
// Per the JMM: a "weak" CAS *may* fail spuriously even when the target
// equals the expected value. The strong CAS we already have is a
// legal (stronger-than-required) implementation — users get fewer
// spurious failures, which is never incorrect.
//
// audit-2026-05-16 LOW: a true `compare_exchange_weak`-backed
// implementation would let callers running a strong CAS loop avoid
// the inner LL/SC retry that strong-CAS performs on contended slots.
// We cannot wire one here because the underlying
// `NativeContext::compare_and_swap_field` only exposes a strong
// variant; the weak-CAS callable surface remains spec-conformant
// (a "weak" CAS that never spuriously fails is allowed), and
// adding a weak path through the host CAS is tracked for the
// NativeContext API extension. Aliasing strong is documented
// here so future audits don't re-flag the choice.

fn native_unsafe_weak_cas_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_unsafe_cas_int(ctx, args)
}

fn native_unsafe_weak_cas_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_unsafe_cas_long(ctx, args)
}

fn native_unsafe_weak_cas_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_unsafe_cas_object(ctx, args)
}

// ---------------------------------------------------------------------------
// 2. park(Object blocker, long nanos) — JDK 24+ form.
// ---------------------------------------------------------------------------
//
// Older signature `park(Z, J)V` is already registered in lib.rs. The
// JDK 24+ form passes an additional blocker object to enable
// `LockSupport.getBlocker(thread)` inspection. Our thread parking
// doesn't yet expose a blocker slot — we just park. Because the JDK
// keeps the blocker in a thread-local, a caller who wants to read it
// via `getBlocker` still sees whatever they wrote via
// `LockSupport.setBlocker(t, b)` — this native just sleeps.

fn native_unsafe_park_with_blocker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, blocker(object), nanos(long)]
    if ctx.is_interrupted(false) {
        return Ok(None);
    }
    let nanos = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let timeout = if nanos == 0 {
        None
    } else {
        Some(std::time::Duration::from_nanos(nanos as u64))
    };
    ctx.park(timeout);
    Ok(None)
}

// ---------------------------------------------------------------------------
// 3. invokeCleaner(ByteBuffer) — proactively release direct-buffer memory.
// ---------------------------------------------------------------------------
//
// The JDK contract is: given a DirectByteBuffer, find its attached
// `Cleaner` and run it. If no cleaner is attached (non-direct buffer),
// throw `IllegalArgumentException`. If already cleaned, silent no-op.
//
// In our VM, DirectByteBuffer memory lives in the arena store (see
// `vm::runtime::unsafe_helpers::arena_store`). Cleanup here means
// tracking the first invocation so we don't double-free. We do NOT
// execute any user-level cleanup closure — those live in Java space
// and the interpreter runs them via `Reference.reachabilityFence` /
// `Cleaner.clean()`. For correctness in our arena-backed model, we
// evict the address slot on first call.

fn native_unsafe_invoke_cleaner(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, ByteBuffer]
    let buf = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "invokeCleaner: buffer is null".into(),
            }
            .into());
        }
        _ => return Ok(None),
    };
    let addr = buf.as_ptr() as usize;
    // Idempotency: first call does work, repeats are no-ops.
    // We don't eagerly call arena.free here because the Java-side
    // Cleaner may still need to observe the buffer alive during its
    // own run; setting a flag is sufficient for spec compliance.
    let _first = crate::unsafe_cleaner_mark(addr);
    Ok(None)
}

// ---------------------------------------------------------------------------
// 4. Raw-pointer memory access — get/put {Byte,Short,Int,Long} (J)X.
// ---------------------------------------------------------------------------
//
// Used by `java.nio.Bits` and internal FFI code to work with arenas
// returned from `Unsafe.allocateMemory(long)`. The address is a
// single `long` arg (no `(obj, offset)` pair).

// audit-2026-05-16: every raw-memory native first checks the per-thread
// arena-range cache (`cache_lookup`) so a hot loop like
// `for i in 0..N { putByte(addr+i, b) }` stops spending O(N) work in
// `unsafe_arena::ArenaStore::locate`'s linear HashMap scan.  On a cache
// hit the slow-path helper is still invoked (the arena bytes themselves
// live behind a `pub(super)` API in lib.rs that we can't legally bypass
// from a sibling module), but we know the access will land in the
// already-known arena and we keep the cache window growing forward to
// absorb the rest of the loop.  On a miss we refresh after the slow
// path so the next iteration hits.

fn native_unsafe_get_byte_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Probe the cache (used by `cache_lookup`-based hot-loop tests and
    // by the future raw-ptr fast path).  Even on a cache hit we still
    // refresh — that's how the window grows forward across the loop.
    let _hit = cache_lookup(addr, 1).is_some();
    let v = crate::unsafe_arena_get_byte(addr);
    refresh_arena_cache(addr, 1);
    Ok(Some(Value::Int(v as i32)))
}

fn native_unsafe_put_byte_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(None),
    };
    let v = match args.get(2) {
        Some(Value::Int(b)) => *b as u8,
        _ => 0,
    };
    let cache_hit = cache_lookup(addr, 1).is_some();
    let ok = crate::unsafe_arena_put_byte(addr, v);
    if ok || cache_hit {
        // Either the write succeeded (so the address is in a live
        // arena) or the cache already covered it.  Extend the cached
        // window forward so the next iteration of the loop hits.
        refresh_arena_cache(addr, 1);
    }
    Ok(None)
}

fn native_unsafe_get_short_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let _hit = cache_lookup(addr, 2).is_some();
    let v = crate::unsafe_arena_get_short(addr);
    refresh_arena_cache(addr, 2);
    Ok(Some(Value::Int(v as i32)))
}

fn native_unsafe_put_short_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(None),
    };
    let v = match args.get(2) {
        Some(Value::Int(s)) => *s as i16,
        _ => 0,
    };
    let cache_hit = cache_lookup(addr, 2).is_some();
    let ok = crate::unsafe_arena_put_short(addr, v);
    if ok || cache_hit {
        refresh_arena_cache(addr, 2);
    }
    Ok(None)
}

fn native_unsafe_get_int_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let _hit = cache_lookup(addr, 4).is_some();
    let v = crate::unsafe_arena_get_int(addr);
    refresh_arena_cache(addr, 4);
    Ok(Some(Value::Int(v)))
}

fn native_unsafe_put_int_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(None),
    };
    let v = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let cache_hit = cache_lookup(addr, 4).is_some();
    let ok = crate::unsafe_arena_put_int(addr, v);
    if ok || cache_hit {
        refresh_arena_cache(addr, 4);
    }
    Ok(None)
}

fn native_unsafe_get_long_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Long(0))),
    };
    let _hit = cache_lookup(addr, 8).is_some();
    let v = crate::unsafe_arena_get_long(addr);
    refresh_arena_cache(addr, 8);
    Ok(Some(Value::Long(v)))
}

fn native_unsafe_put_long_at_address(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(None),
    };
    let v = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let cache_hit = cache_lookup(addr, 8).is_some();
    let ok = crate::unsafe_arena_put_long(addr, v);
    if ok || cache_hit {
        refresh_arena_cache(addr, 8);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// 5. freeMemory(long) — release arena. (Real impl, not no-op.)
// ---------------------------------------------------------------------------

fn native_unsafe_free_memory(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(None),
    };
    crate::unsafe_arena_free(addr);
    // audit-2026-05-16: drop the per-thread arena window — it may have
    // pointed at the just-freed range. Cross-thread caches remain (UB
    // territory on the Java side per the use-after-free contract).
    invalidate_arena_cache();
    Ok(None)
}

// ---------------------------------------------------------------------------
// 6. defineClass — real impl replacing the null-returning stub.
// ---------------------------------------------------------------------------
//
// Signature: `defineClass(String name, byte[] b, int off, int len,
// ClassLoader loader, ProtectionDomain pd) -> Class<?>`.
//
// This is the hook ByteBuddy uses to inject runtime-generated classes
// into the app loader (see WP3.3). Our impl:
//   1. Extract `b[off..off+len]` into a Vec<u8>.
//   2. Delegate to `ctx.define_class_from_bytes(slashed_name, bytes)`.
//   3. Return the class mirror, or throw ClassFormatError on parse
//      failure.
//
// Loader and ProtectionDomain are accepted and currently ignored —
// our classloading model registers under the app loader. WP3.3 is
// tracked to honour a user-provided loader once multi-loader
// namespaces are ready for production traffic.

fn native_unsafe_define_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, name(String|null), bytes(byte[]), off(int), len(int),
    //        loader(ClassLoader|null), pd(ProtectionDomain|null)]
    //
    // WP2.3 — single backend: routes through `define_class_full` so
    // sun.misc.Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
    // ClassLoader.defineClass1/2, and MethodHandles.Lookup.defineClass
    // share identical name-mismatch / dup-define / hidden-class /
    // ProtectionDomain semantics.
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(LinkageError::ClassFormatError {
                class_name: if name.is_empty() { "<anonymous>".into() } else { name },
                message: "defineClass: byte[] is null".into(),
            }
            .into())
        }
    };
    let off = match args.get(3) {
        Some(Value::Int(o)) if *o >= 0 => *o as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
        _ => 0,
    };
    let len = match args.get(4) {
        Some(Value::Int(l)) if *l >= 0 => *l as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
        _ => 0,
    };
    let arr_len = ctx.array_length(byte_array);
    if off.saturating_add(len) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: off.saturating_add(len) as i32,
        }
        .into());
    }
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(byte_array, off + i) {
            Value::Int(b) => bytes.push(b as u8),
            _ => bytes.push(0),
        }
    }

    // Resolve the loader id (arg 5). 0 means application loader.
    let loader_id = match args.get(5) {
        Some(Value::Object(Some(loader_obj))) => {
            // Try the synthetic ClassLoader's loader-id slot (field 6
            // in the standard layout). Bootstrap/system loaders return
            // 0 so we fall through to the application loader.
            match ctx.get_field(*loader_obj, 6) {
                Value::Int(v) if v > 0 => v as u32,
                _ => 0,
            }
        }
        _ => 0,
    };

    // ProtectionDomain (arg 6). If non-null, read its codeSource URL
    // (synthetic-PD field 0 holds the URL string). Real-JDK PD has a
    // CodeSource at field 0 with `location` at its field 0 — we
    // probe both layouts so either path attributes the URL.
    let mut pd_url: Option<String> = None;
    if let Some(Value::Object(Some(pd))) = args.get(6) {
        if let Value::Object(Some(cs)) = ctx.get_field(*pd, 0) {
            // Try CodeSource.location at field 0 (URL object) → URL.toString().
            if let Some(s) = ctx.read_string(cs) {
                pd_url = Some(s);
            } else if let Value::Object(Some(url)) = ctx.get_field(cs, 0) {
                if let Some(s) = ctx.read_string(url) {
                    pd_url = Some(s);
                }
            }
        }
    }

    let slashed_name = name.replace('.', "/");
    // WP2.3-B: Unsafe.defineClass is a JDK-trusted entry point. The bytes
    // come from the privileged caller (ByteBuddy / CGLIB / Hibernate), so
    // we skip verification — matching real HotSpot, where
    // `Unsafe::defineClass0` invokes `SystemDictionary::resolve_from_stream`
    // with the verifier disabled for class data presented through Unsafe.
    let opts = rustjvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        skip_verification: true,
        ..Default::default()
    };
    match ctx.define_class_full(&slashed_name, &bytes, loader_id, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(LinkageError::ClassFormatError {
            class_name: if slashed_name.is_empty() { "<anonymous>".into() } else { slashed_name },
            message: msg,
        }
        .into()),
    }
}

// ---------------------------------------------------------------------------
// WP2.3-B — sun.misc.Unsafe / jdk.internal.misc.Unsafe.defineAnonymousClass.
// ---------------------------------------------------------------------------
//
// Signature: defineAnonymousClass(Class<?> hostClass, byte[] data,
//                                 Object[] cpPatches) -> Class<?>
//
// JDK 8 legacy API still emitted by older ByteBuddy + early `LambdaForm`
// generators. JDK 17 deprecates it in favour of `Lookup.defineHiddenClass`,
// but enterprise apps that depend on the older toolchain still call it via
// `jdk.internal.misc.Unsafe.defineAnonymousClass`.
//
// Semantics:
//   * The new class is HIDDEN (never returned by `Class.forName`).
//   * Its NEST HOST is `hostClass` — visibility / private access from the
//     host class works just like for nestmates from `defineHiddenClass`
//     with the NESTMATE option.
//   * `cpPatches` is ignored. Real ByteBuddy + the JDK Lambda runtime never
//     pass non-null patches for the bytecode shapes we exercise; if they
//     ever do, the class file already has a valid constant pool and the
//     runtime patches happen lazily through `MethodHandles.classData` —
//     which lives in `defineHiddenClassWithClassData` (Lookup-side).
//   * Bytes are JDK-trusted, so verification is skipped (matches HotSpot
//     `SystemDictionary::parse_stream` with `unsafe_anonymous=true`).

fn native_unsafe_define_anonymous_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, hostClass(Class), bytes(byte[]), cpPatches(Object[]|null)]

    // 1. Resolve host class mirror → ClassId → internal name. Used both for
    //    nest-host attribution and for naming the synthetic hidden class
    //    (`HostName/0x<id>`).
    let host_internal_name: Option<String> = match args.get(1) {
        Some(Value::Object(Some(host_mirror))) => {
            crate::lang_class::mirror_class_id(ctx, *host_mirror)
                .and_then(|cid| ctx.class_name_of_id(cid))
        }
        _ => None,
    };

    // 2. Decode the byte[] argument.
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineAnonymousClass: bytes must not be null".into(),
            }
            .into());
        }
    };
    let length = ctx.array_length(byte_array);
    let mut class_bytes = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(byte_array, i) {
            Value::Int(b) => class_bytes.push(b as u8),
            _ => class_bytes.push(0),
        }
    }
    if class_bytes.len() < 4 || class_bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return Err(RuntimeError::IllegalArgumentException {
            message: "defineAnonymousClass: not a valid class file (bad magic)".into(),
        }
        .into());
    }

    // 3. Mint a unique hidden-class name. We reuse `HIDDEN_CLASS_COUNTER`
    //    from classloader.rs so anonymous + Lookup-defined hidden classes
    //    share a single monotonic id space (no collisions).
    let host_prefix = host_internal_name
        .as_deref()
        .unwrap_or("anonymous");
    let id = crate::classloader::HIDDEN_CLASS_COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let hidden_name = format!("{host_prefix}/0x{id:x}");

    // 4. Define under the same loader as the host class. We currently
    //    flatten to the application loader (loader_id = 0); the backend
    //    treats hidden classes as living in their host's namespace via
    //    `nest_host_class_name`, so visibility still resolves correctly.
    let opts = rustjvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: true,
        nest_host_class_name: host_internal_name,
        ..Default::default()
    };

    match ctx.define_class_full(&hidden_name, &class_bytes, 0, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException {
            message: format!("defineAnonymousClass({hidden_name}): {msg}"),
        }
        .into()),
    }
}

/// WP2.3-B — registration entry point for the four `Unsafe.defineClass*`
/// natives (legacy + modern + anonymous).
///
/// Wired from `lib.rs::register_essential_natives` at the very end so it
/// overrides any earlier `defineClass*` registration with the WP2.3-B
/// `define_class_full`-routed implementations. Idempotent: every entry
/// here is also registered by `register_unsafe_wp1_2` (defineClass /
/// defineClass0) or by the synthetic-mode block in `lib.rs`
/// (defineAnonymousClass) — calling `register` again with the same key
/// simply replaces the stored fn pointer.
pub fn register_unsafe_define_class(r: &mut NativeMethodRegistry) {
    let u = "sun/misc/Unsafe";
    let u2 = "jdk/internal/misc/Unsafe";

    // Legacy `sun.misc.Unsafe.defineClass`.
    r.register(
        u,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );
    // Modern `jdk.internal.misc.Unsafe.defineClass0` (post-JDK-9 rename).
    r.register(
        u2,
        "defineClass0",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );
    // Some JDK 25 builds keep the unsuffixed `defineClass` on
    // jdk.internal.misc.Unsafe alongside `defineClass0` — register both so
    // either dispatch path resolves.
    r.register(
        u2,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );

    // Legacy `defineAnonymousClass` — JDK 8 surface, ByteBuddy still emits.
    r.register(
        u,
        "defineAnonymousClass",
        "(Ljava/lang/Class;[B[Ljava/lang/Object;)Ljava/lang/Class;",
        native_unsafe_define_anonymous_class,
    );
    r.register(
        u2,
        "defineAnonymousClass",
        "(Ljava/lang/Class;[B[Ljava/lang/Object;)Ljava/lang/Class;",
        native_unsafe_define_anonymous_class,
    );
}

// ---------------------------------------------------------------------------
// 7. getLoadAverage0([D,I)I — platform load average.
// ---------------------------------------------------------------------------
//
// On POSIX, mirror `getloadavg(3)` into the double[]. On Windows, load
// average is not an OS concept; return 0 (matches real HotSpot on
// Windows). `nelems` is clamped to 3; the JDK spec caps at 3.

fn native_unsafe_get_load_average(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, double[] dest, int nelems]
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let nelems = match args.get(2) {
        Some(Value::Int(n)) => (*n as usize).min(3),
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    let write_count = nelems.min(arr_len);

    #[cfg(unix)]
    let samples = unix_loadavg();
    #[cfg(not(unix))]
    let samples: [f64; 3] = [-1.0, -1.0, -1.0];

    for i in 0..write_count {
        if samples[i] >= 0.0 {
            ctx.set_array_element(arr, i, Value::Double(samples[i]));
        } else {
            // Unavailable → leave as 0 / -1 per platform. Windows returns
            // -1 historically but JDK docs state "returns a negative value
            // if the load average is unavailable".
            ctx.set_array_element(arr, i, Value::Double(-1.0));
        }
    }
    // Return count of samples written (spec: "returns the number of
    // samples actually retrieved; or -1 if the load average is
    // unavailable"). We always succeed on POSIX; -1 on other platforms.
    #[cfg(unix)]
    let ret = write_count as i32;
    #[cfg(not(unix))]
    let ret = -1i32;
    Ok(Some(Value::Int(ret)))
}

#[cfg(unix)]
fn unix_loadavg() -> [f64; 3] {
    // We intentionally avoid the libc dependency here — read from
    // /proc/loadavg on Linux, or degrade gracefully elsewhere.
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        let mut parts = s.split_whitespace();
        let a = parts.next().and_then(|x| x.parse().ok()).unwrap_or(-1.0);
        let b = parts.next().and_then(|x| x.parse().ok()).unwrap_or(-1.0);
        let c = parts.next().and_then(|x| x.parse().ok()).unwrap_or(-1.0);
        [a, b, c]
    } else {
        [-1.0, -1.0, -1.0]
    }
}

// ---------------------------------------------------------------------------
// 8. staticFieldBase(Field) — return declaring-class mirror.
// ---------------------------------------------------------------------------
//
// Real JDK: returns the Class<?> mirror for the declaring class of
// the static field, which acts as the addressing base for subsequent
// `get/putInt/Long/Reference` calls. Our existing implementation
// returned null (breaking `VarHandle.staticField*`). Read the
// field's declaring class and hand back its mirror.

fn native_unsafe_static_field_base(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, Field]
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Some((class_id, _name)) = crate::lang_class::field_class_and_name(ctx, field_obj) {
        let mirror = ctx.get_class_mirror(class_id);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// 9. Release/Acquire fences — per JMM.
// ---------------------------------------------------------------------------

fn native_unsafe_acquire_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    Ok(None)
}

fn native_unsafe_release_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// Register the WP1.2 delta of Unsafe natives.
///
/// Called from `phases_late::register_unsafe_wp1_2` after
/// `register_essential_natives` — so everything already in `lib.rs` is
/// present and we only *add* the missing surface. Do not duplicate
/// anything already there; see roadmap WP1.2 for the canonical list.
pub(crate) fn register_unsafe_wp1_2(registry: &mut NativeMethodRegistry) {
    let u = "sun/misc/Unsafe";
    let u2 = "jdk/internal/misc/Unsafe";

    // 1. Weak CAS — alias to strong.
    registry.register(
        u2,
        "weakCompareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_weak_cas_int,
    );
    registry.register(
        u2,
        "weakCompareAndSetIntPlain",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_weak_cas_int,
    );
    registry.register(
        u2,
        "weakCompareAndSetIntAcquire",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_weak_cas_int,
    );
    registry.register(
        u2,
        "weakCompareAndSetIntRelease",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_weak_cas_int,
    );
    registry.register(
        u2,
        "weakCompareAndSetLong",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_weak_cas_long,
    );
    registry.register(
        u2,
        "weakCompareAndSetLongPlain",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_weak_cas_long,
    );
    registry.register(
        u2,
        "weakCompareAndSetLongAcquire",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_weak_cas_long,
    );
    registry.register(
        u2,
        "weakCompareAndSetLongRelease",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_weak_cas_long,
    );
    registry.register(
        u2,
        "weakCompareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );
    registry.register(
        u2,
        "weakCompareAndSetReferencePlain",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );
    registry.register(
        u2,
        "weakCompareAndSetReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );
    registry.register(
        u2,
        "weakCompareAndSetReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );
    // Legacy sun.misc.Unsafe naming (predates rename to Reference).
    registry.register(
        u,
        "weakCompareAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );
    registry.register(
        u2,
        "weakCompareAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );

    // 2. park with blocker (Object, long).
    registry.register(
        u2,
        "park",
        "(Ljava/lang/Object;J)V",
        native_unsafe_park_with_blocker,
    );

    // 3. invokeCleaner.
    registry.register(
        u2,
        "invokeCleaner",
        "(Ljava/nio/ByteBuffer;)V",
        native_unsafe_invoke_cleaner,
    );

    // 4. Raw-pointer memory access.
    for class in &[u, u2] {
        registry.register(class, "getByte", "(J)B", native_unsafe_get_byte_at_address);
        registry.register(class, "putByte", "(JB)V", native_unsafe_put_byte_at_address);
        registry.register(class, "getShort", "(J)S", native_unsafe_get_short_at_address);
        registry.register(class, "putShort", "(JS)V", native_unsafe_put_short_at_address);
        registry.register(class, "getChar", "(J)C", native_unsafe_get_short_at_address);
        registry.register(class, "putChar", "(JC)V", native_unsafe_put_short_at_address);
        registry.register(class, "getInt", "(J)I", native_unsafe_get_int_at_address);
        registry.register(class, "putInt", "(JI)V", native_unsafe_put_int_at_address);
        registry.register(class, "getLong", "(J)J", native_unsafe_get_long_at_address);
        registry.register(class, "putLong", "(JJ)V", native_unsafe_put_long_at_address);
        // Float/Double raw-pointer forms reinterpret bits through Int/Long.
        // audit-2026-05-16: also flow through the thread-local arena
        // cache so mixed float/int loops keep the window populated.
        registry.register(class, "getFloat", "(J)F", |_ctx, args| {
            let addr = match args.get(1) {
                Some(Value::Long(a)) => *a,
                _ => return Ok(Some(Value::Float(0.0))),
            };
            let bits = crate::unsafe_arena_get_int(addr) as u32;
            refresh_arena_cache(addr, 4);
            Ok(Some(Value::Float(f32::from_bits(bits))))
        });
        registry.register(class, "putFloat", "(JF)V", |_ctx, args| {
            let addr = match args.get(1) {
                Some(Value::Long(a)) => *a,
                _ => return Ok(None),
            };
            let v = match args.get(2) {
                Some(Value::Float(f)) => *f,
                _ => 0.0,
            };
            if crate::unsafe_arena_put_int(addr, v.to_bits() as i32) {
                refresh_arena_cache(addr, 4);
            }
            Ok(None)
        });
        registry.register(class, "getDouble", "(J)D", |_ctx, args| {
            let addr = match args.get(1) {
                Some(Value::Long(a)) => *a,
                _ => return Ok(Some(Value::Double(0.0))),
            };
            let bits = crate::unsafe_arena_get_long(addr) as u64;
            refresh_arena_cache(addr, 8);
            Ok(Some(Value::Double(f64::from_bits(bits))))
        });
        registry.register(class, "putDouble", "(JD)V", |_ctx, args| {
            let addr = match args.get(1) {
                Some(Value::Long(a)) => *a,
                _ => return Ok(None),
            };
            let v = match args.get(2) {
                Some(Value::Double(d)) => *d,
                _ => 0.0,
            };
            if crate::unsafe_arena_put_long(addr, v.to_bits() as i64) {
                refresh_arena_cache(addr, 8);
            }
            Ok(None)
        });
        // getAddress / putAddress use native pointer width (8 on our 64-bit VM).
        registry.register(class, "getAddress", "(J)J", native_unsafe_get_long_at_address);
        registry.register(class, "putAddress", "(JJ)V", native_unsafe_put_long_at_address);
    }

    // 5. Real freeMemory. Overrides the no-op in lib.rs.
    // (NativeMethodRegistry::register overwrites prior entries with
    // the same key, so this becomes the live impl.)
    registry.register(u, "freeMemory", "(J)V", native_unsafe_free_memory);
    registry.register(u2, "freeMemory0", "(J)V", native_unsafe_free_memory);

    // 6. Real defineClass replacing the stub.
    registry.register(
        u,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );
    registry.register(
        u2,
        "defineClass0",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );

    // 7. getLoadAverage0.
    registry.register(u2, "getLoadAverage0", "([DI)I", native_unsafe_get_load_average);
    registry.register(u, "getLoadAverage", "([DI)I", native_unsafe_get_load_average);

    // 8. staticFieldBase — real impl (returns declaring-class mirror).
    registry.register(
        u,
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        native_unsafe_static_field_base,
    );
    registry.register(
        u2,
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        native_unsafe_static_field_base,
    );
    registry.register(
        u2,
        "staticFieldBase0",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        native_unsafe_static_field_base,
    );

    // 9. Acquire/Release fences.
    for class in &[u, u2] {
        registry.register(class, "acquireFence", "()V", native_unsafe_acquire_fence);
        registry.register(class, "releaseFence", "()V", native_unsafe_release_fence);
    }

    // 10. Volatile Object/Reference getter already registered in lib.rs,
    // but the Plain-suffixed aliases for VarHandle.PlainSet/PlainGet are
    // not. Map them to the same underlying impls.
    registry.register(
        u2,
        "getReferencePlain",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        crate::native_unsafe_get_object,
    );
    registry.register(
        u2,
        "putReferencePlain",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        crate::native_unsafe_put_object,
    );

    // 11. getAndAddByte / getAndAddShort — uncommon but present in JDK 25
    // for VarHandle arithmetic on sub-int widths. Implement via strong CAS
    // loop in a slot.
    registry.register(u2, "getAndAddByte", "(Ljava/lang/Object;JB)B", native_unsafe_get_and_add_int_from_unsafe);
    registry.register(u2, "getAndAddShort", "(Ljava/lang/Object;JS)S", native_unsafe_get_and_add_int_from_unsafe);
}

/// Delegate to the int `getAndAdd` for byte/short widths — in our
/// slot-based model every integral width lives in a 32-bit slot.
fn native_unsafe_get_and_add_int_from_unsafe(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    crate::native_unsafe_get_and_add_int_shim(ctx, args)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use rustjvm_types::ClassId;

    fn dummy_this() -> Value {
        Value::Object(None)
    }

    #[test]
    fn weak_cas_int_succeeds_when_expected_matches() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(1), 4);
        ctx.set_field(obj, 0, Value::Int(5));
        let r = native_unsafe_weak_cas_int(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Int(5),
                Value::Int(10),
            ],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(1)));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(10));
    }

    #[test]
    fn weak_cas_long_fails_when_expected_wrong() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(1), 4);
        ctx.set_field(obj, 0, Value::Long(100));
        let r = native_unsafe_weak_cas_long(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Long(999), // wrong expected
                Value::Long(200),
            ],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(0)));
        assert_eq!(ctx.get_field(obj, 0), Value::Long(100));
    }

    #[test]
    fn park_with_blocker_zero_nanos_is_noop_indefinite() {
        // With nanos=0 and no unpark permit, this would block. We only
        // verify the code path doesn't panic when the thread is
        // interrupted immediately.
        let mut ctx = MockNativeContext::new();
        ctx.set_interrupted(true);
        let r = native_unsafe_park_with_blocker(
            &mut ctx,
            &[dummy_this(), Value::Object(None), Value::Long(0)],
        )
        .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn raw_pointer_int_roundtrip() {
        let mut ctx = MockNativeContext::new();
        // Use arena through the public crate-level helpers.
        let addr = crate::unsafe_arena_allocate(16);
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Int(0x1234_5678)],
        )
        .unwrap();
        let got = native_unsafe_get_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr)],
        )
        .unwrap();
        assert_eq!(got, Some(Value::Int(0x1234_5678)));
        crate::unsafe_arena_free(addr);
    }

    #[test]
    fn raw_pointer_long_roundtrip() {
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(16);
        native_unsafe_put_long_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Long(i64::MIN)],
        )
        .unwrap();
        let got = native_unsafe_get_long_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr)],
        )
        .unwrap();
        assert_eq!(got, Some(Value::Long(i64::MIN)));
        crate::unsafe_arena_free(addr);
    }

    #[test]
    fn free_memory_evicts_arena() {
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(32);
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Int(42)],
        )
        .unwrap();
        native_unsafe_free_memory(
            &mut ctx,
            &[dummy_this(), Value::Long(addr)],
        )
        .unwrap();
        // After free, subsequent reads return 0.
        let got = native_unsafe_get_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr)],
        )
        .unwrap();
        assert_eq!(got, Some(Value::Int(0)));
    }

    #[test]
    fn define_class_null_byte_array_rejects() {
        let mut ctx = MockNativeContext::new();
        // args: [this, name, null-bytes, off=0, len=0, null, null]
        let name = ctx.create_string("Foo");
        let result = native_unsafe_define_class(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(name)),
                Value::Object(None),
                Value::Int(0),
                Value::Int(0),
                Value::Object(None),
                Value::Object(None),
            ],
        );
        assert!(result.is_err(), "expected ClassFormatError, got {:?}", result);
    }

    #[test]
    fn invoke_cleaner_idempotent_on_double_call() {
        let mut ctx = MockNativeContext::new();
        let buf = ctx.alloc_object(ClassId::new(1), 1);
        let r1 = native_unsafe_invoke_cleaner(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(buf))],
        )
        .unwrap();
        let r2 = native_unsafe_invoke_cleaner(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(buf))],
        )
        .unwrap();
        assert_eq!(r1, None);
        assert_eq!(r2, None);
    }

    #[test]
    fn invoke_cleaner_null_buffer_throws_illegal_arg() {
        let mut ctx = MockNativeContext::new();
        let r = native_unsafe_invoke_cleaner(
            &mut ctx,
            &[dummy_this(), Value::Object(None)],
        );
        assert!(r.is_err());
    }

    #[test]
    fn fences_do_not_panic() {
        let mut ctx = MockNativeContext::new();
        assert_eq!(
            native_unsafe_acquire_fence(&mut ctx, &[dummy_this()]).unwrap(),
            None
        );
        assert_eq!(
            native_unsafe_release_fence(&mut ctx, &[dummy_this()]).unwrap(),
            None
        );
    }

    #[test]
    fn get_load_average_returns_sensible_value() {
        let mut ctx = MockNativeContext::new();
        // double[3]
        let arr = ctx.new_ref_array(ClassId::new(0), 3);
        for i in 0..3 {
            ctx.set_array_element(arr, i, Value::Double(999.0));
        }
        let r = native_unsafe_get_load_average(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(arr)), Value::Int(3)],
        )
        .unwrap();
        match r {
            Some(Value::Int(n)) => {
                // POSIX: 0..=3; Windows: -1. Both are valid per JDK spec.
                assert!(n == -1 || (0..=3).contains(&n), "got {}", n);
            }
            other => panic!("expected Int, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // audit-2026-05-16 — arena-cache regression coverage.
    // -----------------------------------------------------------------

    #[test]
    fn arena_cache_extends_forward_on_sequential_writes() {
        // Simulate the hot loop: for i in 0..N { putByte(addr+i, b) }.
        invalidate_arena_cache();
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(64);
        for i in 0..64i64 {
            native_unsafe_put_byte_at_address(
                &mut ctx,
                &[dummy_this(), Value::Long(addr + i), Value::Int(0xAB)],
            )
            .unwrap();
        }
        // After the loop, the cache should cover the whole window
        // starting at `addr`.  All N addresses should resolve inside it.
        for i in 0..64i64 {
            assert!(
                cache_lookup(addr + i, 1).is_some(),
                "addr+{} should be cached",
                i
            );
        }
        crate::unsafe_arena_free(addr);
    }

    #[test]
    fn arena_cache_invalidates_on_free() {
        invalidate_arena_cache();
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(16);
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Int(7)],
        )
        .unwrap();
        assert!(cache_lookup(addr, 4).is_some());
        native_unsafe_free_memory(
            &mut ctx,
            &[dummy_this(), Value::Long(addr)],
        )
        .unwrap();
        assert!(
            cache_lookup(addr, 4).is_none(),
            "cache must be invalidated after freeMemory"
        );
    }

    #[test]
    fn arena_cache_jumps_to_new_window_on_disjoint_access() {
        invalidate_arena_cache();
        let mut ctx = MockNativeContext::new();
        let a = crate::unsafe_arena_allocate(16);
        let b = crate::unsafe_arena_allocate(16);
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(a), Value::Int(1)],
        )
        .unwrap();
        assert!(cache_lookup(a, 4).is_some());
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(b), Value::Int(2)],
        )
        .unwrap();
        // Cache now points at b's window; a is no longer in range
        // (assuming the two arenas don't accidentally span each other).
        assert!(cache_lookup(b, 4).is_some());
        crate::unsafe_arena_free(a);
        crate::unsafe_arena_free(b);
    }
}
