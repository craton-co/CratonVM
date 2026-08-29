// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.5 — `java.nio.DirectByteBuffer` allocation, accounting, pooling, and
//! Cleaner-style reclamation.
//!
//! Acceptance criterion (from `gaps/wildfly-ejbca-roadmap.md` §6 WP3.5):
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

/// Configure the process-wide direct-memory soft cap at VM boot, mirroring
/// real JDK's `-XX:MaxDirectMemorySize` resolution: an explicit flag value if
/// given, otherwise `-Xmx` (`Runtime.maxMemory()`). Called once from
/// `vm_init::SharedVm::new` with the config's resolved value; falls back to
/// `DEFAULT_MAX_DIRECT_BYTES` for any caller (e.g. unit tests) that never
/// boots a full VM and so never calls this.
///
/// See fixed-suite-bugs/h2-suite-bugs/bug-h2-largeblob-direct-memory-oom.md:
/// before this, the cap was hardcoded to 256 MiB regardless of `-Xmx`, so a
/// `-Xmx 1g` H2 MVStore workload with genuine ~250 MiB peak direct-buffer
/// usage (chunk writer thread) hit a ceiling HotSpot doesn't impose at the
/// same heap size.
pub fn configure_max_direct_memory(bytes: i64) {
    let clamped = bytes.max(0);
    bits().max.store(clamped, Ordering::Relaxed);
}

