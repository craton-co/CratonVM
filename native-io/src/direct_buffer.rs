// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.5 — `java.nio.DirectByteBuffer` allocation, accounting, pooling, and
//! Cleaner-style reclamation.
//!
//! Acceptance criterion (from `docs/wildfly-ejbca-roadmap.md` §6 WP3.5):
//! a tight loop of `ByteBuffer.allocateDirect(4096)` for 1M iterations must
//! keep native RSS bounded. Achieving that requires four pieces working
//! together:
//!
//!   1. **Real native allocation.**  We back each `DirectByteBuffer` with
//!      a real OS allocation.  Using `std::alloc::System` (which delegates
//!      to `HeapAlloc`/`malloc` on Windows/Unix respectively) gives us a
//!      page-aligned address that is genuinely off-heap from the VM's
//!      managed heap.  No `Vec<u8>` wrappers — that would defeat the whole
//!      point of "direct" and would bloat the managed heap instead of RSS.
//!
//!   2. **`Bits.reserveMemory` accounting.**  The JDK class
//!      `java.nio.Bits` tracks the total reserved direct memory and
//!      throws `OutOfMemoryError` when it would exceed
//!      `-XX:MaxDirectMemorySize`.  We replicate that behaviour with a
//!      `(reserved_bytes, count, max_bytes)` triple.  `max_bytes` defaults
//!      to `Runtime.maxMemory()` (matching real-JDK 25 default when the
//!      flag is unset) — represented here as a soft cap of 256 MiB which
//!      is enough headroom for the 4 GiB worth of 4 KiB allocations the
//!      acceptance test would otherwise demand if pooling didn't kick in.
//!
//!   3. **Pooling / free-list.**  4 KiB-bucketed power-of-two free list:
//!      when a buffer is freed we cache up to 256 entries per bucket.
//!      The loop in the acceptance test exercises a single bucket
//!      (4 KiB), so the pool's hit ratio is effectively 100% and the OS
//!      sees only a single transient allocation per worker thread.
//!
//!   4. **Phantom-queue reclamation.**  WP1.10 (Cleaner / weak refs) is
//!      partial as of session 93, so we cannot fully participate in the
//!      JDK's phantom-reference machinery.  Instead we register a
//!      `register0(DirectByteBuffer, long, int)` hook that captures the
//!      buffer object + (addr, capacity) and inspects it on a periodic
//!      worker thread.  When the Java object becomes unreachable from the
//!      VM (signalled by `cleanerExpired0`, called by our class-unload
//!      and finalizer pathway), we return its memory to the pool.  When
//!      the VM exits we drain the pool and free everything to keep
//!      Valgrind happy.  Pure best-effort, but combined with the pool the
//!      RSS bound is held even without full Cleaner integration.
//!
//! See `direct_buffer_register_natives` for the FQNs registered.

use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

// Cleaner IDs are internal monotonic AtomicI32 counters — no adversarial
// keying. FxHashMap avoids SipHash on the hot register/fire path. Matches
// native-api's T10.9.B migration.
use rustc_hash::FxHashMap;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Accounting — Bits.reserveMemory / Bits.unreserveMemory
// ---------------------------------------------------------------------------

/// Soft cap on total direct memory.  Matches the JDK 25 default for
/// `Runtime.maxMemory()` when `-XX:MaxDirectMemorySize` is not set; the
/// real JDK reads the actual flag, but we don't yet wire that through
/// (WP outside this scope).  256 MiB is enough that the WP3.5 acceptance
/// test (1M × 4 KiB with pooling enabled) stays well under it; an app
/// that genuinely needs more can still bypass via the pool's hit path.
const DEFAULT_MAX_DIRECT_BYTES: i64 = 256 * 1024 * 1024;

struct Bits {
    reserved: AtomicI64,
    count: AtomicI64,
    max: AtomicI64,
}

fn bits() -> &'static Bits {
    static B: OnceLock<Bits> = OnceLock::new();
    B.get_or_init(|| Bits {
        reserved: AtomicI64::new(0),
        count: AtomicI64::new(0),
        max: AtomicI64::new(DEFAULT_MAX_DIRECT_BYTES),
    })
}

fn oom(message: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
        message: message.into(),
    }))
}

fn try_reserve(size: i64) -> Result<(), MethodCallFailed> {
    let b = bits();
    let max = b.max.load(Ordering::Relaxed);
    // Optimistic add-then-check-then-rollback CAS loop; the JDK's
    // `Bits.reserveMemory` uses essentially the same pattern.
    loop {
        let cur = b.reserved.load(Ordering::Acquire);
        let next = cur.saturating_add(size);
        if next > max {
            return Err(oom(format!(
                "Direct buffer memory: tried {size}, used {cur}, max {max}"
            )));
        }
        if b.reserved
            .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            b.count.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
    }
}

fn release(size: i64) {
    let b = bits();
    b.reserved.fetch_sub(size, Ordering::AcqRel);
    b.count.fetch_sub(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Pool — power-of-two bucket free list
// ---------------------------------------------------------------------------
//
// Layout: 16 buckets covering 64 B → 2 MiB.  Each bucket holds up to
// `POOL_PER_BUCKET` (size, *mut u8) entries.  When the bucket is full we
// fall back to `dealloc` so the pool can't grow without bound.  *mut u8
// is treated as a raw integer here; we never deref it from the pool side.

const POOL_BUCKETS: usize = 16;
const POOL_BASE_SHIFT: u32 = 6; // 1 << 6 = 64 bytes
const POOL_PER_BUCKET: usize = 256;

/// Fixed alignment for every direct-buffer backing allocation.  8 bytes is
/// sufficient for any primitive `Unsafe.put*` write through the buffer.
const DBB_ALIGN: usize = 8;

/// Canonical *allocation size* for a logical byte count.
///
/// LAYOUT-SAFETY (Global-allocator contract): `GlobalAlloc::dealloc` is UB
/// unless the `Layout` passed exactly matches the one used to `alloc` — both
/// size *and* align. The pool reuses a block allocated for one request to
/// satisfy a *different* request whose size merely shares the same bucket
/// (`pool_take` accepts any `entry.size >= size`), so the raw logical size at
/// free time generally differs from the size the block was minted with.
/// Reconstructing a `Layout` from that free-time size would dealloc with the
/// wrong size → UB.
///
/// To make the `Layout` a pure function of the *block* (not of whichever
/// request happens to be holding it), every block is allocated at — and freed
/// with — the canonical size for its bucket: `next_power_of_two`, floored at
/// the pool's base bucket size (64 B). Any two logical sizes that map to the
/// same bucket therefore round to the identical canonical size, so the
/// `Layout` recomputed on *any* free path is byte-identical to the one used at
/// allocation. Sizes above the largest bucket (`bucket_for == None`) are not
/// pooled — they round to their own power of two and are alloc'd/dealloc'd at
/// exactly that size on both ends, which is likewise self-consistent.
///
/// Returns `None` only when rounding would overflow `usize` (size never
/// allocatable anyway).
#[inline]
fn canonical_alloc_size(size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let rounded = size.checked_next_power_of_two()?;
    Some(rounded.max(1usize << POOL_BASE_SHIFT))
}

/// The exact `Layout` used to allocate (and therefore the only `Layout` legal
/// to deallocate) a block sized for `size` logical bytes. Always built from
/// the canonical size so `alloc`/`dealloc` Layouts are identical regardless of
/// which request a pooled block is serving. Returns `None` for sizes that
/// cannot be laid out (zero, or overflowing).
#[inline]
fn dbb_layout(size: usize) -> Option<Layout> {
    let canon = canonical_alloc_size(size)?;
    Layout::from_size_align(canon, DBB_ALIGN).ok()
}

#[derive(Copy, Clone)]
struct PoolEntry {
    /// Canonical allocation size of this block (see `canonical_alloc_size`),
    /// i.e. the size the backing memory was actually `alloc`'d at — NOT the
    /// logical request that last used it. This is the size that reconstructs
    /// the original allocation `Layout`, so it is what must be used on the
    /// dealloc path when the block is evicted to the OS.
    size: usize,
    addr: usize, // *mut u8 stored as usize for Send safety in Mutex
}

struct Pool {
    buckets: [Mutex<Vec<PoolEntry>>; POOL_BUCKETS],
}

fn pool() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool {
        // const-generic array init: 16 fresh Mutex<Vec<>>s.
        buckets: std::array::from_fn(|_| Mutex::new(Vec::with_capacity(16))),
    })
}

