// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{
    native_unsafe_cas_int, native_unsafe_cas_long, native_unsafe_cas_object,
    native_unsafe_get_int_volatile, native_unsafe_put_int_volatile, unsafe_obj, unsafe_offset,
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
///
/// audit-round6: the get/put natives no longer consult this on the hot
/// path — they trust the validated accessor's verdict directly and only
/// invalidate the cache on a freed-arena miss (so a stale window can't
/// mask a use-after-free). The range-lookup helper is retained for the
/// regression tests that assert the forward-extend / invalidate behavior.
#[cfg_attr(not(test), allow(dead_code))]
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
            Some((base, size)) if addr >= base && (addr - base) as u64 <= size as u64 => {
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

// FIX(bug-A): real-pointer fall-through for the single-element `Unsafe.get/putX(long)`
// natives. An address the arena store rejected but whose tag bit (62) is CLEAR is
// a real OS pointer — e.g. a `DirectByteBuffer` address from `dbb_allocate` that
// Netty pooled buffers write through `Unsafe.putByte`. Route it to real memory via
// the unified `NativeContext` accessor (the same one the `copyMemory` path uses,
// which raw-reads/writes real pointers and routes arena handles to the off-heap
// store). TAGGED rejects are freed/out-of-bounds handles and MUST keep surfacing
// the use-after-free `IllegalArgumentException` — never raw-access them (that would
// dereference a synthetic 0x4000_… address and SIGSEGV), so they are excluded here.
//
// GAP M2 (C2 review, "gate all native and FFI capabilities"): this pair is the
// single most powerful thing an untrusted class can reach, because a raw read
// and a raw write at an arbitrary address subsume every other capability —
// they can rewrite the gates themselves. They are therefore the one place a
// `Capability::RawMemory` check has to sit; one edit here covers every
// untagged-address `Unsafe` get/put (`getByte`/`putByte`/`getShort`/…/`putLong`).
//
// The gate runs only AFTER the two cheap address predicates have decided this
// really is a raw-pointer access. That ordering matters twice: an arena or
// tagged address must keep its existing use-after-free diagnosis untouched,
// and the audit report must not be flooded with rows for accesses that never
// dereferenced a raw pointer.
//
// `Ok(false)` means "not a raw pointer / the copy failed" — exactly what the
// old `bool` meant, so every caller's fallback path is unchanged. `Err` is a
// capability refusal, which is a `SecurityException` and must not be
// misreported as the "not in any live arena" `IllegalArgumentException`.
//
// See `capability_gate::gate_raw_memory` for why the permissive path here is a
// thread-local load and an integer compare rather than a full check.
#[inline]
fn real_ptr_read(
    ctx: &dyn NativeContext,
    addr: i64,
    out: &mut [u8],
) -> Result<bool, MethodCallFailed> {
    if addr <= 0 || crate::unsafe_arena_addr_is_tagged(addr) {
        return Ok(false);
    }
    crate::capability_gate::gate_raw_memory(
        ctx,
        crate::capability_gate::RAW_MEMORY_UNSAFE_ADDRESS,
    )?;
    Ok(ctx.copy_from_native_memory(addr, out))
}
#[inline]
fn real_ptr_write(
    ctx: &mut dyn NativeContext,
    addr: i64,
    data: &[u8],
) -> Result<bool, MethodCallFailed> {
    if addr <= 0 || crate::unsafe_arena_addr_is_tagged(addr) {
        return Ok(false);
    }
    crate::capability_gate::gate_raw_memory(
        &*ctx,
        crate::capability_gate::RAW_MEMORY_UNSAFE_ADDRESS,
    )?;
    Ok(ctx.copy_to_native_memory(addr, data))
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
    if ctx.is_current_virtual()
        && ctx.vt_pin_count() == 0
        && ctx.vt_park_for(timeout.unwrap_or(std::time::Duration::ZERO))
    {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield {
                wake_after_nanos: timeout
                    .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
                    .unwrap_or(0),
            },
        ));
    }
    // AQS-PARK-PIN: pin the blocker across the block so it can't be lost to
    // the JIT register-invisibility gap regardless of which JDK park overload
    // is live (see the matching fix in `NativeContextImpl::park`,
    // vm/src/vm/vm_exec.rs, and `native_lock_support_park`). Mirrors
    // `monitor_wait_keepalive`.
    let pin = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    ctx.park(timeout);
    if let Some(pin) = pin {
        ctx.unpin_native_roots(pin);
    }
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

fn native_unsafe_invoke_cleaner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    // MEASURED, HotSpot 25.0.4+7: a slice or duplicate of a direct buffer is an
    // `IllegalArgumentException` ("duplicate or slice"), and a non-direct
    // buffer is too. Freeing through a view would release memory the ORIGINAL
    // buffer still owns, so the refusal is the whole point of the method's
    // contract. CratonVM accepted every one of them.
    //
    // The JDK's own test is `!(directBuffer instanceof DirectBuffer) ||
    // ((DirectBuffer) directBuffer).attachment() != null` -- a view keeps a
    // reference to what it was cut from in `attachment`, and only a root
    // direct buffer has it null.
    if matches!(ctx.get_field_by_name(buf, "att"), Value::Object(Some(_))) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "invokeCleaner: duplicate or slice".into(),
        }
        .into());
    }
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
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    // audit-round5 fix #3 (HIGH): mirror the round-4 put-side IAE behavior.
    // The previous get-side called `unsafe_arena_get_byte` which silently
    // returned 0 on an out-of-arena address — callers reading garbage 0s
    // tripped downstream NPEs far from the actual bug. Throw IAE when the
    // cache misses AND the bounds-checked read misses.
    match crate::unsafe_arena_try_get_byte(addr) {
        Some(v) => {
            refresh_arena_cache(addr, 1);
            // `Unsafe.getByte` returns a Java `byte` — signed. Casting `u8`
            // straight to `i32` zero-extends (0x83 -> 131) instead of
            // sign-extending (0x83 -> -125), which silently flips every
            // caller's `b < 0` / top-bit check for byte values >= 0x80. Route
            // through `i8` first, matching the two-arg `(Object,long)` sibling
            // (`native_unsafe_get_byte_mb`), which already does this correctly.
            Ok(Some(Value::Int(v as i8 as i32)))
        }
        // audit-round6 fix (LOW, use-after-free unmasking): the validated
        // accessor (`try_get_byte`) is the authoritative liveness check and
        // it just reported the address is NOT in a live arena (freed /
        // shrunk). A per-thread cache hit here is therefore STALE — the
        // window it remembers was freed underneath us. Previously we trusted
        // the stale cache and returned 0, masking the use-after-free in the
        // Java caller. Instead drop the stale entry and surface the same
        // IllegalArgumentException the validated slow path mandates.
        None => {
            // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw read.
            let mut b = [0u8; 1];
            if real_ptr_read(ctx, addr, &mut b)? {
                // Sign-extend — see the comment on the arena-hit branch above.
                return Ok(Some(Value::Int(b[0] as i8 as i32)));
            }
            invalidate_arena_cache();
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.getByte: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        }
    }
}

fn native_unsafe_put_byte_at_address(
    ctx: &mut dyn NativeContext,
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
    let ok = crate::unsafe_arena_put_byte(addr, v);
    if ok {
        // Write landed in a live arena.  Extend the cached window forward
        // so the next iteration of the loop hits.
        refresh_arena_cache(addr, 1);
        return Ok(None);
    }
    // audit-round6 fix (LOW, use-after-free unmasking): the validated
    // accessor (`put_byte`) rejected the write, so the target lives in no
    // live arena (freed / shrunk). A per-thread cache hit here would be
    // STALE; previously we honored it and silently dropped the write,
    // masking the use-after-free in the Java caller. Drop the stale entry
    // and surface the `IllegalArgumentException` the validated path (and
    // Java's `Unsafe.putByte(long, byte)` contract) mandates.
    // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw write.
    if real_ptr_write(ctx, addr, &[v])? {
        return Ok(None);
    }
    invalidate_arena_cache();
    Err(RuntimeError::IllegalArgumentException {
        message: format!("Unsafe.putByte: address 0x{addr:x} is not in any live arena"),
    }
    .into())
}