/// Live direct buffers, and the bytes they hold — what
/// `BufferPoolMXBean("direct")` reports as `getCount()` and `getMemoryUsed()`.
///
/// These are the SAME two counters `try_reserve`/`release` maintain for the
/// soft cap, deliberately: a pool bean that counted separately would be a
/// second answer to a question the allocator already answers, and the
/// allocator's is the one that is true. `reserved` tracks LIVE memory (a block
/// is unreserved the moment it is freed and re-reserved when it comes back out
/// of the bucket pool), so both numbers fall when a buffer is released, exactly
/// as HotSpot's do.
///
/// `getTotalCapacity()` and `getMemoryUsed()` are the same number here. On
/// HotSpot they differ only by the page-alignment slop `Bits.reserveMemory`
/// adds for a buffer whose capacity is not page-aligned; CratonVM reserves the
/// logical size, so there is no slop to report and inventing one would be a
/// fabricated number rather than a measured one.
pub fn direct_buffer_pool_stats() -> (i64, i64) {
    let b = bits();
    (
        b.count.load(Ordering::Relaxed).max(0),
        b.reserved.load(Ordering::Acquire).max(0),
    )
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
/// [`dbb_allocate`] with the same reclaim-and-retry [`bits_reserve_memory`]
/// performs, for the callers that have a `NativeContext` to collect with.
///
/// `bits_reserve_memory`'s retry does not cover the path that matters most in
/// REAL-JDK mode, because that native never runs there: `java/nio/Bits` is not
/// in `force_native_over_real_jdk_bytecode`, and `Bits.reserveMemory` is
/// ordinary Java, so the JDK's own bytecode wins and our accounting is only
/// consulted from the `Unsafe.allocateMemory0` that the real
/// `DirectByteBuffer(int)` constructor calls. That call had NO retry at all —
/// the first refusal threw.
///
/// The two budgets are separate counters and drift: ours also counts every
/// other `Unsafe.allocateMemory` caller, so it can saturate while the JDK's
/// `Bits` still believes there is room, and the JDK's own `System.gc()`-and-
/// retry never gets a chance to run. That is the shape reported in
/// `known-issues/direct-memory-still-exhausts-under-sustained-churn-20260805.md`,
/// whose `OutOfMemoryError: Direct buffer memory: tried …, used …, max …`
/// message is this module's, raised from `try_reserve` below, on an H2
/// background writer thread.
///
/// Three rounds, bounded exactly as the sibling above and as the JDK bounds
/// its own retry, and with the same "no progress means nothing is
/// reclaimable" early exit so a genuinely exhausted cap still surfaces
/// promptly.
fn dbb_allocate_collecting(
    ctx: &mut dyn NativeContext,
    size: i64,
) -> Result<u64, MethodCallFailed> {
    let first_failure = match dbb_allocate(size) {
        Ok(addr) => return Ok(addr),
        Err(e) => e,
    };
    // Only a reservation refusal is worth collecting for. A negative size, an
    // unrepresentable layout, or the system allocator itself returning null
    // are not things a collection can change.
    if !is_reservation_failure(size) {
        return Err(first_failure);
    }
    const RECLAIM_ROUNDS: u32 = 3;
    for _ in 0..RECLAIM_ROUNDS {
        let before = bits().reserved.load(Ordering::Acquire);
        ctx.force_gc();
        match dbb_allocate(size) {
            Ok(addr) => return Ok(addr),
            Err(e) if !is_reservation_failure(size) => return Err(e),
            Err(_) => {}
        }
        if bits().reserved.load(Ordering::Acquire) >= before {
            break;
        }
    }
    Err(first_failure)
}

/// Would a reservation of `size` still exceed the direct-memory cap?
#[inline]
fn is_reservation_failure(size: i64) -> bool {
    let b = bits();
    size > 0
        && b.reserved.load(Ordering::Acquire).saturating_add(size) > b.max.load(Ordering::Relaxed)
}

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
    // SAFETY: `addr` is a fresh allocation of exactly `usize_size` bytes (or
    // the zero-size sentinel) and is writable for that entire range.
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

/// Read the single `long` argument of an INSTANCE `Unsafe` method.
///
/// `sun.misc.Unsafe.allocateMemory(long)` / `freeMemory(long)` (and the
/// `jdk.internal.misc.Unsafe` `*0` twins) are instance methods, so the
/// interpreter passes the receiver as `args[0]` and the `long` as `args[1]`.
/// Reading `args[0]` — as this file did — decoded the receiver reference as
/// `0`, so `allocateMemory(n)` allocated nothing and returned address 0 for
/// EVERY caller, and `freeMemory(addr)` took its `addr == 0` early return and
/// silently leaked every block. These registrations win the registry slot over
/// `native-builtins`' arena-backed twins (`register_io_natives` runs after
/// `register_builtins`), so the broken pair was the live implementation.
///
/// A leading object reference is skipped rather than assumed, so a direct
/// caller that passes the bare argument list still resolves correctly.
fn unsafe_long_arg(args: &[Value]) -> i64 {
    match args.first() {
        Some(Value::Object(_)) => arg_long(args, 1),
        _ => arg_long(args, 0),
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
/// `java.nio.Bits.reserveMemory(long size, long cap)`.
///
/// The JDK's contract is NOT "reserve or throw" — it is "reserve, and if the
/// cap is reached, make the collector reclaim unreachable direct buffers and try
/// again; throw only when that fails too". `Bits.reserveMemory` spells this out:
/// wait for reference processing, then `System.gc()`, then retry with an
/// exponential back-off before it constructs an `OutOfMemoryError`.
///
/// That retry is load-bearing, because direct memory is invisible to the heap's
/// own occupancy trigger: a program can churn gigabytes of `allocateDirect`
/// while the Java heap stays nearly empty, so nothing else on the VM's side has
/// any reason to collect. This used to throw on the first refusal, which turned
/// "the buffers you dropped have not been reclaimed *yet*" into a hard
/// `OutOfMemoryError: Direct buffer memory`.
fn bits_reserve_memory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = arg_long(args, 0);
    if size < 0 {
        return Err(oom(format!("negative reserveMemory: {size}")));
    }
    let Err(first_failure) = try_reserve(size) else {
        return Ok(None);
    };
    if dm_dbg_enabled() {
        eprintln!(
            "[dm] reserveMemory REFUSED size={} reserved={} max={} thread={:?}",
            size,
            bits().reserved.load(Ordering::Acquire),
            bits().max.load(Ordering::Relaxed),
            std::thread::current().id()
        );
    }
    // Reclaim-and-retry. Each round forces a collection — which runs reference
    // processing and, through it, the `jdk.internal.ref.Cleaner` every
    // `DirectByteBuffer` registers — and then re-attempts the reservation.
    // Bounded so a genuinely exhausted cap still surfaces the OOM promptly
    // rather than spinning; the JDK bounds its own retry the same way.
    const RECLAIM_ROUNDS: u32 = 3;
    for round in 0..RECLAIM_ROUNDS {
        let before = bits().reserved.load(Ordering::Acquire);
        ctx.force_gc();
        let after = bits().reserved.load(Ordering::Acquire);
        if dm_dbg_enabled() {
            eprintln!(
                "[dm] reserveMemory round={} before={} after={} freed={}",
                round,
                before,
                after,
                before as i64 - after as i64
            );
        }
        if try_reserve(size).is_ok() {
            if dm_dbg_enabled() {
                eprintln!("[dm] reserveMemory GRANTED after round={}", round);
            }
            return Ok(None);
        }
        // No progress at all from a full collection means nothing is
        // reclaimable; further rounds would only add latency to the OOM.
        if after >= before {
            if dm_dbg_enabled() {
                eprintln!(
                    "[dm] reserveMemory no progress, breaking at round={}",
                    round
                );
            }
            break;
        }
    }
    if dm_dbg_enabled() {
        eprintln!(
            "[dm] reserveMemory GIVING UP size={} reserved={}",
            size,
            bits().reserved.load(Ordering::Acquire)
        );
    }
    Err(first_failure)
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
    let addr = dbb_allocate_collecting(ctx, cap)?;
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
    //
    // CONVERGED (F37-1 §4, landing F26-1's N1 and closing the fourth of W7-76
    // §8.2's five sites). Byte-for-byte the pair this replaced — same two field
    // names, same two values, same `cfg!(target_endian)` test — so this is a
    // no-op today BY CONSTRUCTION, which is the point: the copy it removes is
    // one that could drift, and its sibling in `native-builtins/src/charset.rs`
    // already HAD (that one wrote `bigEndian` and not `nativeByteOrder`). Same
    // crate as the helper, so a plain `crate::` path, not a cross-crate one.
    crate::seed_buffer_byte_order(ctx, buf);
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
    let addr = unsafe_long_arg(args) as u64;
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
/// accurately.
///
/// This IS the `ByteBuffer.allocateDirect` path in real-JDK mode, contrary to
/// what this comment used to say: the real `DirectByteBuffer(int)` constructor
/// calls `Unsafe.allocateMemory`, and `bits_reserve_memory` — which would
/// otherwise have reserved first — never runs there, because the JDK's own
/// `java.nio.Bits` bytecode wins. `dbb_allocate_direct0` is the SYNTHETIC-mode
/// path. Hence the reclaim-and-retry here; see `dbb_allocate_collecting`.
fn unsafe_allocate_memory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = unsafe_long_arg(args);
    let addr = dbb_allocate_collecting(ctx, size)?;
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
    // Root handle of a pooled entry that is being handed back out. The pool
    // stops tracking it the moment it leaves `entries`, so its handle has to be
    // released — but only after the writes below, so the buffer stays rooted
    // for the whole hand-off. Leaving it registered leaked one JNI global
    // reference per pool hit, i.e. per NIO transfer: `TEMPORARY_BUFFER_POOL_LIMIT`
    // bounds the pool at three entries per thread, but nothing bounds the root
    // table behind it. Measured on three H2 `TestFileSystem` filesystems (~4.5 s,
    // 23 collections): section 9 of `roots.rs` contributed **57 036** roots at
    // the last GC without this release and **7** with it, and every GC root scan
    // walked all of them.
    let mut handed_out_root = 0usize;
    let mut reusable = None;
    TEMPORARY_BUFFERS.with(|entries| {
        let mut entries = entries.borrow_mut();
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.vm_identity == vm_identity && entry.capacity >= size)
        {
            let entry = entries.remove(index);
            match ctx.resolve_global_root(entry.root) {
                Some(buffer) => {
                    handed_out_root = entry.root;
                    reusable = Some(buffer);
                }
                // Already collected out from under the pool: the entry is dead,
                // so drop its handle here and fall through to a fresh buffer.
                None => {
                    ctx.remove_global_root(entry.root);
                }
            }
        }
    });
    let buffer = match reusable {
        Some(buffer) => buffer,
        // Only reached with `handed_out_root == 0`, so the early return below
        // cannot strand a handle.
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
    // The buffer is now on its way back to the caller, which publishes it
    // through the native-return handoff (`native_pending_return`), so the pool's
    // root has nothing left to protect.
    if handed_out_root != 0 {
        ctx.remove_global_root(handed_out_root);
    }
    Ok(Some(Value::Object(Some(buffer))))
}

fn temporary_direct_buffer_release(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(buffer) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let capacity = ctx
        .get_field_by_name(buffer, "capacity")
        .as_int()
        .unwrap_or(-1);
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

// ---------------------------------------------------------------------------
// Per-element absolute accessors — `DirectByteBuffer.get(int)` / `put(int, byte)`
// ---------------------------------------------------------------------------
//
// PERF (H2 `TestFileSystem.testConcurrent` on `nioMemLZF:`, residual 4 of
// retired `h2-jitban-longtail1` write-up). These two methods
// are real-JDK bytecode, and each one expands to a try/finally around five
// nested invocations:
//
//     get(i) -> session(), checkIndex(i), ix(i), SCOPED_MEMORY_ACCESS.getByte(),
//               Reference.reachabilityFence(this)
//
// `org.h2.compress.CompressLZF` has `(ByteBuffer, ...)` overloads of
// `compress`/`expand` that move exactly one byte per call, so a 64 KB page is
// ~65,000 of those chains per pass. Measured end-to-end cost was ~2 us per
// element — the `byte[]` overload of the identical algorithm is a `baload`
// with no call at all, which is why `memLZF:` ran 12x against HotSpot while
// `nioMemLZF:` ran 1,130x. The cost was never LZF, `ByteBuffer` as such, or
// the spin lock the test wraps around it: it was per-element dispatch.
//
// These natives collapse the whole chain into one call that reads `address`
// and `limit` out of the receiver and performs a single raw access.
//
// DELIBERATELY CONSERVATIVE. Every case not fully modelled here bails to the
// real bytecode via `invoke_virtual_bytecode_only` rather than guessing:
//
//   * layout not resolvable (synthetic-jdk mode, or a JDK whose `Buffer`
//     fields are named differently) — one-time, memoised;
//   * index outside `[0, limit)` — the JDK throws a *plain*
//     `IndexOutOfBoundsException` with a message this layer has no
//     `RuntimeError` variant for, so the bytecode throws it;
//   * `isReadOnly` receiver on the `put` side — `DirectByteBufferR` overrides
//     `put(int, byte)`, so normal dispatch never reaches this native with one,
//     but a defensive check keeps the guarantee independent of that;
//   * an address the memory layer declines (freed arena handle, non-readable
//     pointer) — the bytecode path raises whatever the JDK raises.
//
// The bail is a real virtual dispatch to the class-file body, so it can never
// re-enter this native. It is also why these four cannot be registered as LEAF
// natives (`NativeMethodRegistry::set_leaf`): a leaf must not re-enter Java, and
// the bail does exactly that.
//
// WHERE THIS STANDS (2026-08-05). Collapsing the chain was necessary but not
// sufficient: with the natives in place the cost was still ~650 ns per element
// against HotSpot's 0.4 ns, essentially all of it dispatch. The general
// per-call-site native cache and the native funnel's own refcount fix took that
// to ~270-320 ns, i.e. `nioMemLZF:` from ~101 to ~50 ms/op against HotSpot's
// 0.55 (`probes/DbbElemProbe.java`, `probes/LzfProbe.java`).
//
// Two facts bound what is left, and both were measured rather than assumed:
//
//   * **These four cannot claim LEAF** (`NativeMethodRegistry::set_leaf`), which
//     is why the general leaf bypass does not reach them: the bail above
//     re-enters Java and the leaf contract forbids that. Claiming it anyway was
//     measured at ~15% (46 -> 39 ms/op interleaved) and reverted. The cheap way
//     to earn the claim is to stop bailing — raise the JDK's own
//     `IndexOutOfBoundsException` from here instead of deferring to bytecode for
//     it — which would leave only the unresolvable-layout case behind.
//   * **`ByteBuffer.allocateDirect` memory is an `Unsafe`-arena handle, not a
//     real pointer** — `CRATONVM_DBG_DBB_ELEM=1` reports `raw-pointer=0
//     arena-handle=N`. So the structural answer, a JIT intrinsic lowering the
//     element access inline with no call at all, cannot be emitted as things
//     stand: inline code cannot do a locked map probe to resolve a handle.
//
// Both are a bounded project of their own and are deliberately not attempted
// here.

/// Field indices this native family reads out of a `DirectByteBuffer`.
#[derive(Clone, Copy)]
struct DbbElemFields {
    /// `java.nio.Buffer.address` — the base of the off-heap region. Already
    /// includes any slice/duplicate offset, exactly as `ix(int)` assumes.
    address: usize,
    /// `java.nio.Buffer.limit` — the bound `Buffer.checkIndex(int)` enforces.
    /// Note this is the *limit*, not the capacity: `bb.limit(10); bb.get(20)`
    /// must throw even on a 64-byte buffer.
    limit: usize,
    /// `java.nio.ByteBuffer.isReadOnly`.
    is_read_only: usize,
    /// `java.nio.Buffer.position` — the cursor the relative accessors bump.
    position: usize,
    /// `java.nio.ByteBuffer.bigEndian` — the order the WIDE accessors below
    /// encode with. `ByteBuffer.order(ByteOrder)` writes it, and the JDK's own
    /// `getLong(int)` passes it straight to
    /// `ScopedMemoryAccess.getLongUnaligned`, so honouring it is not an
    /// embellishment: a native that assumed big-endian would silently return
    /// byte-swapped values for every `order(LITTLE_ENDIAN)` buffer.
    ///
    /// `Option`, and deliberately NOT a `?` in the constructor below. The four
    /// fields above are what the BYTE accessors need and have needed since
    /// 2026-08-05; folding a fifth resolution into the same `?` would mean an
    /// image without a `bigEndian` field — synthetic-JDK mode, where
    /// `java/nio/ByteBuffer` is this VM's own class — silently sending the
    /// byte accessors back to bytecode as well. A missing `bigEndian` bails
    /// only the wide accessors, which is the blast radius it earns.
    big_endian: Option<usize>,
}

/// `CRATONVM_DBG_DBB_ELEM` — per-accessor census for the element natives.
///
/// The four accessors are the whole of the `nioMemLZF:` throughput gap, and the
/// two questions that decide what to do about it are invisible from a profile:
/// how often each one *bails* to the real bytecode (a bail is ~6 nested
/// dispatches, so a small bail rate dominates the average), and whether the
/// `address` they resolve is a real pointer or a tagged
/// `Unsafe.allocateMemory` arena handle (an inline JIT lowering could only be
/// emitted for the former). Both are counted here.
mod elem_census {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub(super) const GET_ABS: usize = 0;
    pub(super) const PUT_ABS: usize = 1;
    pub(super) const GET_REL: usize = 2;
    pub(super) const PUT_REL: usize = 3;
    const NAMES: [&str; 4] = ["get(int)", "put(int,byte)", "get()", "put(byte)"];

    /// `unsafe_natives_ext::unsafe_arena::ARENA_TAG` — the reserved high bit
    /// every `Unsafe.allocateMemory` handle carries and no real OS pointer ever
    /// has. Duplicated as a constant rather than imported because `native-io`
    /// does not depend on `native-builtins`; the invariant it encodes is
    /// documented at the definition.
    const ARENA_TAG: i64 = 1 << 62;
    /// Print the running census every this many accesses. Rust does not drop
    /// statics at exit and this module has no VM-shutdown hook, so an interval
    /// dump is the only form guaranteed to be seen.
    const DUMP_EVERY: u64 = 2_000_000;

    static SERVED: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static BAILED: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static TAGGED: AtomicU64 = AtomicU64::new(0);
    static RAW: AtomicU64 = AtomicU64::new(0);
    static TOTAL: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(super) fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DBB_ELEM").is_some())
    }

    /// Record a served access and classify its resolved address. `addr` is the
    /// absolute element address, so its tag bit is the buffer's.
    #[inline]
    pub(super) fn served(kind: usize, addr: i64) {
        if !enabled() {
            return;
        }
        SERVED[kind].fetch_add(1, Ordering::Relaxed);
        if addr & ARENA_TAG != 0 {
            TAGGED.fetch_add(1, Ordering::Relaxed);
        } else {
            RAW.fetch_add(1, Ordering::Relaxed);
        }
        tick();
    }

    #[inline]
    pub(super) fn bailed(kind: usize) {
        if !enabled() {
            return;
        }
        BAILED[kind].fetch_add(1, Ordering::Relaxed);
        tick();
    }

    fn tick() {
        if (TOTAL.fetch_add(1, Ordering::Relaxed) + 1) % DUMP_EVERY != 0 {
            return;
        }
        for k in 0..4 {
            let s = SERVED[k].load(Ordering::Relaxed);
            let b = BAILED[k].load(Ordering::Relaxed);
            if s | b != 0 {
                eprintln!("[dbb-elem] {:14} served={s} bailed={b}", NAMES[k]);
            }
        }
        eprintln!(
            "[dbb-elem] address kind: raw-pointer={} arena-handle={}",
            RAW.load(Ordering::Relaxed),
            TAGGED.load(Ordering::Relaxed),
        );
    }
}

/// What the single-byte element accessors have PROVEN, published so the JIT can
/// serve those two calls without the native funnel.
///
/// # Why anything is published at all
///
/// `--dump-native-registry` on netty's `AbstractIntegrationTest.testHugeDecompress`
/// puts `DirectByteBuffer.put(int,byte)` and `get(int)` at the top of the census:
/// one funnel round trip per BYTE, 268 million of each. The bodies below are a
/// bounds check and a one-byte copy; everything else is the ~160 ns generic
/// native dispatch around them, which is what `jit_dbb_put_byte_direct` /
/// `jit_dbb_get_byte_direct` (`vm/src/jit/helpers.rs`) skip.
///
/// # Why the JIT is told rather than asking
///
/// The fast path must not re-derive this layout. A second copy of a field index
/// is exactly how a check goes silently dead, so the numbers here are the ones
/// `dbb_elem_fields` resolved and the class ids are ones an accessor actually
/// SERVED -- not ones anybody looked up by name. Until the funnel has run once
/// nothing is published, `slots()` answers `None`, and every site keeps today's
/// dispatch. That makes the fast path unreachable before the slow path has
/// agreed with it, which is the direction that cannot be wrong.
pub mod elem_fastpath {
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    /// `-1` until `dbb_elem_fields` has resolved this VM's layout.
    static ADDRESS_SLOT: AtomicI32 = AtomicI32::new(-1);
    static LIMIT_SLOT: AtomicI32 = AtomicI32::new(-1);
    static READONLY_SLOT: AtomicI32 = AtomicI32::new(-1);
    /// Receiver class ids an accessor has served, one pair per direction.
    /// `DirectByteBuffer` and `DirectByteBufferR` are the two the JDK has; a
    /// third would simply not be served here.
    static SERVED_GET: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
    static SERVED_PUT: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

    /// `(address, limit, is_read_only)` field indices, or `None` while the
    /// funnel has not resolved them.
    pub fn slots() -> Option<(usize, usize, usize)> {
        let a = ADDRESS_SLOT.load(Ordering::Relaxed);
        let l = LIMIT_SLOT.load(Ordering::Relaxed);
        let r = READONLY_SLOT.load(Ordering::Relaxed);
        if a < 0 || l < 0 || r < 0 {
            return None;
        }
        // Casts: each was stored from a `usize` field index below.
        Some((a as usize, l as usize, r as usize))
    }

    /// Has an accessor of this direction actually served a receiver of this
    /// class? `write` selects the put table, which a read-only carrier can
    /// never enter because `dbb_elem_addr(for_write = true)` refuses it.
    pub fn class_is_served(class_id: u32, write: bool) -> bool {
        if class_id == 0 {
            return false;
        }
        let table = if write { &SERVED_PUT } else { &SERVED_GET };
        table.iter().any(|c| c.load(Ordering::Relaxed) == class_id)
    }

    pub(super) fn publish_slots(address: usize, limit: usize, read_only: usize) {
        // Cast: a field index, far below `i32::MAX`. A layout that somehow
        // exceeded it stays unpublished rather than wrapping into a valid-
        // looking slot.
        let fit = |v: usize| i32::try_from(v).unwrap_or(-1);
        ADDRESS_SLOT.store(fit(address), Ordering::Relaxed);
        LIMIT_SLOT.store(fit(limit), Ordering::Relaxed);
        READONLY_SLOT.store(fit(read_only), Ordering::Relaxed);
    }

    pub(super) fn note_served(class_id: u32, write: bool) {
        if class_id == 0 {
            return;
        }
        let table = if write { &SERVED_PUT } else { &SERVED_GET };
        for cell in table.iter() {
            match cell.load(Ordering::Relaxed) {
                v if v == class_id => return,
                0 => {
                    // A racing writer that wins stores a class id that was also
                    // served, so a lost race costs nothing.
                    let _ =
                        cell.compare_exchange(0, class_id, Ordering::Relaxed, Ordering::Relaxed);
                    return;
                }
                _ => {}
            }
        }
    }
}

/// Resolve (once) the field indices used by the element accessors.
///
/// `DirectByteBufferR` adds no instance fields of its own, so a single
/// resolution against `java/nio/DirectByteBuffer` covers both receivers.
/// `None` means "this VM's layout is not the one modelled here" and every
/// caller bails to bytecode.
fn dbb_elem_fields(ctx: &mut dyn NativeContext) -> Option<DbbElemFields> {
    static CACHE: OnceLock<Option<DbbElemFields>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        const CLASS: &str = "java/nio/DirectByteBuffer";
        let fields = DbbElemFields {
            address: ctx.resolve_field_index(CLASS, "address")?,
            limit: ctx.resolve_field_index(CLASS, "limit")?,
            is_read_only: ctx.resolve_field_index(CLASS, "isReadOnly")?,
            position: ctx.resolve_field_index(CLASS, "position")?,
            big_endian: ctx.resolve_field_index(CLASS, "bigEndian"),
        };
        elem_fastpath::publish_slots(fields.address, fields.limit, fields.is_read_only);
        Some(fields)
    })
}