#[inline]
fn bucket_for(size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let rounded = size.next_power_of_two();
    let shift = rounded.trailing_zeros();
    if shift < POOL_BASE_SHIFT {
        return Some(0);
    }
    let idx = (shift - POOL_BASE_SHIFT) as usize;
    if idx >= POOL_BUCKETS {
        None
    } else {
        Some(idx)
    }
}

fn pool_take(size: usize) -> Option<(usize, *mut u8)> {
    let idx = bucket_for(size)?;
    let mut bucket = pool().buckets[idx].lock().ok()?;
    while let Some(entry) = bucket.pop() {
        if entry.size >= size {
            return Some((entry.size, entry.addr as *mut u8));
        }
        // Mismatched bucket entry — drop it to the OS.  If layout
        // construction somehow fails we leak the address rather than
        // panic (defensive: we put real allocs here, but avoid a
        // production panic on unexpected input).
        //
        // Accounting note: pooled blocks are NOT counted in
        // `Bits.reserved` — `dbb_free` releases the reservation before
        // the block enters the pool, and `dbb_allocate` re-reserves it
        // on the way back out. So evicting a stale pooled block to the
        // OS here needs no accounting change.
        //
        // LAYOUT-SAFETY: `entry.size` is the block's *canonical* allocation
        // size (recorded by `pool_put`), so this `Layout` is byte-identical to
        // the one used to `alloc` it — never the logical request size.
        unsafe {
            if let Ok(layout) = Layout::from_size_align(entry.size, DBB_ALIGN) {
                dealloc(entry.addr as *mut u8, layout);
            }
        }
    }
    None
}

fn pool_put(size: usize, addr: *mut u8) -> bool {
    let Some(idx) = bucket_for(size) else {
        return false;
    };
    // Store the *canonical* allocation size, not the logical request size, so
    // that if this block is later evicted to the OS (`pool_take`) the
    // reconstructed `Layout` matches the one it was minted with. Every block
    // that reaches the pool was allocated by `dbb_allocate`, which always uses
    // the canonical size; `bucket_for(size) == Some(_)` here guarantees the
    // round succeeds, but fall back to the raw size defensively rather than
    // panic.
    let canon = canonical_alloc_size(size).unwrap_or(size);
    let Ok(mut bucket) = pool().buckets[idx].lock() else {
        return false;
    };
    if bucket.len() >= POOL_PER_BUCKET {
        return false;
    }
    bucket.push(PoolEntry {
        size: canon,
        addr: addr as usize,
    });
    true
}

// ---------------------------------------------------------------------------
// Allocation core
// ---------------------------------------------------------------------------

/// Allocate `size` bytes for a new DirectByteBuffer.  Tries the pool
/// first, then falls back to the system allocator.  All addresses
/// returned are 8-byte aligned (sufficient for any primitive type used
/// by `Unsafe.put*` writes through the buffer).  Returns the raw
/// address as a u64 the JDK can stash in `DirectByteBuffer.address`.
fn dbb_allocate(size: i64) -> Result<u64, MethodCallFailed> {
    if size < 0 {
        return Err(oom(format!("negative direct buffer size: {size}")));
    }
    if size == 0 {
        // The JDK uses NULL+0-len for zero-size direct buffers; we
        // mirror that to avoid alloc(0) UB with the system allocator.
        return Ok(0);
    }
    // Bug 1: balance `Bits.reserveMemory` / `unreserveMemory` accounting.
    //
    // Reservation tracks *live* direct memory: a block is reserved while
    // it is handed out to Java and unreserved the moment it is freed
    // (`dbb_free`), regardless of whether the bytes are physically
    // returned to the OS or parked in the pool. Pooled (free-listed)
    // blocks therefore carry NO reservation. Whichever way `dbb_allocate`
    // sources the bytes — fresh `alloc` or a pool hit — the block becomes
    // live and must be reserved exactly once here. We reserve up front so
    // the soft cap is honoured before we commit any memory; on any later
    // failure path we `release` to refund it.
    try_reserve(size)?;
    let usize_size = size as usize;
    let addr: *mut u8 = match pool_take(usize_size) {
        Some((_, p)) => p,
        None => {
            // Allocate at the block's *canonical* size (see `dbb_layout`) so
            // every later dealloc — pool eviction or direct free — reconstructs
            // the identical `Layout`, honouring the global-allocator contract.
            // 8-byte align suffices for j{byte,short,int,long,float,double}.
            let layout = match dbb_layout(usize_size) {
                Some(l) => l,
                None => {
                    release(size);
                    return Err(oom(format!("invalid direct buffer layout: {size}")));
                }
            };
            // SAFETY: layout is non-zero (size > 0) and aligned.  alloc
            // returns null on OOM; we surface that as Java OOM and
            // refund the accounting reservation.
            let p = unsafe { alloc(layout) };
            if p.is_null() {
                release(size);
                return Err(oom(format!("native alloc failed for size {size}")));
            }
            p
        }
    };
    // Zero the region — `Unsafe.allocateMemory(n)` itself doesn't
    // zero, but `DirectByteBuffer`'s `<init>(int)` calls
    // `Unsafe.setMemory(addr, n, 0)`.  Doing it here at the source
    // means callers don't have to issue a separate native.
    unsafe { std::ptr::write_bytes(addr, 0, usize_size) };
    // ABA fix: bump this address's live generation. Each free path then
    // recovers the generation captured in its size-record at allocation
    // time (`record_unsafe_alloc` / `register_cleaner` read the current
    // generation) and presents (addr, generation) to the free guard. The
    // freed-set is keyed by (addr, generation) and is never cleared on
    // realloc, so a stale free for a recycled address is rejected by
    // generation mismatch rather than passing a cleared guard. See
    // `freed_addrs` / `mark_freed_or_check` for the invariant.
    bump_generation(addr as u64);
    Ok(addr as u64)
}