fn native_unsafe_get_short_at_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    // audit-round5 fix #3: see `get_byte_at_address`.
    match crate::unsafe_arena_try_get_short(addr) {
        Some(v) => {
            refresh_arena_cache(addr, 2);
            Ok(Some(Value::Int(v as i32)))
        }
        // audit-round6 fix (LOW): see `get_byte_at_address`. A stale cache
        // hit must not mask the validated accessor's freed-arena verdict.
        None => {
            // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw read.
            let mut b = [0u8; 2];
            if real_ptr_read(ctx, addr, &mut b)? {
                return Ok(Some(Value::Int(i16::from_le_bytes(b) as i32)));
            }
            invalidate_arena_cache();
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.getShort: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        }
    }
}

fn native_unsafe_put_short_at_address(
    ctx: &mut dyn NativeContext,
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
    let ok = crate::unsafe_arena_put_short(addr, v);
    if ok {
        refresh_arena_cache(addr, 2);
        return Ok(None);
    }
    // audit-round6 fix (LOW): see `put_byte_at_address`. A stale cache hit
    // must not mask the validated accessor's freed-arena rejection.
    // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw write.
    if real_ptr_write(ctx, addr, &v.to_le_bytes())? {
        return Ok(None);
    }
    invalidate_arena_cache();
    Err(RuntimeError::IllegalArgumentException {
        message: format!("Unsafe.putShort: address 0x{addr:x} is not in any live arena"),
    }
    .into())
}

fn native_unsafe_get_int_at_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    // audit-round5 fix #3: see `get_byte_at_address`.
    match crate::unsafe_arena_try_get_int(addr) {
        Some(v) => {
            refresh_arena_cache(addr, 4);
            Ok(Some(Value::Int(v)))
        }
        // audit-round6 fix (LOW): see `get_byte_at_address`. A stale cache
        // hit must not mask the validated accessor's freed-arena verdict.
        None => {
            // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw read.
            let mut b = [0u8; 4];
            if real_ptr_read(ctx, addr, &mut b)? {
                return Ok(Some(Value::Int(i32::from_le_bytes(b))));
            }
            invalidate_arena_cache();
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.getInt: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        }
    }
}

fn native_unsafe_put_int_at_address(
    ctx: &mut dyn NativeContext,
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
    let ok = crate::unsafe_arena_put_int(addr, v);
    if ok {
        refresh_arena_cache(addr, 4);
        return Ok(None);
    }
    // audit-round6 fix (LOW): see `put_byte_at_address`. A stale cache hit
    // must not mask the validated accessor's freed-arena rejection.
    // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw write.
    if real_ptr_write(ctx, addr, &v.to_le_bytes())? {
        return Ok(None);
    }
    invalidate_arena_cache();
    Err(RuntimeError::IllegalArgumentException {
        message: format!("Unsafe.putInt: address 0x{addr:x} is not in any live arena"),
    }
    .into())
}

fn native_unsafe_get_long_at_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        _ => return Ok(Some(Value::Long(0))),
    };
    // audit-round5 fix #3: see `get_byte_at_address`.
    match crate::unsafe_arena_try_get_long(addr) {
        Some(v) => {
            refresh_arena_cache(addr, 8);
            Ok(Some(Value::Long(v)))
        }
        // audit-round6 fix (LOW): see `get_byte_at_address`. A stale cache
        // hit must not mask the validated accessor's freed-arena verdict.
        None => {
            // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw read.
            let mut b = [0u8; 8];
            if real_ptr_read(ctx, addr, &mut b)? {
                return Ok(Some(Value::Long(i64::from_le_bytes(b))));
            }
            invalidate_arena_cache();
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.getLong: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        }
    }
}

fn native_unsafe_put_long_at_address(
    ctx: &mut dyn NativeContext,
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
    let ok = crate::unsafe_arena_put_long(addr, v);
    if ok {
        refresh_arena_cache(addr, 8);
        return Ok(None);
    }
    // audit-round6 fix (LOW): see `put_byte_at_address`. A stale cache hit
    // must not mask the validated accessor's freed-arena rejection.
    // FIX(bug-A): untagged real pointer (e.g. DirectByteBuffer) → raw write.
    if real_ptr_write(ctx, addr, &v.to_le_bytes())? {
        return Ok(None);
    }
    invalidate_arena_cache();
    Err(RuntimeError::IllegalArgumentException {
        message: format!("Unsafe.putLong: address 0x{addr:x} is not in any live arena"),
    }
    .into())
}

// ---------------------------------------------------------------------------
// 5. freeMemory(long) — release arena. (Real impl, not no-op.)
// ---------------------------------------------------------------------------

fn native_unsafe_free_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
// SECURITY FIX (V5) — single bounds-checked off-heap store.
// ---------------------------------------------------------------------------
//
// Previously CratonVM had TWO disjoint off-heap allocators:
//   * the *tracked* store (deprecated_internal.rs, base 0x1_0000_0000) — wired
//     to allocateMemory / reallocateMemory / setMemory / copyMemory; and
//   * the *arena* store (lib.rs `unsafe_arena`, base 0x10_0000_0000) — wired to
//     the raw single-`long` get/put natives (getByte(J)/putByte(J)/…) and to
//     freeMemory(J).
// The stores never share an address, so an address returned by
// `allocateMemory` was NOT readable through the raw get/put path that
// `java.nio.Bits` uses — it threw IllegalArgumentException, and an address
// freed via the arena path could still be aliased by the tracked store.
//
// These natives consolidate ALL off-heap addressing onto the *arena* store
// (the one with the per-thread UAF cache and bounds-checked accessors). They
// are registered as the LAST word on these (class,name,descriptor) keys in
// BOTH `register_unsafe_wp1_2` (essential-natives mode) and
// `register_unsafe_define_class` (synthetic-overrides mode), so whichever
// registration path runs, the live wiring resolves to a single store.
//
// Base ranges do not collide: the tracked store (now unused by the live
// wiring) starts at 0x1_0000_0000 and the arena store at 0x10_0000_0000.
// Every address the live natives hand out comes from the arena allocator, so
// alloc/realloc/free/get/put/setMemory/copyMemory all agree on the address
// space.

/// Upper bound on a single `setMemory` / `copyMemory` request. Mirrors the
/// 256 MiB cap `lib.rs` applies, so an attacker-controlled `bytes` cannot
/// drive an unbounded loop.
const V5_MAX_OFF_HEAP_OP: usize = 256 * 1024 * 1024;

/// SECURITY FIX (V5): allocateMemory(long) routed to the arena store so the
/// returned address is readable via the raw get/put natives.
fn native_unsafe_allocate_memory_consolidated(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let size = match args.get(1) {
        Some(Value::Long(s)) => *s,
        Some(Value::Int(s)) => *s as i64,
        _ => 0,
    };
    // The JDK's `allocateMemory(long)` bytecode is four steps, and this native
    // replaces all four. MEASURED against HotSpot 25.0.4+7 with
    // `probes/AllocBoundary.java`, which is what fixes the boundary rather
    // than a guess about it:
    //
    //   bytes < 0                    IllegalArgumentException
    //   align8(bytes) overflows      IllegalArgumentException   (MAX and MAX-1)
    //   bytes == 0                   returns 0
    //   2^62-1, 2^40, ...            OutOfMemoryError
    //
    // The overflow rule is not a separate check in the JDK: `alignToHeapWordSize`
    // rounds up to a multiple of 8 FIRST, and for anything above
    // `Long.MAX_VALUE - 7` that wraps negative, so the negative test catches it.
    // Reproducing it as an explicit bound keeps the two cases legible.
    if size < 0 || size > i64::MAX - 7 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("allocateMemory: bad size {size}"),
        }
        .into());
    }
    if size == 0 {
        return Ok(Some(Value::Long(0)));
    }
    match crate::unsafe_arena_try_allocate(size as usize) {
        Some(addr) => Ok(Some(Value::Long(addr))),
        None => Err(RuntimeError::OutOfMemoryError {
            message: format!("Unable to allocate {size} bytes"),
        }
        .into()),
    }
}