/// Shared prologue: validate the receiver + index against the modelled layout
/// and return the absolute address of the element, or `None` to bail.
fn dbb_elem_addr(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: i32,
    for_write: bool,
) -> Option<i64> {
    let fields = dbb_elem_fields(ctx)?;
    // One batched, descriptor-hinted read rather than three separate
    // `get_field` calls. The descriptors are fixed by `java.nio.Buffer`'s own
    // declarations, so they are exactly the answer the per-read metadata lookup
    // inside `get_field` would have produced — and on the one-byte-per-call
    // path, that lookup (and the reference forwarding in front of it) ran three
    // times per element moved. `isReadOnly` is read unconditionally: for a read
    // access its value is simply unused, which is cheaper than splitting the
    // batch.
    let mut vals = [Value::Int(0); 3];
    ctx.get_fields_typed(
        this,
        &[
            (fields.limit, b'I'),
            (fields.address, b'J'),
            (fields.is_read_only, b'Z'),
        ],
        &mut vals,
    );
    let Value::Int(limit) = vals[0] else {
        return None;
    };
    if index < 0 || index >= limit {
        return None;
    }
    if for_write {
        // `Value::Int(0)` is the only shape that proves writability.
        if !matches!(vals[2], Value::Int(0)) {
            return None;
        }
    }
    let Value::Long(address) = vals[1] else {
        return None;
    };
    if address <= 0 {
        return None;
    }
    // `ix(i)` is `address + ((long) i << 0)`; `index` is non-negative and
    // `address` is positive, so this cannot wrap.
    address.checked_add(i64::from(index))
}