fn dbb_free(addr: u64, size: i64) {
    if addr == 0 || size <= 0 {
        return;
    }
    let usize_size = size as usize;
    let p = addr as *mut u8;
    // Bug 1: a freed block is no longer live, so its reservation is
    // refunded here unconditionally — whether the bytes go back to the
    // OS or are parked in the pool. Pooled blocks carry no reservation;
    // `dbb_allocate` re-reserves on the pool hit that hands the block
    // back out. This keeps `try_reserve`/`release` calls strictly
    // paired (one reserve per live block, one release per free).
    if !pool_put(usize_size, p) {
        // Pool full or unbucketable — return to OS.
        //
        // LAYOUT-SAFETY: dealloc with the *canonical*-size `Layout`
        // (`dbb_layout`), identical to the one `dbb_allocate` used to mint this
        // block, regardless of the logical `size` this free was issued with.
        unsafe {
            if let Some(layout) = dbb_layout(usize_size) {
                dealloc(p, layout);
            }
        }
    }
    release(size);
}

// ---------------------------------------------------------------------------
// Phantom queue / Cleaner-coop registry
// ---------------------------------------------------------------------------
//
// WP1.10 is partial — we don't have a way to subscribe to "this
// ObjectRef just became unreachable".  As a stopgap, we maintain a
// registry keyed by the synthetic Cleaner id we hand back from
// `Cleaner.create0`.  When `cleanerExpired0(id)` fires (called either
// from a finalizer-style pathway or explicitly by the user calling
// `((DirectBuffer) buf).cleaner().clean()`), we free the underlying
// memory.  If neither happens, the entry remains until VM exit when
// `dbb_drain_pool` reclaims everything.

struct CleanerEntry {
    addr: u64,
    size: i64,
    /// Generation this entry was registered against (the live generation
    /// of `addr` at allocation time). Threaded to `dbb_free` so a Cleaner
    /// that fires *after* its address has been recycled is rejected by the
    /// (addr, generation) guard instead of freeing the live incarnation.
    generation: u64,
    /// Set to true after the Cleaner's runnable has fired.  Idempotent
    /// guard so an explicit clean() followed by a finalizer-driven
    /// expire doesn't double-free.
    cleaned: bool,
}

fn cleaners() -> &'static Mutex<FxHashMap<i32, CleanerEntry>> {
    static C: OnceLock<Mutex<FxHashMap<i32, CleanerEntry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn next_cleaner_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn register_cleaner(addr: u64, size: i64) -> i32 {
    let id = next_cleaner_id();
    // Capture the address's live generation now so the Cleaner runnable
    // frees against the incarnation it was registered for, even if the
    // address is recycled before the runnable fires (ABA guard).
    let generation = current_generation(addr);
    if let Ok(mut g) = cleaners().lock() {
        g.insert(
            id,
            CleanerEntry {
                addr,
                size,
                generation,
                cleaned: false,
            },
        );
    }
    id
}

fn fire_cleaner(id: i32) -> bool {
    let entry = match cleaners().lock() {
        Ok(mut g) => match g.get_mut(&id) {
            Some(e) if !e.cleaned => {
                e.cleaned = true;
                Some((e.addr, e.size, e.generation))
            }
            _ => None,
        },
        Err(_) => None,
    };
    if let Some((addr, size, generation)) = entry {
        // Generation-checked: if this address has since been recycled to a
        // new live buffer, the guard rejects the free and the live
        // incarnation is left untouched.
        free_checked(addr, size, generation);
        if let Ok(mut g) = cleaners().lock() {
            g.remove(&id);
        }
        true
    } else {
        false
    }
}

/// Bug 1: `Unsafe.freeMemory(addr)` carries no size, so the only way to
/// reclaim a DirectByteBuffer's backing store (and, crucially, refund its
/// `Bits` reservation) is to recover the size recorded at allocation time.
///
/// `Unsafe.allocateMemory` records sizes in `unsafe_allocs`, but the
/// `ByteBuffer.allocateDirect` path (`dbb_allocate_direct0`) does NOT —
/// it records the (addr, size) in the Cleaner registry instead. Without
/// this lookup, a `freeMemory(addr)` on a DirectByteBuffer address found
/// no recorded size, skipped `dbb_free` entirely, and so never called
/// `release` — leaking `Bits.reserved` until a spurious `OutOfMemoryError`.
///
/// This consumes the matching un-cleaned Cleaner entry (marking it cleaned
/// so a later finalizer-driven `fire_cleaner` is an idempotent no-op) and
/// returns the recorded `(size, generation)` so the caller can free it
/// exactly once against the incarnation it was allocated for.
fn take_cleaner_size_for_addr(addr: u64) -> Option<(i64, u64)> {
    let mut g = cleaners().lock().ok()?;
    let id = g
        .iter()
        .find(|(_, e)| e.addr == addr && !e.cleaned)
        .map(|(id, _)| *id)?;
    let (size, generation) = g.get(&id).map(|e| (e.size, e.generation))?;
    // Remove the entry: this addr is being reclaimed now, and a stale
    // entry would let a later `fire_cleaner` double-free it.
    g.remove(&id);
    Some((size, generation))
}

// ---------------------------------------------------------------------------
// Native handlers
// ---------------------------------------------------------------------------

fn arg_long(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// `java.nio.Bits.reserveMemory(long size, long cap)` — the JDK splits
/// "size" (the raw byte count to reserve) and "cap" (the capacity
/// reported back to the user, sometimes inflated to a page boundary).
/// Our accounting only cares about `size`.
fn bits_reserve_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = arg_long(args, 0);
    if size < 0 {
        return Err(oom(format!("negative reserveMemory: {size}")));
    }
    try_reserve(size)?;
    Ok(None)
}

fn bits_unreserve_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = arg_long(args, 0);
    if size > 0 {
        release(size);
    }
    Ok(None)
}

/// `java.nio.Bits.getMaxDirectMemory()` — defaults to our soft cap.
fn bits_get_max_direct_memory(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(bits().max.load(Ordering::Relaxed))))
}