/// SECURITY FIX (V5): reallocateMemory(long,long) routed to the arena store.
/// realloc(NULL, size) == alloc(size), matching the JDK contract.
fn native_unsafe_reallocate_memory_consolidated(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let old_addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        _ => 0,
    };
    let new_size = match args.get(2) {
        Some(Value::Long(s)) => *s,
        Some(Value::Int(s)) => *s as i64,
        _ => 0,
    };
    // Same four steps as `allocateMemory`, plus the JDK's zero rule:
    //   `reallocateMemory(address, 0)` FREES the block and returns 0.
    // MEASURED, HotSpot 25.0.4+7 (`UnsafeShadowSweep`, off-heap section).
    if new_size < 0 || new_size > i64::MAX - 7 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("reallocateMemory: bad size {new_size}"),
        }
        .into());
    }
    if new_size == 0 {
        if old_addr != 0 {
            crate::unsafe_arena_free(old_addr);
        }
        invalidate_arena_cache();
        return Ok(Some(Value::Long(0)));
    }
    let addr = if old_addr == 0 {
        crate::unsafe_arena_try_allocate(new_size as usize)
    } else {
        crate::unsafe_arena_try_reallocate(old_addr, new_size as usize)
    };
    // The arena may have moved/resized under this address — drop the
    // per-thread window so a stale range can't mask a later access.
    invalidate_arena_cache();
    match addr {
        Some(a) => Ok(Some(Value::Long(a))),
        None => Err(RuntimeError::OutOfMemoryError {
            message: format!("Unable to allocate {new_size} bytes"),
        }
        .into()),
    }
}

/// SECURITY FIX (V5): setMemory(Object,long,long,byte). The off-heap form
/// (null object) writes into the arena store byte-by-byte through the
/// bounds-checked `put_byte` accessor; the on-heap form (non-null object)
/// preserves the prior bounds-checked array/field behavior.
fn native_unsafe_set_memory_consolidated(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    // A NEGATIVE length is an IllegalArgumentException, not a size-cap
    // violation. MEASURED, HotSpot 25.0.4+7: `setMemory(addr, -1, 0)` throws
    // IAE from the JDK's own `checkSize`. Casting straight to `usize` turned
    // -1 into 2^64-1, which tripped the 256 MiB cap below and reported the
    // wrong contract: `IllegalStateException`.
    let signed_bytes = match args.get(3) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    if signed_bytes < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Unsafe.setMemory: negative length {signed_bytes}"),
        }
        .into());
    }
    let bytes = signed_bytes as usize;
    let value = match args.get(4) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > V5_MAX_OFF_HEAP_OP {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.setMemory size {bytes} exceeds maximum of {V5_MAX_OFF_HEAP_OP} bytes"
            ),
        }
        .into());
    }

    match obj {
        // On-heap target: keep the bounds-checked array fill (and bounded
        // object-field slot fill) that lib.rs already performs.
        Some(obj_ref) => {
            let off = offset as usize;
            if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array
                && ctx.heap_element_type_of(obj_ref) != cratonvm_types::ArrayElementType::Reference
            {
                let fill = vec![value; bytes];
                if crate::unsafe_array_write_bytes(ctx, obj_ref, off, &fill) {
                    return Ok(None);
                }
                return Err(RuntimeError::aioobe_index_only(off as i32).into());
            }
            // Object-field (non-array) target. `bytes` is treated as a slot
            // count in this slot-based model, so it MUST be bounded by the
            // object's real field count — otherwise an attacker-controlled
            // (offset, bytes) walks `set_field` past the last slot and scribbles
            // over neighbouring heap objects (OOB field write). Validate the
            // whole [off, off+bytes) window up-front and reject if it does not
            // fit, rather than partially filling then aborting.
            let num_fields = ctx.object_num_fields(obj_ref);
            let end = match off.checked_add(bytes) {
                Some(e) => e,
                None => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("Unsafe.setMemory: offset {off} + size {bytes} overflows"),
                    }
                    .into());
                }
            };
            if end > num_fields {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe.setMemory: range [{off}, {end}) exceeds object field count {num_fields}"
                    ),
                }
                .into());
            }
            let fill_value = Value::Int(value as i32);
            for i in 0..bytes {
                ctx.set_field(obj_ref, off + i, fill_value);
            }
            Ok(None)
        }
        // Off-heap target: ONE bulk write through the same bridge
        // `copyMemory` uses, not a byte-at-a-time loop.
        //
        // PERF: this was `for i in 0..bytes { unsafe_arena_put_byte(offset+i) }`,
        // and every `put_byte` takes the arena store's `RwLock` for writing and
        // re-runs a `BTreeMap` range probe to find the block. Measured at
        // **~20 ns per byte** — 600x HotSpot's 0.03 ns/byte, and 200x this
        // VM's OWN `copyMemory`, which was already bulk at 0.10 ns/byte. So a
        // 64 KiB fill cost 1.35 ms.
        //
        // `DirectByteBuffer.<init>` zeroes its whole allocation with
        // `UNSAFE.setMemory(base, size, (byte) 0)`, so EVERY
        // `ByteBuffer.allocateDirect(n)` paid 20n ns — 7 ms for a 64 KiB
        // buffer, against HotSpot's 29 us. That is the dominant cost of every
        // direct-buffer workload in the VM: netty's `AdaptivePoolingAllocator`
        // (the 4.2 default) allocates direct chunks, and
        // `PcapWriteHandlerTest.writePcapGreaterThan4Gb` took 294 s here
        // against HotSpot's 3.8 s, blowing both the harness's 180 s process cap
        // and the suite's 120 s per-test default — which is what
        // `pcapwritehandlertest-hang-reopened-20260816` recorded as a HANG.
        //
        // Going through `copy_to_native_memory` also picks up the real-pointer
        // dispatch `copyMemory` already has, so a `setMemory` on a genuine
        // direct-buffer pointer (rather than a tagged arena handle) no longer
        // reports "not in any live arena". A tagged-but-dead handle is still
        // refused before the bridge sees it, exactly as on the copy path — the
        // liveness check must not be allowed to fall through to a raw store.
        None => {
            if crate::unsafe_arena_addr_is_tagged(offset) && !crate::unsafe_arena_contains(offset) {
                invalidate_arena_cache();
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe.setMemory: address 0x{offset:x} is not in any live arena"
                    ),
                }
                .into());
            }
            let fill = vec![value; bytes];
            if !ctx.copy_to_native_memory(offset, &fill) {
                invalidate_arena_cache();
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe.setMemory: address 0x{offset:x} is not in any live arena"
                    ),
                }
                .into());
            }
            if crate::unsafe_arena_contains(offset) {
                refresh_arena_cache(offset, bytes);
            }
            Ok(None)
        }
    }
}