/// `java.nio.DirectByteBuffer.get(int)` — absolute single-byte read.
fn dbb_get_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return dbb_elem_bail_get(ctx, this, args),
    };
    let Some(addr) = dbb_elem_addr(ctx, this, index, false) else {
        return dbb_elem_bail_get(ctx, this, args);
    };
    let mut byte = [0u8; 1];
    if !ctx.copy_from_native_memory(addr, &mut byte) {
        return dbb_elem_bail_get(ctx, this, args);
    }
    elem_census::served(elem_census::GET_ABS, addr);
    // This receiver's class has now been served by the modelled layout, so the
    // JIT's thin bind may serve the same class without the funnel.
    elem_fastpath::note_served(ctx.class_id_of_object(this).as_u32(), false);
    // `ByteBuffer.get` returns a Java `byte` — signed. Route through `i8` so a
    // value >= 0x80 sign-extends the way every `b < 0` caller expects.
    Ok(Some(Value::Int(i32::from(byte[0] as i8))))
}

/// `java.nio.DirectByteBuffer.put(int, byte)` — absolute single-byte write.
fn dbb_put_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let (index, byte) = match (args.get(1), args.get(2)) {
        (Some(Value::Int(i)), Some(Value::Int(b))) => (*i, *b),
        _ => return dbb_elem_bail_put(ctx, this, args),
    };
    let Some(addr) = dbb_elem_addr(ctx, this, index, true) else {
        return dbb_elem_bail_put(ctx, this, args);
    };
    // Only the low 8 bits are the `byte`; the operand stack widens it to int.
    if !ctx.copy_to_native_memory(addr, &[byte as u8]) {
        return dbb_elem_bail_put(ctx, this, args);
    }
    elem_census::served(elem_census::PUT_ABS, addr);
    // Served by the modelled layout AND writable — see `elem_fastpath`.
    elem_fastpath::note_served(ctx.class_id_of_object(this).as_u32(), true);
    // `put(int, byte)` returns `this`.
    Ok(Some(Value::Object(Some(this))))
}