/// `java.nio.DirectByteBuffer.allocateDirect0(int)` — synthetic
/// override used by our `ByteBuffer.allocateDirect(int)` redirect when
/// real-JDK's `<init>(int)` is unavailable.  Returns the buffer object
/// with `address`, `capacity`, `limit`, `position` populated and a
/// real backing allocation.  Layout is intentionally tolerant of the
/// real-JDK `Buffer` class: we set fields by name where possible and
/// fall back to slot indices for synthetic mode.
fn dbb_allocate_direct0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let addr = dbb_allocate(cap)?;
    let cleaner_id = if cap > 0 {
        register_cleaner(addr, cap)
    } else {
        0
    };
    let cid = ctx
        .ensure_class_initialized("java/nio/DirectByteBuffer")
        .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
    // Allocate with the real total field count when available so real-JDK
    // DirectByteBuffer bytecode (put/get reaching ByteBuffer.hb/offset,
    // DirectByteBuffer.att, …) does not read past the object. Falls back to 8
    // slots for pure-synthetic-jdk mode.
    let real_n = ctx.class_num_total_fields(cid);
    let buf = ctx.alloc_object(cid, real_n.max(8));
    ctx.set_field_by_name(buf, "address", Value::Long(addr as i64));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap as i32));
    ctx.set_field_by_name(buf, "limit", Value::Int(cap as i32));
    ctx.set_field_by_name(buf, "position", Value::Int(0));
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    // ByteBuffer defaults to BIG_ENDIAN. This allocator bypasses the Java
    // DirectByteBuffer constructor, so seed the order fields explicitly; XNIO
    // Remoting frames depend on putInt writing network-order lengths.
    ctx.set_field_by_name(buf, "bigEndian", Value::Int(1));
    ctx.set_field_by_name(
        buf,
        "nativeByteOrder",
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 }),
    );
    // NIO-DIRECTBUFFER FIX (2026-06-04): the fixed-slot writes below assume a
    // layout (position@0/limit@1/capacity@2/mark@3) that does NOT match the real
    // `java.nio.Buffer` layout (mark@0/position@1/limit@2/capacity@3/address@4/
    // segment@5). In real-JDK mode they CLOBBER the correct by-name writes above
    // — `set_field(buf,3,-1)` overwrote `capacity`→-1 and `set_field(buf,1,cap)`
    // overwrote `position`→cap — so `allocateDirect(n)` returned a buffer with
    // capacity=-1/position=n, throwing BufferOverflow on put() and (down the NIO
    // socket dispatcher) a native-write SIGSEGV. Only apply the synthetic-layout
    // fallback when the real named fields are NOT present (guard on a capacity
    // read-back). See `reference_server_socket_gap`.
    let named_ok = matches!(
        ctx.get_field_by_name(buf, "capacity"),
        Value::Int(c) if c == cap as i32
    );
    if !named_ok {
        ctx.set_field(buf, 0, Value::Int(0)); // position
        ctx.set_field(buf, 1, Value::Int(cap as i32)); // limit
        ctx.set_field(buf, 2, Value::Int(cap as i32)); // capacity
        ctx.set_field(buf, 3, Value::Int(-1)); // mark
        ctx.set_field(buf, 4, Value::Long(addr as i64)); // address
        ctx.set_field(buf, 5, Value::Long(cap)); // native_size
        ctx.set_field(buf, 6, Value::Int(cleaner_id)); // cleaner id
        ctx.set_field(buf, 7, Value::Int(0)); // padding / direct flag
    }

    // Round-5 Fix 6 (HIGH): wire a real PhantomReference / Cleaner so the
    // backing native memory is released when the DirectByteBuffer is
    // GC-collected. Without this, the only paths that free the memory are
    // an explicit `((DirectBuffer)buf).cleaner().clean()` Java-side call
    // or VM-shutdown drain — DBB allocated in a tight loop would otherwise
    // grow the bucketed pool forever.
    //
    // Wiring is parallel to the NEW-17 ByteBuffer.allocateDirect path in
    // `native-builtins/src/servlet.rs`, but uses a dedicated
    // `BucketDirectBufferDeallocator` class because servlet.rs's
    // `DirectBufferDeallocator.run()V` is keyed to the NativeMemoryTable
    // (alloc_id Long), whereas this `dbb_allocate_direct0` path uses our
    // bucketed pool (keyed by cleaner_id Int from `register_cleaner`).
    //
    // Field layout for Cleanable: (0=action, 1=cleaned_flag, 2=ref_id).
    // The interpreter's `run_cleaner_actions` invokes deallocator.run()V on
    // each pending cleanable, which dispatches to our `bucket_dealloc_run`
    // below to call `fire_cleaner(id)`.
    if cap > 0 && cleaner_id != 0 {
        let dealloc_cid = ctx
            .ensure_class_initialized("jdk/internal/ref/BucketDirectBufferDeallocator")
            .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
        let dealloc = ctx.alloc_object(dealloc_cid, 2);
        // field 0 = cleaner_id (Int) — matches our `bucket_dealloc_run` ABI
        // field 1 = addr (Long) — diagnostics
        ctx.set_field(dealloc, 0, Value::Int(cleaner_id));
        ctx.set_field(dealloc, 1, Value::Long(addr as i64));

        let cleanable_cid = ctx
            .ensure_class_initialized("java/lang/ref/Cleaner$Cleanable")
            .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
        let cleanable = ctx.alloc_object(cleanable_cid, 3);
        ctx.set_field(cleanable, 0, Value::Object(Some(dealloc))); // action
        ctx.set_field(cleanable, 1, Value::Int(0)); // cleaned
        ctx.set_field(cleanable, 2, Value::Int(-1)); // ref id

        // ref_type = 3 (Cleaner) — see vm_exec::discover_reference dispatch.
        // The ref processor enqueues `cleanable` into `cleaner_actions` when
        // `buf` (the referent) becomes unreachable; `cleaner_thread` then
        // drains, and `interpreter::run_cleaner_actions` invokes
        // `BucketDirectBufferDeallocator.run()V` to fire the cleaner.
        ctx.discover_reference(3, cleanable, buf, None);
    }

    Ok(Some(Value::Object(Some(buf))))
}

/// `jdk.internal.ref.BucketDirectBufferDeallocator.run()V` — Cleaner
/// dispatch target for the bucketed-pool `dbb_allocate_direct0` path.
/// Field 0 carries the synthetic cleaner_id from `register_cleaner`;
/// `fire_cleaner` is idempotent so concurrent GC drains and explicit
/// `((DirectBuffer) buf).cleaner().clean()` paths never double-free.
fn bucket_dealloc_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let id = match ctx.get_field(this, 0) {
        Value::Int(v) => v,
        _ => 0,
    };
    if id != 0 {
        fire_cleaner(id);
        // Zero the slot so the cleanable's idempotency + a stray re-invoke
        // (e.g. shutdown drain after Cleaner already fired) are both no-ops.
        ctx.set_field(this, 0, Value::Int(0));
    }
    Ok(None)
}

/// `jdk.internal.ref.Cleaner.create0(Object, long /*addr*/, long /*size*/) -> int`
/// — register a Cleaner runnable for the given (addr, size).  Returns
/// the synthetic id our `cleanerExpired0` consumes.
fn cleaner_create0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 1) as u64;
    let size = arg_long(args, 2);
    let id = register_cleaner(addr, size);
    Ok(Some(Value::Int(id)))
}

/// `jdk.internal.ref.Cleaner.cleanerExpired0(int id)` — the runnable
/// fires; we free the memory.  Idempotent.
fn cleaner_expired0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    fire_cleaner(id);
    Ok(None)
}

/// `sun.nio.ch.DirectBuffer.address()` — read the `address` field.
fn directbuffer_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Long(0)));
    };
    match ctx.get_field_by_name(this, "address") {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(ctx.get_field(this, 4))),
    }
}