/// SECURITY FIX (V5): copyMemory(Object,long,Object,long,long). The fully
/// off-heap form (both objects null) copies through the NativeContext memory
/// bridge so arena handles and real direct-memory pointers both work; any form
/// touching a heap object delegates to the existing bounds-checked lib.rs
/// handler.
pub(crate) fn native_unsafe_copy_memory_consolidated(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let src_null = matches!(args.get(1), Some(Value::Object(None)) | None);
    let dst_null = matches!(args.get(3), Some(Value::Object(None)) | None);

    // A NEGATIVE length is an IllegalArgumentException on EVERY arm, so the
    // check has to come before the dispatch, not after it. It was below the
    // heap-heap delegation, and the heap-heap arm therefore kept reporting the
    // 256 MiB size cap's `IllegalStateException` -- the guard was in the
    // function the caller never reached. MEASURED, HotSpot 25.0.4+7:
    // `copyMemory(byte[], base, byte[], base, -1)` -> IllegalArgumentException.
    if matches!(args.get(5), Some(Value::Long(b)) if *b < 0)
        || matches!(args.get(5), Some(Value::Int(b)) if *b < 0)
    {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Unsafe.copyMemory: negative length".to_string(),
        }
        .into());
    }

    // Heap↔heap: keep lib.rs's bounds-checked array/field copy.
    if !src_null && !dst_null {
        return crate::native_unsafe_copy_memory(ctx, args);
    }

    let src_addr = match args.get(2) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        _ => 0,
    };
    let dst_addr = match args.get(4) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        _ => 0,
    };
    // See `native_unsafe_set_memory_consolidated`: a negative length is an
    // IllegalArgumentException and the 256 MiB cap is a different contract.
    let signed_bytes = match args.get(5) {
        Some(Value::Long(b)) => *b,
        Some(Value::Int(b)) => *b as i64,
        _ => 0,
    };
    if signed_bytes < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Unsafe.copyMemory: negative length {signed_bytes}"),
        }
        .into());
    }
    let bytes = signed_bytes as usize;
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > V5_MAX_OFF_HEAP_OP {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.copyMemory size {bytes} exceeds maximum of {V5_MAX_OFF_HEAP_OP} bytes"
            ),
        }
        .into());
    }

    // MIXED heap↔off-heap copies. The fully-off-heap loop below only handles
    // arena↔arena, and `native_unsafe_copy_memory` only handles heap↔heap, so
    // without this a heap↔off-heap copy was silently DROPPED. This is the path
    // `DirectByteBuffer.put(heapBuffer)` / `get(heapBuffer)` takes (via
    // `ScopedMemoryAccess.copyMemory` → `Unsafe.copyMemory`), which is exactly
    // how `IOUtil` fills/drains the temp direct buffer in the FileChannel /
    // SocketChannel heap-buffer path. The off-heap side is routed through
    // `copy_*_native_memory` so an `Unsafe.allocateMemory` arena handle lands
    // in the off-heap store (and a real pointer falls through to a raw copy).
    if !src_null && dst_null {
        // heap src → off-heap dst
        let src_obj = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let buf = crate::unsafe_array_read_bytes(ctx, src_obj, src_addr as usize, bytes)
            .ok_or_else(|| RuntimeError::aioobe_index_only(src_addr as i32))?;
        if !ctx.copy_to_native_memory(dst_addr, &buf) {
            invalidate_arena_cache();
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "Unsafe.copyMemory: dst address 0x{dst_addr:x} is not addressable"
                ),
            }
            .into());
        }
        invalidate_arena_cache();
        return Ok(None);
    }
    if src_null && !dst_null {
        // off-heap src → heap dst
        let dst_obj = match args.get(3) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let mut buf = vec![0u8; bytes];
        if !ctx.copy_from_native_memory(src_addr, &mut buf) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "Unsafe.copyMemory: src address 0x{src_addr:x} is not addressable"
                ),
            }
            .into());
        }
        if !crate::unsafe_array_write_bytes(ctx, dst_obj, dst_addr as usize, &buf) {
            return Err(RuntimeError::aioobe_index_only(dst_addr as i32).into());
        }
        return Ok(None);
    }

    // Fully off-heap copy. Source/destination may be either tagged VM arena
    // handles (`Unsafe.allocateMemory`) or real native pointers used by direct
    // buffers. Keep tagged-but-not-live handles on the arena error path so a
    // freed/shrunk arena is never dereferenced as a raw pointer.
    if crate::unsafe_arena_addr_is_tagged(src_addr) && !crate::unsafe_arena_contains(src_addr) {
        invalidate_arena_cache();
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "Unsafe.copyMemory: src address 0x{src_addr:x} is not in any live arena"
            ),
        }
        .into());
    }
    if crate::unsafe_arena_addr_is_tagged(dst_addr) && !crate::unsafe_arena_contains(dst_addr) {
        invalidate_arena_cache();
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "Unsafe.copyMemory: dst address 0x{dst_addr:x} is not in any live arena"
            ),
        }
        .into());
    }

    // Read the full source range before writing the destination. That preserves
    // memmove-like behaviour for overlapping arena/native ranges and lets the
    // NativeContext do the arena-vs-real-pointer dispatch in one place.
    let mut buf = vec![0u8; bytes];
    if !ctx.copy_from_native_memory(src_addr, &mut buf) {
        invalidate_arena_cache();
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Unsafe.copyMemory: src address 0x{src_addr:x} is not addressable"),
        }
        .into());
    }
    if !ctx.copy_to_native_memory(dst_addr, &buf) {
        invalidate_arena_cache();
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Unsafe.copyMemory: dst address 0x{dst_addr:x} is not addressable"),
        }
        .into());
    }
    if crate::unsafe_arena_contains(dst_addr) {
        refresh_arena_cache(dst_addr, bytes);
    }
    Ok(None)
}