// ── Wide absolute accessors (`getLong(int)` and friends) ─────────────────
//
// WHY THESE EXIST, measured rather than assumed. `--dump-native-registry`'s
// invocation census on `probes/NioAccessorRate.java` (2026-08-18, current dev,
// 1 600 000 operations per arm) says what one `DirectByteBuffer.putLong(int,
// long)` actually executes:
//
//   | invocations | native                                     |
//   |------------:|--------------------------------------------|
//   |   4 800 000 | `java/nio/DirectByteBuffer.session()`       |
//   |   3 200 000 | `ScopedMemoryAccess.putLongUnaligned(…)`    |
//   |   1 600 000 | `java/nio/HeapByteBuffer.session()`         |
//   |   1 600 000 | `ScopedMemoryAccess.getLongUnaligned(…)`    |
//   |   1 600 000 | `ScopedMemoryAccess.putIntUnaligned(…)`     |
//
// which is exactly **two native calls per wide accessor** — `session()` (a
// shim returning the constant `null`, registered for the checkcast reason
// documented at its own registration in `native-builtins/src/lib.rs`) and the
// `ScopedMemoryAccess` store itself. Against that, the single-byte
// `put(int, byte)` already served here is **one** native call, and the two
// measure 616 ns and 271 ns per operation on the same host and the same
// binary. The arithmetic closes on ~290 ns per registered-native call: the
// wide accessors are not paying for width, they are paying for the extra rung.
//
// Serving them here removes both rungs at once. The JDK's `getLong(int)` body
// is `SCOPED_MEMORY_ACCESS.getLongUnaligned(session(), null, ix(checkIndex(i,
// 8)), bigEndian)` inside a `reachabilityFence`, and every part of that is
// reproduced below out of the same fields the byte accessors already read.
//
// The refusals are deliberately the SAME as the byte accessors': an
// unresolvable layout, an out-of-range index, a read-only receiver, or an
// address the memory layer declines all bail to the real class-file body, so
// the exception this VM raises is always the JDK's own. See the block comment
// above `DbbElemFields` for why that bail is also why these cannot claim LEAF.
//
// `DirectByteBufferR` overrides every `put*` with a throwing body, so the
// hierarchy walk never reaches the `put` registrations for a read-only
// receiver; the `for_write` check below is the belt to that braces.

/// Resolve one wide absolute access: bounds-check `index` for `nb` bytes the
/// way `Buffer.checkIndex(int, int)` does, and answer the element address plus
/// the buffer's byte order.
///
/// `Buffer.checkIndex(i, nb)` is `Preconditions.checkIndex(i, limit - nb + 1,
/// …)`, i.e. `0 <= i && i + nb <= limit`. Written as `index > limit - nb` so
/// no addition can overflow for a hostile `index`.
fn dbb_wide_addr(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: i32,
    nb: i32,
    for_write: bool,
) -> Option<(i64, bool)> {
    let fields = dbb_elem_fields(ctx)?;
    let big_endian_slot = fields.big_endian?;
    let mut vals = [Value::Int(0); 4];
    ctx.get_fields_typed(
        this,
        &[
            (fields.limit, b'I'),
            (fields.address, b'J'),
            (fields.is_read_only, b'Z'),
            (big_endian_slot, b'Z'),
        ],
        &mut vals,
    );
    let Value::Int(limit) = vals[0] else {
        return None;
    };
    if index < 0 || limit < nb || index > limit - nb {
        return None;
    }
    if for_write {
        // `Value::Int(0)` is the only shape that proves writability.
        if !matches!(vals[2], Value::Int(0)) {
            return None;
        }
    }
    let Value::Int(be) = vals[3] else {
        return None;
    };
    let Value::Long(address) = vals[1] else {
        return None;
    };
    if address <= 0 {
        return None;
    }
    // `index` is non-negative and `address` positive, so this cannot wrap.
    Some((address.checked_add(i64::from(index))?, be != 0))
}