/// `jdk.internal.misc.Unsafe.freeMemory(long addr)` — the canonical
/// JDK escape hatch for explicit DirectByteBuffer reclamation. The
/// JDK contract is "the address must have come from `allocateMemory`",
/// which on our side means it was minted by `dbb_allocate`. We don't
/// know the size at this entry point (Unsafe.freeMemory takes only
/// addr), so we cannot return it to the pool — fall through to a
/// system `dealloc` with a synthesised 1-byte layout, which is wrong
/// for `Bits` accounting but matches the JDK's "no-op if you mis-use"
/// behaviour. Callers that care about correct accounting should use
/// `dbb_free_explicit` (registered as `freeMemory(JJ)V`) instead.
///
/// Round-5 Fix 6 (HIGH): `dbb_allocate_direct0` now installs a Cleaner-
/// typed phantom reference via `ctx.discover_reference(3, ...)` and a
/// `BucketDirectBufferDeallocator` runnable, so GC reclaims unreachable
/// DirectByteBuffers automatically. This `Unsafe.freeMemory` entry point
/// still leaks if used without a matching `Unsafe.allocateMemory` (no
/// size info is available here), but the DirectByteBuffer path is now
/// fully GC-driven.
fn unsafe_free_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0) as u64;
    if addr == 0 {
        return Ok(None);
    }
    // Real-JDK Unsafe.freeMemory tracks size in `AllocationTable`; we
    // mimic that with `unsafe_allocs`. `dbb_free` deallocates with the
    // same `(size, align=8)` layout `dbb_allocate` used, so the dealloc
    // is layout-correct, and (crucially) it calls `release(size)` so the
    // `Bits.reserved` accounting is decremented.
    //
    // Bug 1: `Unsafe.freeMemory` takes only an address, so we recover
    // the original size from one of two registries:
    //   * `unsafe_allocs` — populated by `Unsafe.allocateMemory`.
    //   * the Cleaner registry — populated by `dbb_allocate_direct0`
    //     for the `ByteBuffer.allocateDirect` path, which never touches
    //     `unsafe_allocs`.
    // Previously only the first was consulted, so `freeMemory(addr)` on
    // a DirectByteBuffer address found no size, skipped `dbb_free`
    // entirely, and never called `release` — leaking `Bits.reserved`
    // monotonically until a spurious `OutOfMemoryError`. We now fall
    // back to the Cleaner registry so every freeable address has its
    // reservation refunded exactly once. Only if neither registry knows
    // the size do we leak (a wrong-layout `dealloc` would be UB) — but
    // such an address was never minted by our allocator, so there is no
    // reservation to refund either, and accounting stays balanced.
    //
    // ABA fix: each registry stamps the allocation-time generation into
    // its record, so the `(size, generation)` we recover identifies the
    // *exact incarnation* this free was issued against. Consuming the
    // record is itself a single-winner claim (atomic `remove`), and
    // `free_checked` re-validates the generation against the address's
    // current live generation: a stale free whose record was already
    // consumed finds nothing here, and a free that races in after the
    // address was recycled is rejected by generation mismatch inside
    // `free_checked` — neither can double-`pool_put` the live block.
    if let Some((size, generation)) =
        take_unsafe_alloc(addr).or_else(|| take_cleaner_size_for_addr(addr))
    {
        free_checked(addr, size, generation);
    }
    Ok(None)
}

/// `jdk.internal.misc.Unsafe.allocateMemory(long size) -> long` —
/// records the (addr, size) so `freeMemory(addr)` can reclaim it
/// accurately. JVM users who go via `ByteBuffer.allocateDirect` use
/// `dbb_allocate_direct0` above and don't touch this path.
fn unsafe_allocate_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = arg_long(args, 0);
    let addr = dbb_allocate(size)?;
    if addr != 0 {
        record_unsafe_alloc(addr, size);
    }
    Ok(Some(Value::Long(addr as i64)))
}

/// Per-allocation (addr → (size, generation)) table for
/// Unsafe.allocate/freeMemory. Kept separate from the Cleaner registry
/// because Unsafe-allocated memory has no associated Cleaner. The
/// generation is the address's live generation captured at allocation
/// time, so `freeMemory(addr)` (which carries no generation of its own)
/// can present the correct (addr, generation) to the ABA guard.
fn unsafe_allocs() -> &'static Mutex<FxHashMap<u64, (i64, u64)>> {
    static U: OnceLock<Mutex<FxHashMap<u64, (i64, u64)>>> = OnceLock::new();
    U.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn record_unsafe_alloc(addr: u64, size: i64) {
    // Stamp the address's current live generation (set by `bump_generation`
    // inside `dbb_allocate`) so a later `freeMemory(addr)` frees against
    // *this* incarnation. The freed-set is keyed by (addr, generation) and
    // is never cleared on realloc — there is deliberately nothing to clear
    // here; a recycled address simply carries a fresh generation that has
    // no freed entry yet.
    let generation = current_generation(addr);
    if let Ok(mut g) = unsafe_allocs().lock() {
        g.insert(addr, (size, generation));
    }
}

fn take_unsafe_alloc(addr: u64) -> Option<(i64, u64)> {
    unsafe_allocs().lock().ok()?.remove(&addr)
}

// ---------------------------------------------------------------------------
// ABA double-free guard — per-address generation / epoch
// ---------------------------------------------------------------------------
//
// Bug 2 (CRIT): `Unsafe.freeMemory(addr)` + `freeMemoryExplicit(addr, size)`
// on the same address previously double-pool_put'd the buffer (the first
// went through `take_unsafe_alloc` → `dbb_free`; the second went straight
// to `dbb_free` via the supplied size). A double pool_put corrupts the
// free-list — the same address ends up in two pool slots and is later
// handed to two distinct Java allocations simultaneously.
//
// History of this guard:
//   * round-9: a 4096-entry FIFO of freed addresses — evicted entries
//     under high churn reopened the double-free window.
//   * round-10: an unbounded `FxHashSet<u64>` keyed by address alone,
//     CLEARED on realloc (`clear_freed`) so it stayed bounded. That
//     clear is exactly the ABA hole this finding flags: a stale
//     `freeMemory(old_addr)` arriving AFTER the same address has been
//     recycled to a new live buffer passes the (cleared) check and
//     double-frees / pool_puts the live allocation. The per-address
//     generation counter was noted as the alternative but rejected "for
//     simplicity". The pool-corruption consequence is severe, so it is
//     adopted here.
//
// Scheme (close the ABA window):
//   * `generations`: addr -> current live generation (u64), bumped by
//     `bump_generation` every time `dbb_allocate` hands the address out.
//   * Every size-record (`unsafe_allocs`, `CleanerEntry`) captures the
//     live generation at allocation time, so every free path can present
//     the (addr, generation) it was *issued against* — even
//     `Unsafe.freeMemory`, which carries only an address, recovers the
//     generation from the record it consumes.
//   * `freed_addrs`: the set of (addr, generation) pairs that have already
//     been freed. It is NEVER cleared on realloc.
//
// Invariant enforced by `mark_freed_or_check(addr, gen)` — a free is
// honoured iff BOTH hold:
//   1. `gen == current_generation(addr)` — the free targets the live
//      incarnation. A stale free issued against an older generation (its
//      address since recycled) fails here and is rejected. THIS is what
//      closes the ABA window: the recycled address now carries a newer
//      generation, so the stale (addr, old_gen) free no longer matches.
//   2. `(addr, gen)` is not already in `freed_addrs` — single-free
//      idempotency within one incarnation (defeats freeMemory +
//      freeMemoryExplicit double-free on the same live buffer).
//
// Memory bounding (without the buggy realloc-clear): when an address is
// recycled, `bump_generation` prunes any freed-set entries for that
// address's now-dead older generations — they can never be queried again
// (only the current generation is ever validated). So the set stays
// bounded by the live-but-freed working set, exactly as before, while no
// longer reopening the window.

/// addr -> current live generation. Bumped each time the address is handed
/// out by `dbb_allocate`.
fn generations() -> &'static Mutex<FxHashMap<u64, u64>> {
    static G: OnceLock<Mutex<FxHashMap<u64, u64>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Bump `addr`'s live generation and return the new value. Called from
/// `dbb_allocate` the moment the address becomes live for a new buffer.
/// Also prunes freed-set entries for this address's older (now-dead)
/// generations so the freed-set stays bounded without the old, ABA-prone
/// realloc-clear.
fn bump_generation(addr: u64) -> u64 {
    let next = {
        let mut g = match generations().lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let slot = g.entry(addr).or_insert(0);
        *slot = slot.wrapping_add(1);
        *slot
    };
    // Drop stale freed-set entries for this address: only `next` (the new
    // current generation) can ever be validated from now on, so any older
    // (addr, *) pair is unreachable and safe to forget.
    if let Ok(mut f) = freed_addrs().lock() {
        f.retain(|&(a, gen)| a != addr || gen == next);
    }
    next
}