/// SECURITY FIX (V5): register the consolidated off-heap memory natives so
/// allocate/reallocate/free/setMemory/copyMemory AND every raw single-`long`
/// get/put all resolve to the SAME (arena) store. Called from both
/// `register_unsafe_wp1_2` and `register_unsafe_define_class` so it wins as
/// the last registration in essential-only AND synthetic-overrides modes.
pub(crate) fn register_consolidated_off_heap_store(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let u = "sun/misc/Unsafe";
    let u2 = "jdk/internal/misc/Unsafe";

    // allocate / reallocate / free — sun.misc + jdk.internal.misc (0-suffix).
    registry.register(
        u,
        "allocateMemory",
        "(J)J",
        native_unsafe_allocate_memory_consolidated,
    );
    registry.register(
        u,
        "reallocateMemory",
        "(JJ)J",
        native_unsafe_reallocate_memory_consolidated,
    );
    registry.register(u, "freeMemory", "(J)V", native_unsafe_free_memory);
    registry.register_with_kind(
        u2,
        "allocateMemory0",
        "(J)J",
        native_unsafe_allocate_memory_consolidated,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        u2,
        "reallocateMemory0",
        "(JJ)J",
        native_unsafe_reallocate_memory_consolidated,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        u2,
        "freeMemory0",
        "(J)V",
        native_unsafe_free_memory,
        NativeKind::Bridge,
    );
    // Some JDK builds expose the unsuffixed forms on jdk.internal.misc.Unsafe.
    registry.register(
        u2,
        "allocateMemory",
        "(J)J",
        native_unsafe_allocate_memory_consolidated,
    );
    registry.register(
        u2,
        "reallocateMemory",
        "(JJ)J",
        native_unsafe_reallocate_memory_consolidated,
    );
    registry.register(u2, "freeMemory", "(J)V", native_unsafe_free_memory);

    // setMemory / copyMemory — off-heap form routed to the arena, on-heap
    // form delegated to the bounds-checked lib.rs handlers.
    registry.register(
        u,
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        native_unsafe_set_memory_consolidated,
    );
    registry.register(
        u2,
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        native_unsafe_set_memory_consolidated,
    );
    registry.register_with_kind(
        u2,
        "setMemory0",
        "(Ljava/lang/Object;JJB)V",
        native_unsafe_set_memory_consolidated,
        NativeKind::Bridge,
    );
    registry.register(
        u,
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        native_unsafe_copy_memory_consolidated,
    );
    registry.register(
        u2,
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        native_unsafe_copy_memory_consolidated,
    );
    registry.register_with_kind(
        u2,
        "copyMemory0",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        native_unsafe_copy_memory_consolidated,
        NativeKind::Bridge,
    );
    registry.set_category(__prev_cat);
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

// `pub(crate)` for the same reason as `native_unsafe_static_field_base`:
// `unsafe_natives_ext::register_unsafe_natives` used to register an
// always-null `defineClass` closure here. It happened to be overwritten again
// by `register_unsafe_define_class`, but only by ordering luck — point the
// earlier registration at the real implementation so the outcome no longer
// depends on which registrar happens to run last.
pub(crate) fn native_unsafe_define_class(
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
                class_name: if name.is_empty() {
                    "<anonymous>".into()
                } else {
                    name
                },
                message: "defineClass: byte[] is null".into(),
            }
            .into())
        }
    };
    let off = match args.get(3) {
        Some(Value::Int(o)) if *o >= 0 => *o as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
        _ => 0,
    };
    let len = match args.get(4) {
        Some(Value::Int(l)) if *l >= 0 => *l as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
        _ => 0,
    };
    let arr_len = ctx.array_length(byte_array);
    if off.saturating_add(len) > arr_len {
        return Err(RuntimeError::aioobe_index_only(off.saturating_add(len) as i32).into());
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

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => crate::classloader::extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

    let slashed_name = name.replace('.', "/");
    // SECURITY FIX (V9): do NOT unconditionally skip verification.
    // Previously `skip_verification: true` was hard-coded on the theory that
    // Unsafe.defineClass bytes always come from a privileged caller. But
    // Unsafe is reachable from attacker-influenced code (deserialization,
    // reflection bridges), so skipping the verifier on attacker-controlled
    // bytes would let a malformed/hostile class file bypass bytecode safety
    // checks. Gate the skip behind the same process-wide native-access trust
    // gate used elsewhere (`panama::native_access_enabled`, secure-by-default
    // false). When native access is granted the host has opted into trusting
    // privileged native paths (e.g. ByteBuddy/CGLIB toolchains under
    // --enable-native-access), so we preserve the HotSpot fast path; otherwise
    // we VERIFY. Ideally this would be a per-caller trusted-caller predicate,
    // but no such hook is reachable from this native — the native-access gate
    // is the closest trust signal available here.
    let skip_verification = crate::panama::native_access_enabled();
    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        skip_verification,
        // BUG-10: `Unsafe.defineClass` is the privileged, all-powerful define
        // path HotSpot routes around `preDefineClass`'s prohibited-package
        // guard. ByteBuddy's `ClassInjector$UsingUnsafe` relies on this to
        // inject `java.lang.ClassLoader$ByteBuddyAccessor$V1` (used by AssertJ,
        // Mockito, …). Mark the define privileged so the H5 guard is bypassed,
        // matching the real JVM. Verification stays independently gated above.
        privileged_define: true,
        ..Default::default()
    };
    match ctx.define_class_full(&slashed_name, &bytes, loader_id, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(LinkageError::ClassFormatError {
            class_name: if slashed_name.is_empty() {
                "<anonymous>".into()
            } else {
                slashed_name
            },
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
    let host_prefix = host_internal_name.as_deref().unwrap_or("anonymous");
    let id =
        crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let hidden_name = format!("{host_prefix}/0x{id:x}");

    // 4. Define under the same loader as the host class. We currently
    //    flatten to the application loader (loader_id = 0); the backend
    //    treats hidden classes as living in their host's namespace via
    //    `nest_host_class_name`, so visibility still resolves correctly.
    // SECURITY FIX (V9): gate `skip_verification` behind the native-access
    // trust gate (secure-by-default false) rather than hard-coding `true`.
    // defineAnonymousClass takes attacker-influenceable bytes just like
    // defineClass; only skip the verifier when the host has granted native
    // access (a deliberate trust opt-in), otherwise verify the bytes. See the
    // companion note in `native_unsafe_define_class`.
    let opts = cratonvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: crate::panama::native_access_enabled(),
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
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let u = "sun/misc/Unsafe";
    let u2 = "jdk/internal/misc/Unsafe";

    // SECURITY FIX (V5): in synthetic-overrides mode `register_unsafe_natives`
    // runs AGAIN (re-wiring allocateMemory to a heap byte array and freeMemory
    // to a no-op) AFTER `register_unsafe_wp1_2` already consolidated the store.
    // This function is the last unsafe-related registration on that path, so
    // re-assert the single arena store here to keep the live wiring consistent
    // in BOTH essential-only and synthetic modes.
    register_consolidated_off_heap_store(r);

    // Legacy `sun.misc.Unsafe.defineClass`.
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

    // `defineAnonymousClass` RETIRED 2026-08-29. Its comment justified it as
    // "JDK 8 surface, ByteBuddy still emits" -- and that premise is not true of
    // any image this VM supports: the method is ABSENT from 17, 21 and 25, so a
    // caller emitting it, from bytecode or reflectively, fails resolution
    // against the real class no matter what is registered here. A guard scoped
    // by a stated premise is only as good as the premise. Evidence for the
    // whole retirement is on the `monitorEnter` note in `unsafe_natives_ext.rs`.
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 7. getLoadAverage0([D,I)I — platform load average.
// ---------------------------------------------------------------------------
//
// On POSIX, mirror `getloadavg(3)` into the double[]. On Windows, load
// average is not an OS concept; return 0 (matches real HotSpot on
// Windows). `nelems` is clamped to 3; the JDK spec caps at 3.

fn native_unsafe_get_load_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [this, double[] dest, int nelems]
    // The JDK's wrapper is
    //   if (nelems < 0 || nelems > 3 || nelems > loadavg.length)
    //       throw new ArrayIndexOutOfBoundsException();
    // and a null array fails on `.length` first. MEASURED, HotSpot 25.0.4+7:
    // `getLoadAverage(null, 1)` -> NullPointerException,
    // `getLoadAverage(new double[1], 3)` -> ArrayIndexOutOfBoundsException.
    // CratonVM answered 0 and silently CLAMPED. The clamp meant no memory was
    // ever written out of bounds -- so this is a contract defect, not a safety
    // one -- but a caller that sizes its array from the return value is told
    // that three samples exist in a one-element array.
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Unsafe.getLoadAverage: null array".to_string()),
            }
            .into())
        }
    };
    let requested = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    if requested < 0 || requested > 3 || (requested as usize) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: requested,
            message: Some(format!(
                "Unsafe.getLoadAverage: nelems {requested} out of range for an array of {arr_len}"
            )),
        }
        .into());
    }
    let nelems = requested as usize;
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