/// Read `nb` bytes (2, 4 or 8) at `addr` and assemble them in `big_endian`
/// order into the low bits of an `i64`.
fn dbb_wide_load(
    ctx: &mut dyn NativeContext,
    addr: i64,
    nb: usize,
    big_endian: bool,
) -> Option<i64> {
    let mut buf = [0u8; 8];
    if !ctx.copy_from_native_memory(addr, &mut buf[..nb]) {
        return None;
    }
    let mut acc: u64 = 0;
    if big_endian {
        for &b in &buf[..nb] {
            acc = (acc << 8) | u64::from(b);
        }
    } else {
        for (i, &b) in buf[..nb].iter().enumerate() {
            acc |= u64::from(b) << (8 * i);
        }
    }
    // Cast: a bit pattern; the caller narrows and extends per its Java type.
    Some(acc as i64)
}

/// Store the low `nb` bytes of `bits` at `addr` in `big_endian` order.
fn dbb_wide_store(
    ctx: &mut dyn NativeContext,
    addr: i64,
    nb: usize,
    big_endian: bool,
    bits: i64,
) -> bool {
    let mut buf = [0u8; 8];
    // Cast: a bit pattern, not a magnitude.
    let raw = bits as u64;
    for (i, slot) in buf[..nb].iter_mut().enumerate() {
        let shift = if big_endian { 8 * (nb - 1 - i) } else { 8 * i };
        // Cast: truncation to one byte is the intent.
        *slot = (raw >> shift) as u8;
    }
    ctx.copy_to_native_memory(addr, &buf[..nb])
}

/// The Java-visible shape of one wide accessor: how many bytes it moves, and
/// how the assembled bits become a `Value` (or come from one).
#[derive(Clone, Copy)]
enum WideKind {
    /// `short` — sign-extended into an int slot.
    Short,
    /// `char` — zero-extended into an int slot.
    Char,
    Int,
    Long,
    Float,
    Double,
}

impl WideKind {
    fn width(self) -> usize {
        match self {
            WideKind::Short | WideKind::Char => 2,
            WideKind::Int | WideKind::Float => 4,
            WideKind::Long | WideKind::Double => 8,
        }
    }

    /// Turn the loaded bits into the `Value` the descriptor promises.
    fn to_value(self, bits: i64) -> Value {
        match self {
            // Cast chain: take the low 16 bits, then sign- or zero-extend, the
            // way `getShort`/`getChar` differ in the JDK.
            WideKind::Short => Value::Int(i32::from(bits as u16 as i16)),
            WideKind::Char => Value::Int(i32::from(bits as u16)),
            WideKind::Int => Value::Int(bits as i32),
            WideKind::Long => Value::Long(bits),
            WideKind::Float => Value::Float(f32::from_bits(bits as u32)),
            WideKind::Double => Value::Double(f64::from_bits(bits as u64)),
        }
    }

    /// Turn the argument `Value` into the bits to store, or `None` when the
    /// operand is not the shape the descriptor declares — which bails to the
    /// class-file body rather than storing a guess.
    fn from_value(self, v: Value) -> Option<i64> {
        Some(match (self, v) {
            (WideKind::Short | WideKind::Char, Value::Int(x)) => i64::from(x as u16),
            (WideKind::Int, Value::Int(x)) => i64::from(x as u32),
            (WideKind::Long, Value::Long(x)) => x,
            (WideKind::Float, Value::Float(x)) => i64::from(x.to_bits()),
            // Cast: a bit pattern, re-read as an i64 by the store.
            (WideKind::Double, Value::Double(x)) => x.to_bits() as i64,
            _ => return None,
        })
    }
}

/// One registered wide getter. `name`/`descriptor` are used only to bail back
/// to the class-file body, so they must match the registration exactly.
fn dbb_wide_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: WideKind,
    name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(kind.to_value(0)));
    };
    let Some(Value::Int(index)) = args.get(1).copied() else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    // Cast: `width()` is 2, 4 or 8.
    let Some((addr, be)) = dbb_wide_addr(ctx, this, index, kind.width() as i32, false) else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    let Some(bits) = dbb_wide_load(ctx, addr, kind.width(), be) else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    Ok(Some(kind.to_value(bits)))
}

/// One registered wide setter. Returns `this`, as every `ByteBuffer.put*` does.
fn dbb_wide_put(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: WideKind,
    name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let (Some(Value::Int(index)), Some(raw)) = (args.get(1).copied(), args.get(2).copied()) else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    let Some(bits) = kind.from_value(raw) else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    // Cast: `width()` is 2, 4 or 8.
    let Some((addr, be)) = dbb_wide_addr(ctx, this, index, kind.width() as i32, true) else {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    };
    if !dbb_wide_store(ctx, addr, kind.width(), be, bits) {
        return ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]);
    }
    Ok(Some(Value::Object(Some(this))))
}

fn dbb_elem_bail_get(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    elem_census::bailed(elem_census::GET_ABS);
    ctx.invoke_virtual_bytecode_only(this, "get", "(I)B", &args[1..])
}

fn dbb_elem_bail_put(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    elem_census::bailed(elem_census::PUT_ABS);
    ctx.invoke_virtual_bytecode_only(this, "put", "(IB)Ljava/nio/ByteBuffer;", &args[1..])
}

/// Shared prologue for the RELATIVE accessors: validate the receiver, and
/// return `(element_address, position)` so the caller can commit
/// `position + 1` only after the access has actually succeeded.
///
/// The JDK's `nextGetIndex()`/`nextPutIndex()` bump `position` *before* the
/// memory access, so a failing access still consumes the slot. This does the
/// opposite deliberately: every failure here is a bail, and the bytecode we
/// bail to runs `nextGetIndex()` itself. Committing first would advance
/// `position` twice for one logical element.
fn dbb_rel_addr(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    for_write: bool,
) -> Option<(i64, i32)> {
    let fields = dbb_elem_fields(ctx)?;
    // One batched read — see `dbb_elem_addr`.
    let mut vals = [Value::Int(0); 4];
    ctx.get_fields_typed(
        this,
        &[
            (fields.position, b'I'),
            (fields.limit, b'I'),
            (fields.address, b'J'),
            (fields.is_read_only, b'Z'),
        ],
        &mut vals,
    );
    let (Value::Int(position), Value::Int(limit)) = (vals[0], vals[1]) else {
        return None;
    };
    // `position < 0` cannot happen through the public API, but a bail costs
    // nothing and keeps the arithmetic below provably non-negative.
    if position < 0 || position >= limit {
        // BufferUnderflowException / BufferOverflowException — thrown by the
        // real `nextGetIndex()` / `nextPutIndex()`.
        return None;
    }
    if for_write && !matches!(vals[3], Value::Int(0)) {
        return None;
    }
    let Value::Long(address) = vals[2] else {
        return None;
    };
    if address <= 0 {
        return None;
    }
    Some((address.checked_add(i64::from(position))?, position))
}