/// The current live generation for `addr`, or 0 if it has never been
/// handed out (matching the initial value used before the first bump).
fn current_generation(addr: u64) -> u64 {
    generations()
        .lock()
        .ok()
        .and_then(|g| g.get(&addr).copied())
        .unwrap_or(0)
}

/// Set of (addr, generation) pairs already freed. Never cleared on
/// realloc; pruned per-address by `bump_generation`.
fn freed_addrs() -> &'static Mutex<rustc_hash::FxHashSet<(u64, u64)>> {
    static F: OnceLock<Mutex<rustc_hash::FxHashSet<(u64, u64)>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(rustc_hash::FxHashSet::default()))
}

/// Returns `true` if this free should be SKIPPED — either because the
/// address has been recycled since this free was issued (generation
/// mismatch: a stale free for a dead incarnation), or because this exact
/// incarnation was already freed. Otherwise records (addr, generation)
/// and returns `false`. Both checks run under the freed-set lock; the
/// generation read is a snapshot taken first.
fn mark_freed_or_check(addr: u64, generation: u64) -> bool {
    // Stale-free / ABA check: reject a free whose generation no longer
    // matches the address's current live generation. A `freeMemory` /
    // Cleaner that fires after `addr` was recycled lands here and is
    // dropped, leaving the live incarnation untouched.
    if current_generation(addr) != generation {
        return true;
    }
    let mut g = match freed_addrs().lock() {
        Ok(g) => g,
        Err(_) => return false, // poisoned — best-effort: allow the free
    };
    // `HashSet::insert` returns `false` when the value was already
    // present — that's exactly the "already freed this incarnation" signal.
    !g.insert((addr, generation))
}

/// Single chokepoint for every free path. Validates the (addr, generation)
/// against the ABA guard and only then returns the block to the pool /
/// OS via `dbb_free`. A stale or duplicate free is logged and skipped so
/// the live allocation at `addr` is never double-`pool_put`.
fn free_checked(addr: u64, size: i64, generation: u64) {
    if addr == 0 || size <= 0 {
        return;
    }
    if mark_freed_or_check(addr, generation) {
        eprintln!(
            "[direct_buffer] free({:#x}, size={}, gen={}) skipped — stale (address recycled) or already-freed incarnation",
            addr, size, generation
        );
        return;
    }
    dbb_free(addr, size);
}

/// `dbb_free_explicit(addr, size)` — escape hatch for Java callers
/// that know both the pointer *and* the original capacity. Returns
/// the bytes to the pool and refunds `Bits` accounting. This is the
/// preferred path from the JDK side: synthetic `Cleaner` runnables
/// call it with the address+size captured at allocation time, so
/// pool reuse stays correct even without `PhantomReference` support.
fn dbb_free_explicit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0) as u64;
    let size = arg_long(args, 1);
    if addr == 0 || size <= 0 {
        return Ok(None);
    }
    // Drop any Unsafe.allocateMemory record so a later `freeMemory(addr)`
    // path doesn't also attempt a free (it would be caught by the
    // freed-set inside `free_checked` anyway, but this keeps the table
    // tidy and frees the recorded generation slot).
    let _ = take_unsafe_alloc(addr);
    // `freeMemoryExplicit` is a synchronous Java call on a buffer the
    // caller still holds, so it targets the address's *current* live
    // incarnation. We resolve that generation and route through the shared
    // ABA chokepoint: a racing `freeMemory(addr)` / second
    // `freeMemoryExplicit` on the same live buffer is rejected by the
    // (addr, generation) freed-set entry (single-free idempotency), and a
    // later free that arrives after the address is recycled is rejected by
    // generation mismatch — neither can double-`pool_put` the live block.
    // `size` is caller-supplied capacity captured at allocation time;
    // `dbb_free` deallocates with the same `(size, align=8)` layout.
    let generation = current_generation(addr);
    free_checked(addr, size, generation);
    Ok(None)
}

/// `((DirectBuffer) buf).cleaner().clean()` plumbed through to here:
/// the buffer carries the cleaner id at slot 6 in synthetic layout, or
/// in field `cleanerId` in real layout.
fn directbuffer_clean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let id = match ctx.get_field_by_name(this, "cleanerId") {
        Value::Int(v) => v,
        _ => match ctx.get_field(this, 6) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    if id != 0 {
        fire_cleaner(id);
    }
    Ok(None)
}

/// `sun.nio.ch.Util$BufferCache` is intended to be thread-confined through a
/// `ThreadLocal`, but the VM can currently expose one cache instance to several
/// Java worker threads. Its JDK bytecode mutates `count`, `start`, and the
/// `ByteBuffer[]` ring without a lock; concurrent `get`/`offer` calls then make
/// `count` claim an entry whose array slot is null. NIO subsequently calls
/// `capacity()` on that null entry.
///
/// Keep the cache itself on the Java heap (so its buffers remain visible to
/// GC), and serialize only the ring operations. The lock is deliberately
/// process-wide: cache operations are tiny and it also covers the accidental
const TEMPORARY_BUFFER_POOL_LIMIT: usize = 3;
const TEMPORARY_BUFFER_MAX_CAPACITY: i32 = 1 << 20;

#[derive(Clone, Copy)]
struct TemporaryBufferEntry {
    vm_identity: usize,
    capacity: i32,
    root: usize,
}

thread_local! {
    static TEMPORARY_BUFFERS: RefCell<Vec<TemporaryBufferEntry>> = const { RefCell::new(Vec::new()) };
}

/// sun.nio.ch.Util normally uses a Java ThreadLocal BufferCache. Under
/// allocation-heavy concurrent postings reads, that cache can be observed by
/// several VM worker threads and its unsynchronised ring produces null slots.
/// Keep a tiny pool keyed by the native worker thread instead. Entries are
/// persistent GC roots, so moving collection remaps them correctly; they are
/// released on eviction and the real direct-buffer Cleaner then owns normal
/// reclamation.
fn temporary_direct_buffer_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = args.get(0).and_then(Value::as_int).unwrap_or(0);
    if size < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "capacity < 0".into(),
        }
        .into());
    }
    let vm_identity = ctx.vm_identity();
    let mut reusable = None;
    TEMPORARY_BUFFERS.with(|entries| {
        let mut entries = entries.borrow_mut();
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.vm_identity == vm_identity && entry.capacity >= size)
        {
            let entry = entries.remove(index);
            reusable = ctx.resolve_global_root(entry.root).map(|buffer| (entry, buffer));
            if reusable.is_none() {
                ctx.remove_global_root(entry.root);
            }
        }
    });
    let buffer = match reusable {
        Some((_entry, buffer)) => buffer,
        None => match ctx.new_object_initialized(
            "java/nio/DirectByteBuffer",
            "(I)V",
            &[Value::Int(size)],
        )? {
            Some(Value::Object(Some(buffer))) => buffer,
            _ => return Ok(Some(Value::Object(None))),
        },
    };
    ctx.set_field_by_name(buffer, "mark", Value::Int(-1));
    ctx.set_field_by_name(buffer, "position", Value::Int(0));
    ctx.set_field_by_name(buffer, "limit", Value::Int(size));
    Ok(Some(Value::Object(Some(buffer))))
}