// `pub(crate)`: also wired from `unsafe_natives_ext::register_unsafe_natives`,
// which runs a SECOND time in synthetic mode (inside
// `register_synthetic_overrides`) AFTER `register_unsafe_wp1_2` below. Before
// that call site pointed here it re-registered an always-null closure, so
// synthetic mode silently lost this real implementation. See the stub-removal
// wave-2 report ("competing registration").
pub(crate) fn native_unsafe_static_field_base(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, Field]
    // MEASURED, HotSpot 25.0.4+7: a null Field is a `NullPointerException` and
    // an INSTANCE field is an `IllegalArgumentException`. CratonVM answered
    // `null` for the first and the declaring class's mirror for the second --
    // and a mirror is a usable addressing base, so the caller's next
    // `putInt(base, offset, v)` wrote into the static area at an offset that
    // was resolved in the INSTANCE space.
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Unsafe.staticFieldBase: null Field".to_string()),
            }
            .into())
        }
    };
    let (is_static, _cid, _slot, _desc) = crate::lang_class::read_field_meta(ctx, field_obj);
    if !is_static {
        return Err(RuntimeError::IllegalArgumentException {
            message: "not a static field".to_string(),
        }
        .into());
    }
    if let Some((class_id, _name)) = crate::lang_class::field_class_and_name(ctx, field_obj) {
        let mirror = ctx.get_class_mirror(class_id);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// 9. Release/Acquire fences — per JMM.
// ---------------------------------------------------------------------------

fn native_unsafe_acquire_fence(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    Ok(None)
}

fn native_unsafe_release_fence(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    Ok(None)
}

// ---------------------------------------------------------------------------
// compareAndExchange family (JDK 9+).
// ---------------------------------------------------------------------------
//
// `compareAndExchange*` differ from `compareAndSet*` only in their return
// value: instead of a bool, they return the value that WAS in the field
// at the moment of the CAS attempt.  On success the returned value equals
// `expected`; on failure it equals whatever the field actually holds.
// Spec-conformance allows a benign read/CAS race — the returned value is
// the "observed before CAS" value, which is sufficient for the
// retry-loop callers in `j.u.c.atomic.*` and `j.l.invoke.VarHandle`.
//
// Memory-order variants (`Acquire`, `Release`, plain) all alias the
// strong CAS — see the weak-CAS note above for the rationale.

fn cae_int_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let offset = crate::unsafe_offset(args, 2);
    let expected_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let expected = Value::Int(expected_i);
    let update = Value::Int(update_i);
    match crate::unsafe_obj(args, 1) {
        None => {
            if let Some(old) =
                crate::unsafe_compare_exchange_static_field(ctx, offset, expected, update)
            {
                return Ok(Some(match old {
                    Value::Int(v) => Value::Int(v),
                    _ => Value::Int(expected_i),
                }));
            }
            let _ = crate::native_unsafe_cas_int(ctx, args)?;
            Ok(Some(Value::Int(expected_i)))
        }
        Some(obj) => {
            let old = if crate::is_synthetic_offset(offset) {
                crate::unsafe_compare_exchange_synthetic_field(ctx, obj, offset, expected, update)
            } else {
                let index = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                    match crate::unsafe_checked_array_index(ctx, obj, offset) {
                        Some(i) => i,
                        None => return Ok(Some(Value::Int(expected_i))),
                    }
                } else {
                    offset
                };
                crate::unsafe_compare_exchange_heap_slot(ctx, obj, index, expected, update)
            };
            Ok(Some(match old {
                Value::Int(v) => Value::Int(v),
                _ => Value::Int(expected_i),
            }))
        }
    }
}

fn cae_long_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let offset = crate::unsafe_offset(args, 2);
    let expected_l = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update_l = match args.get(4) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let expected = Value::Long(expected_l);
    let update = Value::Long(update_l);
    match crate::unsafe_obj(args, 1) {
        None => {
            if let Some(old) =
                crate::unsafe_compare_exchange_static_field(ctx, offset, expected, update)
            {
                return Ok(Some(match old {
                    Value::Long(v) => Value::Long(v),
                    _ => Value::Long(expected_l),
                }));
            }
            let _ = crate::native_unsafe_cas_long(ctx, args)?;
            Ok(Some(Value::Long(expected_l)))
        }
        Some(obj) => {
            let old = if crate::is_synthetic_offset(offset) {
                crate::unsafe_compare_exchange_synthetic_field(ctx, obj, offset, expected, update)
            } else {
                let index = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                    match crate::unsafe_checked_array_index(ctx, obj, offset) {
                        Some(i) => i,
                        None => return Ok(Some(Value::Long(expected_l))),
                    }
                } else {
                    offset
                };
                crate::unsafe_compare_exchange_heap_slot(ctx, obj, index, expected, update)
            };
            Ok(Some(match old {
                Value::Long(v) => Value::Long(v),
                _ => Value::Long(expected_l),
            }))
        }
    }
}

fn cae_object_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let offset = crate::unsafe_offset(args, 2);
    let expected = crate::recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let update = crate::recover_object_arg(args.get(4).copied().unwrap_or(Value::Object(None)));
    match crate::unsafe_obj(args, 1) {
        None => {
            if let Some(old) =
                crate::unsafe_compare_exchange_static_field(ctx, offset, expected, update)
            {
                return Ok(Some(crate::recover_object_arg(old)));
            }
            let _ = crate::native_unsafe_cas_object(ctx, args)?;
            Ok(Some(crate::recover_object_arg(expected)))
        }
        Some(obj) => {
            let old = if crate::is_synthetic_offset(offset) {
                crate::unsafe_compare_exchange_synthetic_field(ctx, obj, offset, expected, update)
            } else {
                let index = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                    match crate::unsafe_checked_array_index(ctx, obj, offset) {
                        Some(i) => i,
                        None => return Ok(Some(Value::Object(None))),
                    }
                } else {
                    offset
                };
                crate::unsafe_compare_exchange_heap_slot(ctx, obj, index, expected, update)
            };
            Ok(Some(crate::recover_object_arg(old)))
        }
    }
}