/// `java.nio.DirectByteBuffer.get()` — relative single-byte read.
fn dbb_get_rel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    let Some((addr, position)) = dbb_rel_addr(ctx, this, false) else {
        elem_census::bailed(elem_census::GET_REL);
        return ctx.invoke_virtual_bytecode_only(this, "get", "()B", &[]);
    };
    let mut byte = [0u8; 1];
    if !ctx.copy_from_native_memory(addr, &mut byte) {
        elem_census::bailed(elem_census::GET_REL);
        return ctx.invoke_virtual_bytecode_only(this, "get", "()B", &[]);
    }
    elem_census::served(elem_census::GET_REL, addr);
    dbb_commit_position(ctx, this, position + 1);
    Ok(Some(Value::Int(i32::from(byte[0] as i8))))
}

/// `java.nio.DirectByteBuffer.put(byte)` — relative single-byte write.
fn dbb_put_rel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let byte = match args.get(1) {
        Some(Value::Int(b)) => *b,
        _ => {
            elem_census::bailed(elem_census::PUT_REL);
            return ctx.invoke_virtual_bytecode_only(
                this,
                "put",
                "(B)Ljava/nio/ByteBuffer;",
                &args[1..],
            );
        }
    };
    let Some((addr, position)) = dbb_rel_addr(ctx, this, true) else {
        elem_census::bailed(elem_census::PUT_REL);
        return ctx.invoke_virtual_bytecode_only(
            this,
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &args[1..],
        );
    };
    if !ctx.copy_to_native_memory(addr, &[byte as u8]) {
        elem_census::bailed(elem_census::PUT_REL);
        return ctx.invoke_virtual_bytecode_only(
            this,
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &args[1..],
        );
    }
    elem_census::served(elem_census::PUT_REL, addr);
    dbb_commit_position(ctx, this, position + 1);
    Ok(Some(Value::Object(Some(this))))
}

/// Advance `position` after a successful relative access. Split out so the
/// `dbb_elem_fields` re-resolution is a single memoised call, not an
/// `Option` the two callers have to re-unwrap.
fn dbb_commit_position(ctx: &mut dyn NativeContext, this: ObjectRef, new_position: i32) {
    if let Some(fields) = dbb_elem_fields(ctx) {
        ctx.set_field(this, fields.position, Value::Int(new_position));
    }
}

// JDK-ONLY-CLASSIFY: unknown — needs census. Direct buffers ARE a memory
// boundary, which is bridge territory under jdk-only-native-review.md §5, but
// not one of the 25 statically resolvable triples here is ACC_NATIVE in JDK 25:
// 9 shadow concrete bytecode, 7 name methods absent from the image, 2 name
// absent classes and 2 have a descriptor that does not match the real one. In
// JDK 25 the actual native boundary for direct memory is `jdk.internal.misc.
// Unsafe`, not `java.nio.Bits` / `DirectByteBuffer`, so several of these look
// like they are bridging one layer too high. Evidence needed: `invocations`
// plus `real_declaring_method` per triple before promoting or demoting any of
// them — and note the descriptor mismatches are dead registrations either way.
/// Register the WP3.5 DirectByteBuffer + Cleaner natives.  Idempotent:
/// safe to call multiple times.  See module docs for FQN list and
/// caveats around partial WP1.10 Cleaner integration.
/// Define and register the twelve wide absolute accessors.
///
/// The registry stores a bare `fn` pointer, so each triple needs its own
/// monomorphic function — a closure capturing `name`/`descriptor` (which the
/// bail path needs to reach the class-file body) cannot coerce to one. The
/// macro writes those twelve functions so the table and the registrations
/// cannot drift apart.
macro_rules! dbb_wide_accessors {
    (
        get { $($gfn:ident => ($gname:literal, $gdesc:literal, $gkind:expr)),* $(,)? }
        put { $($pfn:ident => ($pname:literal, $pdesc:literal, $pkind:expr)),* $(,)? }
    ) => {
        $(
            fn $gfn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                dbb_wide_get(ctx, args, $gkind, $gname, $gdesc)
            }
        )*
        $(
            fn $pfn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                dbb_wide_put(ctx, args, $pkind, $pname, $pdesc)
            }
        )*
        fn dbb_register_wide(r: &mut NativeMethodRegistry) {
            $( r.register("java/nio/DirectByteBuffer", $gname, $gdesc, $gfn); )*
            $( r.register("java/nio/DirectByteBuffer", $pname, $pdesc, $pfn); )*
        }
    };
}