fn temporary_direct_buffer_release(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(buffer) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let capacity = ctx.get_field_by_name(buffer, "capacity").as_int().unwrap_or(-1);
    if !(0..=TEMPORARY_BUFFER_MAX_CAPACITY).contains(&capacity) {
        return Ok(None);
    }
    let vm_identity = ctx.vm_identity();
    let root = ctx.add_global_root(buffer);
    if root == 0 {
        return Ok(None);
    }
    TEMPORARY_BUFFERS.with(|entries| {
        let mut entries = entries.borrow_mut();
        while entries.len() >= TEMPORARY_BUFFER_POOL_LIMIT {
            let evicted = entries.remove(0);
            ctx.remove_global_root(evicted.root);
        }
        entries.push(TemporaryBufferEntry {
            vm_identity,
            capacity,
            root,
        });
    });
    Ok(None)
}

fn cleaner_for_address(addr: u64) -> Option<i32> {
    cleaners().lock().ok().and_then(|g| {
        g.iter()
            .find_map(|(id, entry)| (!entry.cleaned && entry.addr == addr).then_some(*id))
    })
}

fn release_temporary_direct_buffer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(buf) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let addr = match ctx.get_field_by_name(buf, "address") {
        Value::Long(addr) => addr as u64,
        _ => 0,
    };
    if addr != 0 {
        if let Some(id) = cleaner_for_address(addr) {
            fire_cleaner(id);
        }
    }
    Ok(None)
}