fn native_unsafe_cae_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cae_int_impl(ctx, args)
}
fn native_unsafe_cae_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cae_long_impl(ctx, args)
}
fn native_unsafe_cae_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cae_object_impl(ctx, args)
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
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
        u2,
        "weakCompareAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_weak_cas_object,
    );

    // 2. park with blocker (Object, long) RETIRED 2026-08-29: absent from every
    // supported image. `park(ZJ)V` -- the overload the JDK does declare -- stays
    // registered and takes 781 invocations across 16 corpus vectors in strict
    // mode, which is what makes this row's zero mean something.
    // `native_unsafe_park_with_blocker` itself is retained: a unit test below
    // calls it directly.

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
        registry.register(
            class,
            "getShort",
            "(J)S",
            native_unsafe_get_short_at_address,
        );
        registry.register(
            class,
            "putShort",
            "(JS)V",
            native_unsafe_put_short_at_address,
        );
        registry.register(class, "getChar", "(J)C", native_unsafe_get_short_at_address);
        registry.register(
            class,
            "putChar",
            "(JC)V",
            native_unsafe_put_short_at_address,
        );
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
            // audit-round5 fix #3: IAE on out-of-arena address (mirror
            // of the put-side and `get_byte_at_address` semantics).
            match crate::unsafe_arena_try_get_int(addr) {
                Some(v) => {
                    refresh_arena_cache(addr, 4);
                    Ok(Some(Value::Float(f32::from_bits(v as u32))))
                }
                // audit-round6 fix (LOW): see `get_byte_at_address`. Drop
                // the stale cache rather than let it mask the freed verdict.
                None => {
                    invalidate_arena_cache();
                    Err(RuntimeError::IllegalArgumentException {
                        message: format!(
                            "Unsafe.getFloat: address 0x{addr:x} is not in any live arena"
                        ),
                    }
                    .into())
                }
            }
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
                return Ok(None);
            }
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.putFloat: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        });
        registry.register(class, "getDouble", "(J)D", |_ctx, args| {
            let addr = match args.get(1) {
                Some(Value::Long(a)) => *a,
                _ => return Ok(Some(Value::Double(0.0))),
            };
            // audit-round5 fix #3: IAE on out-of-arena address.
            match crate::unsafe_arena_try_get_long(addr) {
                Some(v) => {
                    refresh_arena_cache(addr, 8);
                    Ok(Some(Value::Double(f64::from_bits(v as u64))))
                }
                // audit-round6 fix (LOW): see `get_byte_at_address`. Drop
                // the stale cache rather than let it mask the freed verdict.
                None => {
                    invalidate_arena_cache();
                    Err(RuntimeError::IllegalArgumentException {
                        message: format!(
                            "Unsafe.getDouble: address 0x{addr:x} is not in any live arena"
                        ),
                    }
                    .into())
                }
            }
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
                return Ok(None);
            }
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsafe.putDouble: address 0x{addr:x} is not in any live arena"),
            }
            .into())
        });
        // getAddress / putAddress use native pointer width (8 on our 64-bit VM).
        registry.register(
            class,
            "getAddress",
            "(J)J",
            native_unsafe_get_long_at_address,
        );
        registry.register(
            class,
            "putAddress",
            "(JJ)V",
            native_unsafe_put_long_at_address,
        );
    }

    // 5. Real freeMemory. Overrides the no-op in lib.rs.
    // (NativeMethodRegistry::register overwrites prior entries with
    // the same key, so this becomes the live impl.)
    registry.register(u, "freeMemory", "(J)V", native_unsafe_free_memory);
    registry.register_with_kind(
        u2,
        "freeMemory0",
        "(J)V",
        native_unsafe_free_memory,
        NativeKind::Bridge,
    );

    // SECURITY FIX (V5): consolidate allocate/reallocate/free/setMemory/
    // copyMemory AND the raw get/put natives onto the SINGLE arena store.
    // This is the last memory-native registration in essential-natives mode,
    // so it overrides the disjoint tracked-store wiring from
    // deprecated_internal and the heap-array wiring from
    // register_unsafe_natives.
    register_consolidated_off_heap_store(registry);

    // 6. Real defineClass replacing the stub.
    registry.register_with_kind(
        u2,
        "defineClass0",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
        NativeKind::Bridge,
    );

    // 7. getLoadAverage0.
    registry.register_with_kind(
        u2,
        "getLoadAverage0",
        "([DI)I",
        native_unsafe_get_load_average,
        NativeKind::Bridge,
    );
    registry.register(
        u,
        "getLoadAverage",
        "([DI)I",
        native_unsafe_get_load_average,
    );

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
    registry.register_with_kind(
        u2,
        "staticFieldBase0",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        native_unsafe_static_field_base,
        NativeKind::Bridge,
    );

    // 9. Acquire/Release fences.
    for class in &[u, u2] {
        registry.register(class, "acquireFence", "()V", native_unsafe_acquire_fence);
        registry.register(class, "releaseFence", "()V", native_unsafe_release_fence);
    }

    // 10. Volatile Object/Reference getter already registered in lib.rs,
    // but the Plain-suffixed aliases for VarHandle.PlainSet/PlainGet are
    // not. Map them to the same underlying impls.
    // `getReferencePlain` / `putReferencePlain` RETIRED 2026-08-29: absent from
    // every supported image. They aliased `native_unsafe_{get,put}_object`, so
    // even the bodies were duplicates of registrations that remain live under
    // the names the JDK does declare.

    // 11. getAndAddByte / getAndAddShort — uncommon but present in JDK 25
    // for VarHandle arithmetic on sub-int widths. Implement via strong CAS
    // loop in a slot.
    registry.register(
        u2,
        "getAndAddByte",
        "(Ljava/lang/Object;JB)B",
        native_unsafe_get_and_add_int_from_unsafe,
    );
    registry.register(
        u2,
        "getAndAddShort",
        "(Ljava/lang/Object;JS)S",
        native_unsafe_get_and_add_int_from_unsafe,
    );

    // 12. compareAndExchange* family (JDK 9+). Strong CAS that returns the
    // value SEEN (== expected on success, != expected on failure). All
    // memory-order variants alias strong (see weak-CAS note above for
    // rationale — stronger-than-required is always spec-conformant).
    let int_desc = "(Ljava/lang/Object;JII)I";
    let long_desc = "(Ljava/lang/Object;JJJ)J";
    let ref_desc = "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

    // Only the un-suffixed form is ACC_NATIVE on JDK 25 (both images).
    //
    // CORRECTED 2026-09-11: the rest of this note used to read "the
    // `Acquire`/`Release`/`weak*` spellings are not declared at all - the JDK
    // lowers those to the plain form in `VarHandle` - so they stay ambient",
    // and it is right about ONE of those two groups. `javap -p
    // jdk.internal.misc.Unsafe` on 17, 21 AND 25:
    //
    //   public final int compareAndExchangeIntAcquire(Object, long, int, int);
    //   public final boolean weakCompareAndSetIntAcquire(Object, long, int, int);
    //
    // The `compareAndExchange*{Acquire,Release}` and `weakCompareAndSet*`
    // spellings ARE declared - ordinary Java methods carrying Code, which is
    // what makes them retirable §1.4 shadows, and they are retired in
    // `RETIRED_SHADOW_L5S_TRIPLES`. What is genuinely undeclared on all three
    // images is the `weakCompareAndExchange*` family below, which is a
    // DELETION candidate rather than an ambient registration.
    registry.register_with_kind(
        u2,
        "compareAndExchangeInt",
        int_desc,
        native_unsafe_cae_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    for name in [
        "compareAndExchangeIntAcquire",
        "compareAndExchangeIntRelease",
        "weakCompareAndExchangeInt",
        "weakCompareAndExchangeIntAcquire",
        "weakCompareAndExchangeIntRelease",
    ] {
        registry.register(u2, name, int_desc, native_unsafe_cae_int);
    }

    // Only the un-suffixed form is ACC_NATIVE on JDK 25 (both images). The
    // `Acquire`/`Release`/`weak*` spellings are not declared at all — the JDK
    // lowers those to the plain form in `VarHandle` — so they stay ambient.
    registry.register_with_kind(
        u2,
        "compareAndExchangeLong",
        long_desc,
        native_unsafe_cae_long,
        cratonvm_native_api::NativeKind::Bridge,
    );
    for name in [
        "compareAndExchangeLongAcquire",
        "compareAndExchangeLongRelease",
        "weakCompareAndExchangeLong",
        "weakCompareAndExchangeLongAcquire",
        "weakCompareAndExchangeLongRelease",
    ] {
        registry.register(u2, name, long_desc, native_unsafe_cae_long);
    }

    // `compareAndExchangeReference` is the one ACC_NATIVE spelling on JDK 25
    // (both images). The memory-order variants and the legacy `Object` names
    // are not declared anywhere -- `VarHandle` lowers them to the plain form --
    // and neither is anything on `sun.misc.Unsafe`, so only the first states
    // its kind.
    registry.register_with_kind(
        u2,
        "compareAndExchangeReference",
        ref_desc,
        native_unsafe_cae_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    for name in [
        "compareAndExchangeReferenceAcquire",
        "compareAndExchangeReferenceRelease",
        "compareAndExchangeObject",
        "weakCompareAndExchangeReference",
        "weakCompareAndExchangeReferenceAcquire",
        "weakCompareAndExchangeReferenceRelease",
        "weakCompareAndExchangeObject",
    ] {
        registry.register(u2, name, ref_desc, native_unsafe_cae_object);
        // Legacy sun.misc.Unsafe Object naming.
        if name.ends_with("Object") {
            registry.register(u, name, ref_desc, native_unsafe_cae_object);
        }
    }
    registry.set_category(__prev_cat);
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

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
    fn compare_and_exchange_reference_returns_varhandle_witness() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(1), 1);
        let current = ctx.create_string("current");
        let replacement = ctx.create_string("replacement");
        let stale = ctx.create_string("stale");
        ctx.set_field(obj, 0, Value::Object(Some(current)));

        let failed = native_unsafe_cae_object(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Object(None),
                Value::Object(Some(stale)),
            ],
        )
        .expect("compareAndExchangeReference")
        .expect("witness");
        assert_eq!(failed, Value::Object(Some(current)));
        assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(current)));

        let witness = native_unsafe_cae_object(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Object(Some(current)),
                Value::Object(Some(replacement)),
            ],
        )
        .expect("compareAndExchangeReference success")
        .expect("success witness");
        assert_eq!(witness, Value::Object(Some(current)));
        assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(replacement)));
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

    /// `setMemory` on an off-heap arena must fill the whole range, refuse a
    /// range that leaves the block, and leave the neighbouring bytes alone.
    ///
    /// Guards the bulk rewrite: the byte-at-a-time loop this replaced took the
    /// arena write lock and re-probed the block's `BTreeMap` entry per byte
    /// (~20 ns/byte, so 1.35 ms for a 64 KiB fill), and it also filled up to
    /// the first out-of-range byte BEFORE throwing — the fill is all-or-nothing
    /// now.
    #[test]
    fn set_memory_off_heap_fills_in_bulk_and_stays_in_bounds() {
        let _arena_lock = crate::arena_test_lock();
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(64);

        native_unsafe_set_memory_consolidated(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(None),
                Value::Long(addr + 8),
                Value::Long(16),
                Value::Int(0xAB),
            ],
        )
        .unwrap();
        for i in 0..64i64 {
            let want = if (8..24).contains(&i) { 0xAB } else { 0x00 };
            assert_eq!(
                crate::unsafe_arena_get_byte(addr + i),
                want,
                "byte {i} of the arena"
            );
        }

        // A range that runs off the end of the block is refused, and refused
        // WITHOUT having written any of it.
        let err = native_unsafe_set_memory_consolidated(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(None),
                Value::Long(addr + 60),
                Value::Long(16),
                Value::Int(0xCD),
            ],
        );
        assert!(
            err.is_err(),
            "a fill past the end of the block must be refused"
        );
        for i in 60..64i64 {
            assert_eq!(
                crate::unsafe_arena_get_byte(addr + i),
                0x00,
                "byte {i} was partially filled by a refused setMemory"
            );
        }

        crate::unsafe_arena_free(addr);
    }

    /// A freed (tagged-but-dead) handle must keep reporting the use-after-free
    /// rather than reaching the bridge, where it would be treated as a raw
    /// pointer and stored through.
    #[test]
    fn set_memory_refuses_a_freed_arena_handle() {
        let _arena_lock = crate::arena_test_lock();
        let mut ctx = MockNativeContext::new();
        let addr = crate::unsafe_arena_allocate(32);
        crate::unsafe_arena_free(addr);
        let err = native_unsafe_set_memory_consolidated(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(None),
                Value::Long(addr),
                Value::Long(8),
                Value::Int(1),
            ],
        );
        assert!(err.is_err(), "setMemory on a freed handle must be refused");
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
        let got =
            native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
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
        let got = native_unsafe_get_long_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)])
            .unwrap();
        assert_eq!(got, Some(Value::Long(i64::MIN)));
        crate::unsafe_arena_free(addr);
    }

    #[test]
    fn free_memory_evicts_arena() {
        let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
                                                    // FIX: this test previously asserted that a getInt after freeMemory
                                                    // returned Some(Int(0)) — the obsolete "free is a no-op, reads return
                                                    // 0" contract. Under the consolidated bounds-checked arena (SECURITY
                                                    // FIX V5/V9), freeMemory EVICTS the block: a subsequent access is a
                                                    // use-after-free and must be rejected ("not in any live arena"). The
                                                    // old assertion's .unwrap() panicked on that (correct) Err. Rewritten
                                                    // to exercise the live consolidated arena path and to genuinely prove
                                                    // eviction: writes/reads succeed while live, then error after free.
        let mut ctx = MockNativeContext::new();
        // Allocate through the consolidated arena allocator — the same path
        // `register_consolidated_off_heap_store` wires `allocateMemory` to via
        // `native_unsafe_allocate_memory_consolidated`. Use the RETURNED addr.
        let addr = crate::unsafe_arena_allocate(32);

        // While live: putInt and getInt round-trip successfully.
        native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Int(42)],
        )
        .unwrap();
        let live =
            native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
        assert_eq!(
            live,
            Some(Value::Int(42)),
            "read must succeed while arena is live"
        );

        // Free evicts the arena block.
        native_unsafe_free_memory(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();

        // After free, the address is no longer in any live arena: getInt must
        // error (use-after-free rejected) rather than silently returning 0.
        let after_free =
            native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]);
        assert!(
            after_free.is_err(),
            "getInt after free must be rejected (arena evicted), got {after_free:?}",
        );

        // putInt to the freed address is likewise rejected.
        let put_after_free = native_unsafe_put_int_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Int(7)],
        );
        assert!(
            put_after_free.is_err(),
            "putInt after free must be rejected (arena evicted), got {put_after_free:?}",
        );
    }

    /// R1 (silent-corruption fix): a *real* OS pointer must never be
    /// mis-classified as an arena handle, even when it falls numerically
    /// inside a live arena block's `[base, base+len)` range. Before the
    /// fix, `contains` was pure range membership and a real `allocateDirect`
    /// pointer ≥ 64 GiB landing inside a block would be silently routed to
    /// the off-heap store. Tagging every handle with bit 62 — which no real
    /// Windows/Linux user-mode pointer can have set — makes the two spaces
    /// provably disjoint.
    #[test]
    fn real_pointer_in_arena_numeric_range_is_not_misrouted() {
        let _arena_lock = crate::arena_test_lock(); // shared global arena
        let addr = crate::unsafe_arena_allocate(4096);

        // The handle itself is in-arena.
        assert!(
            crate::unsafe_arena_contains(addr),
            "freshly allocated handle {addr:#x} must be in-arena",
        );
        // Every handle carries the high tag bit (bit 62).
        assert_ne!(
            addr & (1i64 << 62),
            0,
            "arena handle {addr:#x} must carry the reserved tag bit",
        );

        // Synthesize a "real-looking" pointer with the SAME low bits as the
        // handle but the tag bit cleared — i.e. an address in the historical
        // 2^36-based numeric range a real OS pointer could occupy. It must NOT
        // be classified as in-arena, so `copy_*_native_memory` would take the
        // raw path instead of corrupting the off-heap store.
        let real_like = addr & !(1i64 << 62);
        assert!(
            real_like >= 0x10_0000_0000,
            "sanity: stripped address {real_like:#x} is in the dangerous low range",
        );
        assert!(
            !crate::unsafe_arena_contains(real_like),
            "real pointer {real_like:#x} (handle with tag stripped) must NOT be \
             mis-classified as an arena handle",
        );

        // A range read at the real-looking address must also be refused so the
        // NIO routing falls through to the raw pointer path.
        let mut out = [0u8; 8];
        assert!(
            !crate::unsafe_arena_copy_out(real_like, &mut out),
            "copy_out at a non-handle address must fail (forces raw path)",
        );

        crate::unsafe_arena_free(addr);
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
        assert!(
            result.is_err(),
            "expected ClassFormatError, got {:?}",
            result
        );
    }

    #[test]
    fn invoke_cleaner_idempotent_on_double_call() {
        let mut ctx = MockNativeContext::new();
        let buf = ctx.alloc_object(ClassId::new(1), 1);
        let r1 = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(Some(buf))])
            .unwrap();
        let r2 = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(Some(buf))])
            .unwrap();
        assert_eq!(r1, None);
        assert_eq!(r2, None);
    }

    #[test]
    fn invoke_cleaner_null_buffer_throws_illegal_arg() {
        let mut ctx = MockNativeContext::new();
        let r = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(None)]);
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
        native_unsafe_free_memory(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
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
        native_unsafe_put_int_at_address(&mut ctx, &[dummy_this(), Value::Long(a), Value::Int(1)])
            .unwrap();
        assert!(cache_lookup(a, 4).is_some());
        native_unsafe_put_int_at_address(&mut ctx, &[dummy_this(), Value::Long(b), Value::Int(2)])
            .unwrap();
        // Cache now points at b's window; a is no longer in range
        // (assuming the two arenas don't accidentally span each other).
        assert!(cache_lookup(b, 4).is_some());
        crate::unsafe_arena_free(a);
        crate::unsafe_arena_free(b);
    }
}