dbb_wide_accessors! {
    get {
        dbb_get_short_abs  => ("getShort",  "(I)S", WideKind::Short),
        dbb_get_char_abs   => ("getChar",   "(I)C", WideKind::Char),
        dbb_get_int_abs    => ("getInt",    "(I)I", WideKind::Int),
        dbb_get_long_abs   => ("getLong",   "(I)J", WideKind::Long),
        dbb_get_float_abs  => ("getFloat",  "(I)F", WideKind::Float),
        dbb_get_double_abs => ("getDouble", "(I)D", WideKind::Double),
    }
    put {
        dbb_put_short_abs  => ("putShort",  "(IS)Ljava/nio/ByteBuffer;", WideKind::Short),
        dbb_put_char_abs   => ("putChar",   "(IC)Ljava/nio/ByteBuffer;", WideKind::Char),
        dbb_put_int_abs    => ("putInt",    "(II)Ljava/nio/ByteBuffer;", WideKind::Int),
        dbb_put_long_abs   => ("putLong",   "(IJ)Ljava/nio/ByteBuffer;", WideKind::Long),
        dbb_put_float_abs  => ("putFloat",  "(IF)Ljava/nio/ByteBuffer;", WideKind::Float),
        dbb_put_double_abs => ("putDouble", "(ID)Ljava/nio/ByteBuffer;", WideKind::Double),
    }
}

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
        "unreserveMemory",
        "(JJ)V",
        bits_unreserve_memory,
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
    // Round-5 Fix 6: GC-driven Cleaner runnable for `dbb_allocate_direct0`'s
    // bucketed pool path. Pairs with the discover_reference call inside that
    // function so the ref processor drains and fires this on phantom-clear.
    // The 8u/16+ name is `clean`; register alias.

    // `Util$BufferCache` is normally ThreadLocal. VM worker threads can
    // currently share it, so make each ring operation atomic while retaining
    // buffers in the Java-owned cache (rather than allocating a Cleaner on
    // Avoid the racy Java BufferCache while preserving real direct buffers
    // and their Cleaner contract. The pool is local to each native worker
    // thread and bounded to three buffers.
    r.register(
        "sun/nio/ch/Util",
        "getTemporaryDirectBuffer",
        "(I)Ljava/nio/ByteBuffer;",
        temporary_direct_buffer_get,
    );
    for method in [
        "releaseTemporaryDirectBuffer",
        "offerFirstTemporaryDirectBuffer",
        "offerLastTemporaryDirectBuffer",
    ] {
        r.register(
            "sun/nio/ch/Util",
            method,
            "(Ljava/nio/ByteBuffer;)V",
            temporary_direct_buffer_release,
        );
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
    // `jdk.internal.misc.Unsafe.allocateMemory0`/`freeMemory0` are ACC_NATIVE on
    // both the Linux and the Windows JDK 25 image; the un-suffixed pair is the
    // Java wrapper that calls them (a §1.4 shadow), and `sun.misc.Unsafe`
    // declares none of the four on either image. Only what the image backs
    // states its kind.
    for cls in ["jdk/internal/misc/Unsafe", "sun/misc/Unsafe"] {
        r.register(cls, "allocateMemory", "(J)J", unsafe_allocate_memory);
        r.register(cls, "freeMemory", "(J)V", unsafe_free_memory);
    }
    r.register_with_kind(
        "jdk/internal/misc/Unsafe",
        "allocateMemory0",
        "(J)J",
        unsafe_allocate_memory,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        "jdk/internal/misc/Unsafe",
        "freeMemory0",
        "(J)V",
        unsafe_free_memory,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // Synthetic helper used by JDK-side Cleaner runnables that
    // capture (addr, size) at allocation time — see module docs.
    r.register(
        "java/nio/DirectByteBuffer",
        "freeMemoryExplicit",
        "(JJ)V",
        dbb_free_explicit,
    );

    // Per-element absolute accessors. See the block comment above
    // `DbbElemFields` for why these exist and what they deliberately refuse
    // to model. Registered on `DirectByteBuffer` only: `DirectByteBufferR`
    // has its own `put(int, byte)` bytecode (which throws), and inherits
    // `get(int)`, so the hierarchy walk reaches this `get` for a read-only
    // receiver and never reaches this `put`.
    r.register("java/nio/DirectByteBuffer", "get", "(I)B", dbb_get_abs);
    r.register(
        "java/nio/DirectByteBuffer",
        "put",
        "(IB)Ljava/nio/ByteBuffer;",
        dbb_put_abs,
    );
    // The WIDE absolute accessors — see the block comment above
    // `dbb_wide_addr` for the invocation census that motivates them. Same
    // receiver-class reasoning as the byte pair above: registered on
    // `DirectByteBuffer` only, because `DirectByteBufferR` declares its own
    // throwing `put*` bodies and inherits the getters.
    //
    // One named `fn` per triple rather than a loop over a table: the registry
    // takes a bare `fn` pointer, so a closure that captured the name and the
    // descriptor cannot be registered — and those two are exactly what the
    // bail path needs to reach the class-file body.
    dbb_register_wide(r);
    r.register("java/nio/DirectByteBuffer", "get", "()B", dbb_get_rel);
    r.register(
        "java/nio/DirectByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        dbb_put_rel,
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Serialize tests that mutate the global `Bits` accounting state.
    /// Without this, parallel `cargo test` runs see racey reserved-byte
    /// counters across tests.
    fn bits_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// A pooled temporary direct buffer must not leak its global root when it
    /// is handed back out. `TEMPORARY_BUFFER_POOL_LIMIT` bounds the pool at
    /// three entries per thread; the JNI global-ref table backing those entries
    /// is not bounded, so a `get` that reused an entry without releasing its
    /// handle leaked one root per NIO transfer — 57 036 unreclaimable roots
    /// against 7, over three H2 `TestFileSystem` filesystems in ~4.5 s. Without
    /// the release this test sees exactly one leaked root per cycle.
    #[test]
    fn temporary_direct_buffer_reuse_releases_the_pooled_root() {
        // The pool is thread-local and outlives any one test; start from empty
        // so the baseline below describes only this test's roots.
        TEMPORARY_BUFFERS.with(|e| e.borrow_mut().clear());
        let mut ctx = MockNativeContext::new();
        let baseline = ctx.global_root_count();

        const CYCLES: usize = 50;
        let mut first = None;
        let mut last = None;
        for _ in 0..CYCLES {
            let Ok(Some(Value::Object(Some(buffer)))) =
                temporary_direct_buffer_get(&mut ctx, &[Value::Int(4096)])
            else {
                panic!("getTemporaryDirectBuffer returned no buffer");
            };
            // `capacity` is what the release path screens on; the mock's
            // constructor does not populate it.
            ctx.set_field_by_name(buffer, "capacity", Value::Int(4096));
            first.get_or_insert(buffer);
            last = Some(buffer);
            temporary_direct_buffer_release(&mut ctx, &[Value::Object(Some(buffer))])
                .expect("releaseTemporaryDirectBuffer");
        }

        // If the pool never hit, the leak this guards could not arise and the
        // count assertion below would pass for the wrong reason.
        assert_eq!(first, last, "the pooled buffer was never reused");
        let leaked = ctx.global_root_count();
        assert!(
            leaked <= baseline + TEMPORARY_BUFFER_POOL_LIMIT,
            "{CYCLES} get/release cycles left {leaked} global roots \
             (baseline {baseline}); the pool holds at most \
             {TEMPORARY_BUFFER_POOL_LIMIT}"
        );
        TEMPORARY_BUFFERS.with(|e| e.borrow_mut().clear());
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
    fn bug_h2_largeblob_configure_max_direct_memory_round_trips() {
        let _g = bits_test_lock();
        let saved = bits().max.load(Ordering::Relaxed);
        configure_max_direct_memory(777 * 1024 * 1024);
        assert_eq!(bits().max.load(Ordering::Relaxed), 777 * 1024 * 1024);
        // Negative input (e.g. an overflowed/garbage config value) must clamp
        // to 0 rather than going negative, which would make every reservation
        // trivially pass the `next > max` check below zero.
        configure_max_direct_memory(-5);
        assert_eq!(bits().max.load(Ordering::Relaxed), 0);
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

/// DBG (CRATONVM_DBG_DM) -- see `gc_and_alloc::dm_dbg_enabled`. Cached gate.
fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DM").is_some())
}