/// Test-only hook: number of currently-tracked Cleaner registrations.
#[cfg(test)]
fn cleaners_pending_count() -> usize {
    cleaners().lock().map(|g| g.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register the WP3.5 DirectByteBuffer + Cleaner natives.  Idempotent:
/// safe to call multiple times.  See module docs for FQN list and
/// caveats around partial WP1.10 Cleaner integration.
pub fn register_direct_buffer_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // java.nio.Bits accounting natives.
    r.register(
        "java/nio/Bits",
        "reserveMemory",
        "(JJ)V",
        bits_reserve_memory,
    );
    r.register(
        "java/nio/Bits",
        "reserveMemory",
        "(JI)V",
        bits_reserve_memory,
    );
    r.register(
        "java/nio/Bits",
        "unreserveMemory",
        "(JJ)V",
        bits_unreserve_memory,
    );
    r.register(
        "java/nio/Bits",
        "unreserveMemory",
        "(JI)V",
        bits_unreserve_memory,
    );
    r.register(
        "java/nio/Bits",
        "getMaxDirectMemory",
        "()J",
        bits_get_max_direct_memory,
    );

    // VM.maxDirectMemory() also exists in real-JDK as
    // jdk.internal.misc.VM.maxDirectMemory().  We register the same
    // backend under both class names so either dispatch path works.
    r.register(
        "jdk/internal/misc/VM",
        "maxDirectMemory",
        "()J",
        bits_get_max_direct_memory,
    );

    // DirectByteBuffer allocation hook (synthetic-mode override).
    r.register(
        "java/nio/DirectByteBuffer",
        "allocateDirect0",
        "(I)Ljava/nio/ByteBuffer;",
        dbb_allocate_direct0,
    );
    // Keep ByteBuffer.allocateDirect on its real-JDK bytecode path so the
    // DirectByteBuffer constructor installs its non-null Cleaner.

    // Cleaner natives.
    r.register(
        "jdk/internal/ref/Cleaner",
        "create0",
        "(Ljava/lang/Object;JJ)I",
        cleaner_create0,
    );
    // Round-5 Fix 6: GC-driven Cleaner runnable for `dbb_allocate_direct0`'s
    // bucketed pool path. Pairs with the discover_reference call inside that
    // function so the ref processor drains and fires this on phantom-clear.
    r.register(
        "jdk/internal/ref/BucketDirectBufferDeallocator",
        "run",
        "()V",
        bucket_dealloc_run,
    );
    r.register(
        "jdk/internal/ref/Cleaner",
        "cleanerExpired0",
        "(I)V",
        cleaner_expired0,
    );
    // The 8u/16+ name is `clean`; register alias.
    r.register(
        "sun/misc/Cleaner",
        "create0",
        "(Ljava/lang/Object;JJ)I",
        cleaner_create0,
    );

    // `Util$BufferCache` is normally ThreadLocal. VM worker threads can
    // currently share it, so make each ring operation atomic while retaining
    // buffers in the Java-owned cache (rather than allocating a Cleaner on
    // Avoid the racy Java BufferCache while preserving real direct buffers
    // and their Cleaner contract. The pool is local to each native worker
    // thread and bounded to three buffers.
    r.register("sun/nio/ch/Util", "getTemporaryDirectBuffer", "(I)Ljava/nio/ByteBuffer;", temporary_direct_buffer_get);
    for method in ["releaseTemporaryDirectBuffer", "offerFirstTemporaryDirectBuffer", "offerLastTemporaryDirectBuffer"] {
        r.register("sun/nio/ch/Util", method, "(Ljava/nio/ByteBuffer;)V", temporary_direct_buffer_release);
    }

    // DirectBuffer interface methods.
    r.register(
        "sun/nio/ch/DirectBuffer",
        "address",
        "()J",
        directbuffer_address,
    );
    r.register(
        "java/nio/DirectByteBuffer",
        "address",
        "()J",
        directbuffer_address,
    );
    r.register(
        "java/nio/DirectByteBuffer",
        "cleanNative0",
        "()V",
        directbuffer_clean,
    );

    // Unsafe.allocateMemory / freeMemory — the path real-JDK uses
    // when a non-buffer caller goes through Unsafe directly (e.g.
    // `sun.misc.Unsafe.allocateMemory(n)` returns a raw address).
    // We back it with the same pool/accounting machinery as the
    // DirectByteBuffer path so a 4 KiB tight-loop allocate/free
    // stays RSS-bounded regardless of which API the JDK picks.
    for cls in ["jdk/internal/misc/Unsafe", "sun/misc/Unsafe"] {
        r.register(cls, "allocateMemory", "(J)J", unsafe_allocate_memory);
        r.register(cls, "allocateMemory0", "(J)J", unsafe_allocate_memory);
        r.register(cls, "freeMemory", "(J)V", unsafe_free_memory);
        r.register(cls, "freeMemory0", "(J)V", unsafe_free_memory);
    }

    // Synthetic helper used by JDK-side Cleaner runnables that
    // capture (addr, size) at allocation time — see module docs.
    r.register(
        "java/nio/DirectByteBuffer",
        "freeMemoryExplicit",
        "(JJ)V",
        dbb_free_explicit,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockNativeContext;

    /// Serialize tests that mutate the global `Bits` accounting state.
    /// Without this, parallel `cargo test` runs see racey reserved-byte
    /// counters across tests.
    fn bits_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn wp35_bits_reserve_unreserve_balances() {
        let _g = bits_test_lock();
        let baseline = bits().reserved.load(Ordering::Relaxed);
        let mut ctx = MockNativeContext::new();
        bits_reserve_memory(&mut ctx, &[Value::Long(4096), Value::Long(4096)]).unwrap();
        bits_reserve_memory(&mut ctx, &[Value::Long(8192), Value::Long(8192)]).unwrap();
        assert_eq!(
            bits().reserved.load(Ordering::Relaxed),
            baseline + 4096 + 8192
        );
        bits_unreserve_memory(&mut ctx, &[Value::Long(4096), Value::Long(4096)]).unwrap();
        bits_unreserve_memory(&mut ctx, &[Value::Long(8192), Value::Long(8192)]).unwrap();
        assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
    }

    #[test]
    fn wp35_reserve_rejects_when_over_max() {
        let _g = bits_test_lock();
        let saved = bits().max.swap(1024, Ordering::Relaxed);
        let baseline = bits().reserved.load(Ordering::Relaxed);
        let mut ctx = MockNativeContext::new();
        // Drain headroom first.
        let used = (1024 - baseline).max(0);
        if used > 0 {
            bits_reserve_memory(&mut ctx, &[Value::Long(used), Value::Long(used)]).unwrap();
        }
        // Now any further reserve must fail with OOM.
        let err = bits_reserve_memory(&mut ctx, &[Value::Long(1), Value::Long(1)]);
        assert!(err.is_err(), "expected OOM, got {:?}", err);
        if used > 0 {
            bits_unreserve_memory(&mut ctx, &[Value::Long(used), Value::Long(used)]).unwrap();
        }
        bits().max.store(saved, Ordering::Relaxed);
    }

    #[test]
    fn wp35_allocate_then_free_returns_to_pool_for_4k() {
        let _g = bits_test_lock();
        let baseline = bits().reserved.load(Ordering::Relaxed);
        let addr = dbb_allocate(4096).expect("alloc 4k");
        assert_ne!(addr, 0);
        assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline + 4096);
        dbb_free(addr, 4096);
        assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
        // Second 4k alloc should hit the pool — RSS-bounded behaviour.
        // We can't strictly require `addr2 == addr` because earlier tests
        // (or tests interleaved on parallel threads) may have populated
        // the same bucket; the invariant is that allocation succeeds and
        // accounting balances after free.
        let addr2 = dbb_allocate(4096).expect("alloc 4k from pool");
        assert_ne!(addr2, 0);
        assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline + 4096);
        dbb_free(addr2, 4096);
        assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
    }

    #[test]
    fn wp35_allocate_direct0_defaults_to_big_endian_order() {
        let _g = bits_test_lock();
        let baseline = bits().reserved.load(Ordering::Relaxed);
        let mut ctx = MockNativeContext::new();

        let result = dbb_allocate_direct0(&mut ctx, &[Value::Int(0)])
            .expect("allocateDirect0 should not throw");
        let buf = match result {
            Some(Value::Object(Some(o))) => o,
            other => panic!("allocateDirect0 returned {other:?}"),
        };

        assert_eq!(ctx.get_field_by_name(buf, "bigEndian"), Value::Int(1));
        assert_eq!(
            ctx.get_field_by_name(buf, "nativeByteOrder"),
            Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 })
        );
        assert_eq!(
            bits().reserved.load(Ordering::Relaxed),
            baseline,
            "zero-capacity allocation should not touch Bits accounting"
        );
    }

    #[test]
    fn wp35_zero_size_alloc_yields_null_address() {
        let addr = dbb_allocate(0).unwrap();
        assert_eq!(addr, 0);
        // No accounting impact for zero-size.
        dbb_free(0, 0);
    }

    #[test]
    fn wp35_cleaner_expired_is_idempotent() {
        let pending_before = cleaners_pending_count();
        let id = register_cleaner(0xdead_beef, 0); // size=0 → free is no-op
        assert!(cleaners_pending_count() >= pending_before + 1);
        assert!(fire_cleaner(id));
        assert!(!fire_cleaner(id));
        assert_eq!(cleaners_pending_count(), pending_before);
    }

    #[test]
    fn wp35_get_max_direct_memory_returns_soft_cap() {
        let mut ctx = MockNativeContext::new();
        let r = bits_get_max_direct_memory(&mut ctx, &[]).unwrap().unwrap();
        match r {
            Value::Long(v) => assert!(v > 0),
            other => panic!("unexpected return {:?}", other),
        }
    }

    #[test]
    fn wp35_pool_bucket_classification() {
        assert_eq!(bucket_for(0), None);
        assert_eq!(bucket_for(1), Some(0)); // round to 64 (2^6) → idx 0
        assert_eq!(bucket_for(64), Some(0)); // exact bucket-0 fit
        assert_eq!(bucket_for(4096), Some(6)); // 2^12, idx = 12-6 = 6
                                               // Beyond 2 MiB falls through to direct system free.
        assert_eq!(bucket_for(8 * 1024 * 1024), None);
    }

    #[test]
    fn wp35_canonical_alloc_size_floors_at_bucket_base() {
        // Sub-64-B requests round up to the 64-B base bucket size.
        assert_eq!(canonical_alloc_size(1), Some(64));
        assert_eq!(canonical_alloc_size(60), Some(64));
        assert_eq!(canonical_alloc_size(64), Some(64));
        // Above base: plain next-power-of-two.
        assert_eq!(canonical_alloc_size(65), Some(128));
        assert_eq!(canonical_alloc_size(100), Some(128));
        assert_eq!(canonical_alloc_size(4096), Some(4096));
        assert_eq!(canonical_alloc_size(4097), Some(8192));
        assert_eq!(canonical_alloc_size(0), None);
        // Overflowing round must not panic — it reports unallocatable.
        assert_eq!(canonical_alloc_size(usize::MAX), None);
    }

    #[test]
    fn wp35_layout_is_pure_function_of_bucket() {
        // The core invariant this fix protects: any two logical sizes that
        // share a bucket must produce the IDENTICAL allocation `Layout`, so a
        // pooled block minted for one request and freed under a different
        // (same-bucket) request always deallocs with the size+align it was
        // allocated with — never a mismatched `Layout` (global-allocator UB).
        let a = dbb_layout(60).expect("layout 60");
        let b = dbb_layout(64).expect("layout 64");
        let c = dbb_layout(1).expect("layout 1");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.size(), 64);
        assert_eq!(a.align(), DBB_ALIGN);

        // A request that the pool serves from a larger same-bucket block:
        // size 100 and size 128 both canonicalise to 128.
        assert_eq!(dbb_layout(100), dbb_layout(128));
        // Different buckets => different Layouts (sanity).
        assert_ne!(dbb_layout(64), dbb_layout(128));
    }

    #[test]
    fn wp35_pool_records_canonical_size_for_layout_safe_eviction() {
        let _g = bits_test_lock();
        // Allocate a block whose logical size (100) is smaller than its
        // canonical allocation size (128), free it (parks in the pool), then
        // confirm the parked entry carries the canonical size — the value used
        // to rebuild the original `Layout` on eviction. Without this, eviction
        // would dealloc 100 bytes against a 128-byte allocation (UB).
        let addr = dbb_allocate(100).expect("alloc 100");
        assert_ne!(addr, 0);
        dbb_free(addr, 100);
        let idx = bucket_for(100).expect("bucket");
        let bucket = pool().buckets[idx].lock().expect("lock");
        let parked = bucket
            .iter()
            .find(|e| e.addr == addr as usize)
            .expect("parked entry present");
        assert_eq!(
            parked.size,
            canonical_alloc_size(100).unwrap(),
            "pooled entry must record canonical alloc size, not logical request"
        );
    }
}
