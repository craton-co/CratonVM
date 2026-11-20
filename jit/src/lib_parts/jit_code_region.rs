#[allow(unused_imports)]
use super::super::*;


// ---------------------------------------------------------------------------
// JIT code region tracking — validates pointers before transmute to fn ptrs
// ---------------------------------------------------------------------------

/// Tracks allocated JIT code regions for pointer validation before transmute.
///
/// Every `ExecutableBuffer` registers its address range on creation and
/// deregisters on drop. Before transmuting a `*const u8` to a function pointer,
/// callers should use `validate_code_ptr` to confirm it points within a known
/// JIT region.
pub struct JitCodeRegion {
    /// (start_addr, end_addr_exclusive), kept sorted ascending by start_addr.
    ///
    /// Regions are non-overlapping (each is a distinct executable allocation),
    /// so sorting by start address also sorts by end address. This lets
    /// `contains` run a binary search instead of an O(n) linear scan — the
    /// list grows with every compiled method and `contains` is hit on every
    /// JIT call via `validate_code_ptr`.
    pub(crate) regions: Vec<(usize, usize)>,
    /// [`REGIONS_EPOCH`] as of the last [`publish_region_snapshot`]. Unequal to
    /// the live epoch means the snapshot lags the list (it may even be empty
    /// after a removal); see [`JIT_CODE_REGION_SNAPSHOT`] for why that is
    /// allowed in one direction.
    pub(crate) published_epoch: u64,
}

impl JitCodeRegion {
    pub(crate) fn new() -> Self {
        Self {
            regions: Vec::new(),
            published_epoch: 0,
        }
    }

    pub(crate) fn register(&mut self, ptr: *const u8, size: usize) {
        let start = ptr as usize;
        let end = start + size;
        // Insert maintaining the sorted-by-start invariant.
        let idx = self.regions.partition_point(|&(s, _)| s < start);
        self.regions.insert(idx, (start, end));
        bump_regions_epoch();
        // Not republished. A region missing from the snapshot only sends
        // `validate_code_ptr` to the locked lookup, which republishes then.
        // Most buffers are compiled, published and called long before anyone
        // validates into them, and a discarded compile is never validated at
        // all, so one copy covers a whole burst instead of one per buffer.
    }

    pub(crate) fn deregister(&mut self, ptr: *const u8) {
        let addr = ptr as usize;
        // Find the (unique) region with this start address and remove it,
        // preserving the sorted order.
        let lo = self.regions.partition_point(|&(s, _)| s < addr);
        if lo >= self.regions.len() || self.regions[lo].0 != addr {
            // Nothing changed, so neither the epoch nor the snapshot moves.
            return;
        }
        self.regions.remove(lo);
        bump_regions_epoch();
        // A snapshot that lags by a removal would validate a pointer into an
        // unmapped buffer, which is the one wrong answer, so a snapshot that
        // still names the region is withdrawn HERE, before `Drop` unmaps.
        //
        // Withdrawn by publishing an EMPTY snapshot, not a filtered copy (round
        // 11, lane `rt`). A copy is O(n) per removal, and removals come in
        // bursts — the retirement-queue drain after an invalidation drops its
        // bodies back to back — so a burst of N cost O(N * n) under this lock.
        // An empty snapshot can only MISS, and a miss already takes the locked
        // lookup and republishes once (`published_epoch` is stale: the bump
        // above moved the epoch). So the whole burst costs one copy, paid by
        // the first validation after it — the same laziness registrations use,
        // and a tier-up's register-then-retire pair now shares that one copy.
        if region_containing_in(&jit_code_region_snapshot().load(), addr).is_some() {
            jit_code_region_snapshot().store(std::sync::Arc::new(Vec::new()));
        }
    }

    pub(crate) fn contains(&self, ptr: *const u8) -> bool {
        self.region_containing(ptr as usize).is_some()
    }

    /// The region covering `addr`, or `None`.
    ///
    /// Binary search: find the last region whose start <= addr, then check it
    /// covers `addr`. Non-overlapping + sorted means this is the only candidate
    /// region.
    ///
    /// Returns the BOUNDS rather than a bool because [`validate_code_ptr`]'s
    /// per-thread memo remembers the region it landed in, so the next pointer
    /// into the same compiled body answers from a register compare instead of
    /// re-taking the global lock.
    pub(crate) fn region_containing(&self, addr: usize) -> Option<(usize, usize)> {
        let idx = self.regions.partition_point(|&(s, _)| s <= addr);
        if idx == 0 {
            return None;
        }
        let (start, end) = self.regions[idx - 1];
        (addr >= start && addr < end).then_some((start, end))
    }
}

pub(crate) fn jit_code_regions() -> &'static Mutex<JitCodeRegion> {
    static REGIONS: std::sync::OnceLock<Mutex<JitCodeRegion>> = std::sync::OnceLock::new();
    REGIONS.get_or_init(|| Mutex::new(JitCodeRegion::new()))
}

/// Generation of the JIT code region list.
///
/// Bumped by [`JitCodeRegion::register`] and by a [`JitCodeRegion::deregister`]
/// that removed something. Those are the only mutators OF THE LIST, and both
/// run with `jit_code_regions()`'s lock held. So a reader that observes the
/// same value at two moments has observed a region list that was
/// byte-identical throughout, and a snapshot stamped with the live value is
/// exact.
///
/// Starts at 0 and only ever rises: one bump per `ExecutableBuffer` create or
/// drop, and one more per [`ExecutableBuffer::finalize`] (see
/// [`note_code_stream_published`]), which does NOT take the lock because it
/// changes no region. A bump with the list unchanged only makes the next
/// snapshot check republish once; it never makes an answer wrong, so "same
/// value at two moments ⇒ identical list" still holds — the converse was never
/// claimed.
pub(crate) static REGIONS_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that the region list changed. Called with the lock held — except by
/// [`note_code_stream_published`], which changes no region and so needs none.
#[inline]
pub(crate) fn bump_regions_epoch() {
    REGIONS_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Record that a code buffer's bytes have just become executable.
///
/// [`validate_code_ptr`] passes [`REGIONS_EPOCH`] to
/// [`platform::synchronize_instruction_stream_for_epoch`], which issues the
/// AArch64 reader-side `ISB` at most once per thread per epoch value. Bumped
/// only at REGISTRATION, the epoch moved BEFORE the body's bytes were written:
/// a thread that dispatched anything while another thread was still emitting
/// into a fresh buffer recorded the post-registration value as "synchronised",
/// and then entered the finished body with no barrier at all, because nothing
/// moved the epoch again between the last byte and publication. The contract
/// on that function is "bumped after the instruction bytes exist and before the
/// entry can be observed", and `finalize` is the one point every buffer passes
/// through that satisfies both halves.
///
/// Costs one relaxed-contention `fetch_add` per compiled body. On x86-64 the
/// barrier itself compiles out; the bump only makes the next snapshot miss
/// republish once, which the registration bump already forced for the same
/// compile.
#[inline]
pub(crate) fn note_code_stream_published() {
    // Release: a reader that Acquire-loads this value also observes the
    // finished bytes (and the RX flip that preceded this call).
    bump_regions_epoch();
}

/// Hits and misses on the [`validate_code_ptr`] memo, so a run can say whether
/// the fast path is being taken at all rather than leaving that to a timing
/// wash. A feature that reports itself on while contributing nothing looks
/// identical to one that helps, in a table of wall times.
///
/// GATED, and that is not a style choice. A `fetch_add` on a process-global
/// `AtomicU64` per compiled call is precisely the shape of serialisation this
/// change exists to remove: the first cut of the memo counted unconditionally
/// and would have replaced a `Mutex` with a contended cache line, which at 25
/// threads is not obviously the better of the two. It is read once through a
/// `OnceLock` and is a relaxed load plus a not-taken branch when off.
pub(crate) static CODE_PTR_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static CODE_PTR_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether to count snapshot hits/misses. `CRATONVM_DBG_JIT_METHOD_STATS`, the
/// same switch that prints them.
#[inline]
pub(crate) fn code_ptr_memo_census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(v) = ON.get() {
        return *v;
    }
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    })
}

#[inline]
pub(crate) fn note_code_ptr_memo(hit: bool) {
    if !code_ptr_memo_census_enabled() {
        return;
    }
    let c = if hit {
        &CODE_PTR_MEMO_HITS
    } else {
        &CODE_PTR_MEMO_MISSES
    };
    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(hits, misses)` on the code-pointer memo this run. Both are zero unless
/// `CRATONVM_DBG_JIT_METHOD_STATS` was set — see the counters' doc.
pub fn code_ptr_memo_stats() -> (u64, u64) {
    (
        CODE_PTR_MEMO_HITS.load(std::sync::atomic::Ordering::Relaxed),
        CODE_PTR_MEMO_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// How many times the region list has changed this run. Printed beside the
/// hit/miss pair because the two failure modes look identical from the ratio
/// alone: a memo that is too NARROW (several hot regions, one slot) and a memo
/// whose epoch churns (every compile flushes every thread) both read as "mostly
/// misses", and they want opposite fixes.
pub fn code_ptr_regions_epoch() -> u64 {
    REGIONS_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Default-ON kill switch for the memo: `CRATONVM_JIT=-code-ptr-memo`.
///
/// Exists so the change can be A/B-ed inside ONE binary. A cross-binary
/// comparison of a ~1.5% symbol, on a host whose run-to-run spread on this
/// workload is 20%, is not a measurement.
pub fn code_ptr_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !false)
}

/// `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` (default ON, `=0` kills), for the VM's
/// compile door: it asks for [`CompileRequest::jvmti_locals_accessible`] only
/// while the elision is on, so a default request takes no JVMTI lock (JIT
/// round 21 wave 24, lane flipprep). The switch keeps its one reader
/// (`ir_lower`).
pub fn fresh_volatile_store_unfenced_enabled() -> bool {
    ir_lower::ir_fresh_volatile_store_unfenced_enabled()
}

/// Lock-free snapshot of the region list, for [`validate_code_ptr`].
///
/// # Why a snapshot and not a memo
///
/// The first two cuts of this were a per-thread memo of the last region(s) this
/// thread validated into, and both were measured and both were too narrow.
/// Every compiled method gets its own [`ExecutableBuffer`], so a thread running
/// interpreted Java calls into a rotating set of regions and a small cache is
/// evicted by the next call. On `H2UpdateScaleProbe 1 2000 10000`:
///
/// | memo | hits | misses | hit rate |
/// |---|---:|---:|---:|
/// | 1 way | 263 299 | 3 225 254 | **7.5 %** |
/// | 16 ways, direct-mapped on the page | 1 163 588 | 2 477 691 | **32 %** |
///
/// A 32 % hit rate is a fast path that pays for itself twice and delivers once.
/// The `region_epoch` printed beside those counters was **1 838** against 3.6 M
/// calls, which is the number that decides the design: the list barely changes,
/// so the right structure is not a cache of PART of it but a whole immutable
/// copy that readers share and a writer replaces. That is
/// read-copy-update, which is what `ArcSwap` is, and `JitCache::methods`
/// already uses it for the identical shape one screen down.
///
/// The snapshot is rebuilt under `jit_code_regions()`'s lock, so a reader never
/// sees a torn one and never takes a lock on a hit.
///
/// # It may lag the list, never name a removed region
///
/// Rebuilding on every change copied the whole list per `ExecutableBuffer`
/// create and drop, which is O(n) per compile and O(n^2) over a run. Now:
///
/// * a registration only bumps the epoch. The new region is absent from the
///   snapshot, so a validation into it misses, takes the lock, finds it, and
///   republishes (see [`validate_code_ptr`]). An absent region costs one slow
///   lookup, never a wrong answer;
/// * a deregistration withdraws the snapshot at once — by storing an EMPTY
///   one — but only if the snapshot still names the region. A snapshot holding
///   a removed region would validate a pointer into an unmapped buffer. The
///   empty snapshot is rebuilt by the next locked lookup exactly as after a
///   registration, so a burst of removals costs one copy rather than one per
///   removal (round 11, lane `rt`).
///
/// So the snapshot may be INCOMPLETE at any moment — it can lag every region,
/// never name a dead one. A reader that needs completeness (the blocked-stack
/// summary, `collect_code_buffers_in_band`) must catch up and load under
/// `jit_code_regions()`'s lock.
pub(crate) static JIT_CODE_REGION_SNAPSHOT: std::sync::OnceLock<arc_swap::ArcSwap<Vec<(usize, usize)>>> =
    std::sync::OnceLock::new();

pub(crate) fn jit_code_region_snapshot() -> &'static arc_swap::ArcSwap<Vec<(usize, usize)>> {
    JIT_CODE_REGION_SNAPSHOT.get_or_init(|| arc_swap::ArcSwap::from_pointee(Vec::new()))
}

/// Republish the snapshot. Called with `jit_code_regions()`'s lock held, so the
/// copy taken here is of a list nobody is editing and the stamp is its epoch.
pub(crate) fn publish_region_snapshot(regions: &mut JitCodeRegion) {
    regions.published_epoch = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    jit_code_region_snapshot().store(std::sync::Arc::new(regions.regions.clone()));
}

/// Validate that a pointer is safe to transmute to a function pointer.
///
/// Checks: non-null, properly aligned, and falls within a known JIT code region.
/// Returns `Ok(())` if valid, or a descriptive error string.
///
/// # Why this is not just a lock and a binary search
///
/// It is on EVERY compiled call -- `CompiledMethod::try_call` and
/// `try_call_with_context` both validate the entry they are about to jump to --
/// and it took a global `std::sync::Mutex` to do it. Single-threaded on the H2
/// UPDATE path that is 1.54% of CPU (`perf`, flat self-attribution,
/// 2026-08-21); at 25 threads it is a serialisation point on a path with
/// nothing else to serialise on, which is the shape
/// `h2-update-path-throughput-RETIRED-20260821.md` calls
/// "genuinely scaling rather than constant-factor work".
///
/// The lock-free snapshot ([`JIT_CODE_REGION_SNAPSHOT`]) takes the lock out of
/// the steady state without weakening the check. It can omit live regions (added
/// since it was built, or withdrawn wholesale by a removal) but never names a
/// removed one, and a miss falls through to the locked lookup, so every answer
/// equals the locked one.
pub fn validate_code_ptr(ptr: *const u8) -> Result<(), &'static str> {
    if ptr.is_null() {
        return Err("null JIT code pointer");
    }
    // Function pointers should be at least 4-byte aligned (ARM64 requires 4,
    // x86-64 has no strict requirement but 4 is reasonable).
    if (ptr as usize) % 4 != 0 {
        return Err("misaligned JIT code pointer");
    }
    let addr = ptr as usize;
    if code_ptr_memo_enabled() {
        // One `ArcSwap` load and a binary search over the thread's borrowed
        // snapshot. No lock, no allocation, and -- unlike the memo this
        // replaced -- an exact answer on the FIRST probe of every region, so
        // there is no hit rate to be disappointed by.
        let snapshot = jit_code_region_snapshot().load();
        let hit = region_containing_in(&snapshot, addr).is_some();
        note_code_ptr_memo(hit);
        if hit {
            // The AArch64 reader-side context-synchronisation event, on the
            // fast arm as well as the slow one.
            //
            // A snapshot HIT does not mean this thread has ever fetched from
            // that region: the snapshot is an `ArcSwap` shared by every
            // thread, so the publishing thread's republish is visible here
            // without this PE having executed anything from the new bytes.
            // Per ARM ARM B2.4.4 that is exactly the case an `ISB` is for.
            //
            // The epoch gate is what makes the call affordable. The
            // unconditional primitive's contract is "before first entering
            // code another thread published", and a dispatcher cannot tell a
            // first entry from a millionth — so an honest reading of it is
            // "before every compiled call", i.e. a full pipeline flush on the
            // hot path of every invocation in the process. B2.4.4 asks for an
            // event per newly-published instruction stream, not per call:
            // once a PE has taken one after the publication, every later
            // fetch on that PE sees the new bytes. `REGIONS_EPOCH` already
            // counts publications, so it answers that question directly and
            // the steady state is two word compares.
            //
            // Compiled out entirely on x86-64, whose fetch is coherent.
            crate::platform::synchronize_instruction_stream_for_epoch(
                REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire),
            );
            return Ok(());
        }
        // A miss here is not automatically an error: registrations do not
        // republish (see `JIT_CODE_REGION_SNAPSHOT`). Fall through to the
        // authoritative locked lookup rather than reporting a region that
        // exists as absent -- the snapshot is an accelerator, never the source
        // of truth.
    }
    let mut regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
    if !regions.contains(ptr) {
        return Err("JIT code pointer outside known code regions");
    }
    // Found under the lock but missed by the snapshot: it lags by an addition.
    // Republish once, so the next call into any region registered since
    // answers lock-free.
    if code_ptr_memo_enabled()
        && regions.published_epoch != REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire)
    {
        publish_region_snapshot(&mut regions);
    }
    // The same event on the locked arm — see the fast arm above for why the
    // epoch gate is the affordable reading of B2.4.4. This is the arm a thread
    // takes the first time it enters a region registered since its last
    // snapshot, which is the likeliest place for the stale-stream hazard to be
    // real.
    crate::platform::synchronize_instruction_stream_for_epoch(
        REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire),
    );
    Ok(())
}

/// [`JitCodeRegion::region_containing`] over a bare sorted slice, so the
/// snapshot and the locked list run the identical search.
#[inline]
pub(crate) fn region_containing_in(regions: &[(usize, usize)], addr: usize) -> Option<(usize, usize)> {
    let idx = regions.partition_point(|&(s, _)| s <= addr);
    if idx == 0 {
        return None;
    }
    let (start, end) = regions[idx - 1];
    (addr >= start && addr < end).then_some((start, end))
}

// Compile-time size assertions for Value on all platforms.
// Value must be exactly 16 bytes for JIT slot layout. If this fails on a new
// platform, the JIT code-gen must be updated before it can be used there.
const _: () = assert!(
    std::mem::size_of::<Value>() == 16,
    "Value must be 16 bytes for JIT slot layout"
);
const _: () = assert!(
    std::mem::align_of::<Value>() <= 8,
    "Value alignment must not exceed 8 bytes"
);
// ObjectRef must be pointer-sized.
const _: () = assert!(
    std::mem::size_of::<ObjectRef>() == std::mem::size_of::<*mut u8>(),
    "ObjectRef must be pointer-sized"
);

// ---------------------------------------------------------------------------
// Value layout probing — pointer offset within Value::Object(Some(..))
// ---------------------------------------------------------------------------

/// Read the raw 16-byte representation of a Value.
///
/// Safety: Value is compile-time asserted to be exactly 16 bytes.
/// The transmute is safe because both types are the same size and
/// `[u8; 16]` has no alignment or validity requirements.
/// Value is Copy, so no drop semantics are affected.
#[inline]
pub(crate) fn value_to_bytes(val: Value) -> [u8; 16] {
    // Safety: size_of::<Value>() == 16 (asserted above). Both src and dst
    // types are 16 bytes. [u8; 16] accepts any bit pattern.
    unsafe { std::mem::transmute(val) }
}

/// Probe the byte offset of the pointer within Value::Object(Some(..)).
/// Used by JIT inline aaload to extract the pointer from a 16-byte Value slot.
pub fn probe_object_ptr_offset() -> usize {
    // Use an 8-byte aligned marker to satisfy ObjectRef's alignment debug_assert.
    let marker_ptr = 0xDE_AD_BE_EF_CA_FE_BA_B8u64;
    // SAFETY: the marker is only stored in a `Value` and read back as bytes; it is never dereferenced.
    let fake_ref = unsafe { ObjectRef::from_raw(marker_ptr as usize as *mut u8) };
    let val_obj = Value::Object(Some(fake_ref));
    let obj_bytes = value_to_bytes(val_obj);
    let marker_le = marker_ptr.to_le_bytes();
    (0..9)
        .find(|&i| obj_bytes[i..i + 8] == marker_le)
        .expect("cannot locate Object pointer in Value")
}

/// Probe the 16-byte representation of Value::Object(None) as two u64 halves.
/// Used by JIT inline aastore to write the correct discriminant and null pointer.
pub fn probe_object_null_template() -> (u64, u64) {
    let val = Value::Object(None);
    let bytes = value_to_bytes(val);
    let lo = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    (lo, hi)
}

// ---------------------------------------------------------------------------
// JIT error type — non-panicking compile/runtime failure signalling
// ---------------------------------------------------------------------------

/// Errors surfaced by the JIT instead of panicking.
///
/// The JIT contract is "never panics" — every internal codegen or runtime
/// invariant violation that previously triggered a `panic!`, `assert!`, or
/// `expect()` is being migrated to return one of these variants instead.
/// Most variants short-circuit the surrounding compile by tripping the
/// containing [`ExecutableBuffer`]'s sticky overflow flag, after which the
/// existing `if buf.overflowed() { return None; }` bail-outs cause
/// [`compile`](crate::x64::compile) to return `None` (interpreter fallback).
///
/// `try_call`/`try_call_with_context` propagate these directly without
/// going through `overflowed`, because they describe runtime invocation
/// failures rather than codegen ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A `try_patch_i32`/`try_patch_byte` call pointed past the emitted
    /// code, signalling either a codegen invariant break or a recovery
    /// from a prior buffer overflow. `kind` is `"i32"` or `"byte"`;
    /// `offset` is the patch site as recorded by the caller.
    PatchFailed { kind: &'static str, offset: usize },
    /// A rel8/rel32 displacement overflowed its encoding range during a
    /// branch patch. `kind` is `"rel8"` or `"rel32"`; `displacement` is
    /// the out-of-range value. Currently produced by the safe-idiv
    /// guard patches; the surrounding compile bails via `overflowed`.
    DisplacementOverflow {
        kind: &'static str,
        displacement: i64,
    },
    /// `CompiledMethod::try_call` / `try_call_with_context` was invoked
    /// with more arguments than the JIT's hand-rolled call thunks
    /// support (currently 8 / 7 respectively, excluding the implicit
    /// context pointer). `n` is the actual number of arguments
    /// supplied.
    TooManyArgs(usize),
    /// `validate_code_ptr` rejected an entry/trampoline pointer before
    /// transmute to a function pointer. The wrapped string is the
    /// `validate_code_ptr` reason.
    InvalidCodePtr(&'static str),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::PatchFailed { kind, offset } => write!(
                f,
                "JIT patch failed ({kind}) at offset {offset}: site is past emitted len"
            ),
            CompileError::DisplacementOverflow { kind, displacement } => write!(
                f,
                "JIT branch displacement overflowed {kind} encoding (= {displacement})"
            ),
            CompileError::TooManyArgs(n) => {
                write!(f, "JIT call: too many arguments ({n})")
            }
            CompileError::InvalidCodePtr(reason) => {
                write!(f, "JIT invalid code pointer: {reason}")
            }
        }
    }
}

impl std::error::Error for CompileError {}

// ---------------------------------------------------------------------------
// JIT code-cache cap (bounded growth)
// ---------------------------------------------------------------------------
//
// The cache owns executable bodies through `Arc<CompiledMethod>`. Baked direct
// calls, MIC/PIC entries, external dispatch caches, and lock-free readers all
// retain matching strong owners. Invalidation clears dynamic targets and
// transitively withdraws direct callers; the last owner unregisters and unmaps
// the body. The cap is therefore a live-occupancy pressure valve rather than a
// permanent cap-and-stop threshold.

/// Bytes of JIT code currently committed (mapped) by live `ExecutableBuffer`s.
///
/// Bumped in [`ExecutableBuffer::new`] and decremented when the last owner drops
/// and returns the region to the OS. This is the live quantity bounded by the
/// code-cache cap.
pub static COMMITTED_JIT_CODE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Default code-cache cap, in bytes (1 GiB).
///
/// It was 256 MiB, matched to HotSpot's ~240 MiB `ReservedCodeCacheSize`. That
/// comparison does not hold: HotSpot's sweeper evicts cold code, and nothing
/// here does, so reaching the cap is permanent (every method that goes hot
/// afterwards stays interpreted), and CratonVM bodies are larger. The
/// 206-class Hibernate Reactive suite in one process peaks at 493 MB of
/// committed code with every body trimmed (`ExecutableBuffer::trim_unused_tail`),
/// 30,448 bodies live at exit. Under 256 MiB it reached the cap at class 42,
/// and `MultithreadedInsertionWithLazyConnectionTest` then missed its
/// 600-second deadline (660 s); uncapped it passes in 288 s. 1 GiB is twice
/// that peak and still a finite bound on runaway recompilation. Only touched
/// pages cost memory, so the raise reserves nothing up front. See
/// `docs/feature-designs/jit-ir-optimizations.md`.
pub(crate) const DEFAULT_JIT_CODE_CACHE_CAP_BYTES: usize = 1024 * 1024 * 1024;

/// Hands out [`CompiledMethod::artifact_id`]. Monotonic for the life of the
/// process; `0` is reserved for "not recorded".
pub(crate) static ARTIFACT_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_artifact_id() -> u64 {
    ARTIFACT_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Current monotonically handed-out artifact counter. Monotonic for the life
/// of the process.
#[inline]
pub fn current_artifact_id() -> u64 {
    ARTIFACT_ID_COUNTER.load(std::sync::atomic::Ordering::Relaxed)
}

/// Number of `try_compile` calls refused because the code-cache cap was hit.
pub(crate) static JIT_CODE_CACHE_CAP_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Set once, the first time the cap is hit, so the warning is logged exactly once.
pub(crate) static JIT_CODE_CACHE_CAP_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Configured upper bound (in bytes) on total retained JIT code.
///
/// Overridable via `CRATONVM_JIT_CODE_CACHE_MAX_MB` (an integer number of
/// mebibytes); `0` disables the cap entirely (unbounded growth, the legacy
/// behaviour). An unparseable value falls back to the default. Cached on first
/// read so the env lookup happens at most once.
pub fn jit_code_cache_cap_bytes() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(s) => match s.trim().parse::<usize>() {
                // `0` is an explicit "disable the cap" sentinel (treated as
                // `usize::MAX` so the at-capacity check is always false).
                Ok(0) => usize::MAX,
                // Saturate the MiB→bytes multiply so a huge value can't wrap.
                Ok(mb) => mb.saturating_mul(1024 * 1024),
                Err(_) => DEFAULT_JIT_CODE_CACHE_CAP_BYTES,
            },
            Err(_) => DEFAULT_JIT_CODE_CACHE_CAP_BYTES,
        }
    })
}

/// Number of compilations refused so far because the code-cache cap was hit.
/// Diagnostic only.
///
/// NOT equal to the allocation ledger's `CAP_EXCEEDED` bucket
/// ([`jit_code_alloc_failure_stats`]), by design (r10 gauges item 6, written
/// down JIT round 22 lane sync): this counter also counts the refusals the
/// same gate arm makes for a latched executable-memory POLICY denial
/// (`jit_unavailable_by_policy`), which the ledger files under `OS_REFUSED`
/// instead. So this figure is the ledger's `CAP_EXCEEDED` plus those policy
/// refusals; see `note_jit_code_cache_cap_refusal`.
pub fn jit_code_cache_cap_refusals() -> u64 {
    JIT_CODE_CACHE_CAP_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one cap refusal. Called from [`compile_gate::admit`], which is the
/// single place the check now lives — the counter stays here so the accessor
/// above and the static it reads keep one owner.
pub(crate) fn note_jit_code_cache_cap_refusal() {
    JIT_CODE_CACHE_CAP_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // JIT round 22 wave 12 (lane review): the per-thread witness the tiered
    // worker reads for `Completion::cap_refused`
    // (`CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD`).
    crate::tiered::note_compile_cap_refused_on_this_thread();
    // Also the code-cache allocation ledger's `CAP_EXCEEDED` bucket. The two
    // counters are NOT redundant: this one is per-compile-door and is printed
    // by the JIT's own diagnostics, the ledger is the process-wide
    // failed-allocation gauge `vm::jit::code_cache_lifecycle` reports and
    // divides by (`refusal_rate` = failed / (installs + failed)). Before this
    // call the ledger's `CAP_EXCEEDED` bucket could never leave zero, so an
    // operator diagnosing code-cache exhaustion read "no failed allocations"
    // from the one report that is meant to answer exactly that question.
    //
    // Zero bytes, deliberately. The cap gate refuses BEFORE codegen runs, so
    // no buffer size was ever computed for this method — there is no
    // "requested bytes" to report and inventing the sizing heuristic's guess
    // would put a number in `failed_allocation_bytes` that no allocator ever
    // asked for. A zero-byte failure still increments `failed_allocations` and
    // the per-reason bucket, which are the figures this refusal is evidence
    // for; see the `failed_allocation_bytes` note on
    // [`JitCodeAllocFailureStats`].
    //
    // NOT unconditionally `CAP_EXCEEDED`. `jit_code_cache_at_capacity` answers
    // `true` for TWO conditions, and the second is not the cap: a latched
    // policy denial of executable memory (SELinux/PaX `execmem`, a hardened
    // runtime, Windows Arbitrary Code Guard) is "a capacity of zero, whatever
    // the cap", and it reaches this counter through the same gate arm. Filing
    // that under `code-cache-cap-exceeded` would send an operator to
    // `CRATONVM_JIT_CODE_CACHE_MAX_MB` for a machine that will refuse every
    // flip however large the cap is — which is exactly the misdiagnosis these
    // reason codes exist to prevent. One relaxed load of the latch tells the
    // two apart.
    let reason = if jit_unavailable_by_policy() {
        code_alloc_failure::OS_REFUSED
    } else {
        code_alloc_failure::CAP_EXCEEDED
    };
    note_jit_code_alloc_failure(0, reason);
}

// The counters themselves are FIELDS ON `RECLAMATION`, the existing
// code-cache accounting static, rather than a static of their own. That is not
// tidiness: `jit/tests/process_global_statics_ratchet.rs` freezes the number of
// `static` declarations under `jit/src` at a baseline and panics on any
// increase, on the grounds that a static in `cratonvm-jit` is shared by every
// VM in the process. A new ledger static would have broken that gate and the
// fix would have had to touch the ratchet file. Living on `RECLAMATION` costs
// nothing — these ARE code-cache accounting, they are process-wide for the
// same reason the install counts are (one code arena, one OS, one cap), and
// `ReclamationCounters` is private so widening it changes no public shape.

/// Record one refused code-cache allocation.
///
/// `requested_bytes` is what the refused allocation asked the allocator for,
/// or `0` where the refusal happened before any size was computed (the cap
/// gate). `reason` is a [`code_alloc_failure`] code; an out-of-range code
/// still counts in the totals and is dropped from the per-reason histogram
/// rather than panicking — an observability call must never be able to abort a
/// compile thread, where a panic is a permanent decline for the method.
pub(crate) fn note_jit_code_alloc_failure(requested_bytes: usize, reason: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION;
    c.alloc_failures.fetch_add(1, Relaxed);
    c.alloc_failure_bytes
        .fetch_add(requested_bytes as u64, Relaxed);
    if let Some(slot) = c.alloc_failures_by_reason.get(reason) {
        slot.fetch_add(1, Relaxed);
    }
}

/// Refused code-cache allocations, as one snapshot.
///
/// Read one relaxed counter at a time, so a snapshot taken during a refusal
/// can be off by that event. Monotonic: nothing ever decrements these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitCodeAllocFailureStats {
    /// Refused code-cache allocations, all reasons.
    pub failures: u64,
    /// Sum of the bytes those refusals asked for.
    ///
    /// A LOWER BOUND on the memory the process could not get, not a total:
    /// cap-gate refusals contribute zero because they happen before codegen
    /// computes a size (see `note_jit_code_cache_cap_refusal`). Compare it
    /// against `failures` rather than treating it as a mean.
    pub failed_bytes: u64,
    /// Refusals per [`code_alloc_failure`] code.
    pub by_reason: [u64; code_alloc_failure::COUNT],
}

/// Snapshot the refused-allocation ledger.
///
/// The pull side of the inversion described above this function's counters:
/// `vm::jit::code_cache_lifecycle::code_cache_lifecycle_raw` calls this and
/// copies the three figures into the report's `failed_allocations`,
/// `failed_allocation_bytes` and `failed_allocations_by_reason`.
pub fn jit_code_alloc_failure_stats() -> JitCodeAllocFailureStats {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION;
    let mut by_reason = [0u64; code_alloc_failure::COUNT];
    for (slot, out) in c.alloc_failures_by_reason.iter().zip(by_reason.iter_mut()) {
        *out = slot.load(Relaxed);
    }
    JitCodeAllocFailureStats {
        failures: c.alloc_failures.load(Relaxed),
        failed_bytes: c.alloc_failure_bytes.load(Relaxed),
        by_reason,
    }
}

/// Whether retained JIT code has reached the cap, as a pure observation.
///
/// [`jit_code_cache_at_capacity`] is the same comparison with side effects —
/// it drains the retirement queue and prints the one-time warning — and it is
/// the compile gate's, taken on the way to refusing a compile. This one is for
/// a caller that wants to ACT on the pressure instead of being refused by it,
/// which today is the per-VM cold-body sweep driver in
/// `vm/src/runtime/interpreter/jit_bridge.rs`. Keeping them apart matters: a
/// sweep driver calling the gate's version would drain and warn from a code
/// path that is not refusing anything, and the "reached the cap" warning would
/// then be printed by a thread that went on to free room.
///
/// One relaxed load, or none when the cap is disabled.
#[must_use]
pub fn jit_code_cache_under_pressure() -> bool {
    let cap = jit_code_cache_cap_bytes();
    cap != usize::MAX && COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed) >= cap
}

/// Returns `true` if retained JIT code has reached the configured cap, meaning
/// new compilation should be refused (the method stays in the interpreter).
///
/// We compare against `COMMITTED_JIT_CODE_BYTES`, which `ExecutableBuffer::new`
/// bumps for every committed allocation (method bodies, OSR trampolines, deopt
/// stubs) — the same quantity the cap is meant to bound. Logs a one-time warning
/// the first time the cap is reached.
///
/// **No sweep happens here**, and that is a scoping fact rather than an
/// omission. Eviction is per-VM: it needs a [`JitCache`], the bodies in it and
/// its own usage signal, and this function is a process-wide free function that
/// has none of them — there is no registry of live caches in this crate, and
/// adding one would be the process global for per-VM state that `AGENTS.md`
/// forbids. The sweep is [`JitCache::sweep_cold_bodies`], driven from the VM
/// side off [`jit_code_cache_under_pressure`].
pub(crate) fn jit_code_cache_at_capacity() -> bool {
    // A policy denial of executable memory is a capacity of zero, whatever the
    // cap. Answered first, so it holds with the cap disabled too, and before
    // the cap's one-time warning (the latch printed its own).
    if jit_unavailable_by_policy() {
        return true;
    }
    let cap = jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        return false; // cap disabled
    }
    let mut used = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    if used < cap {
        return false;
    }
    // Retired bodies waiting in the retirement queue are committed bytes too.
    // Try to release them before refusing a compile on their account: a queue
    // that drained only at process-wide quiescence used to pin the cache at its
    // cap for as long as one thread stayed parked inside compiled code.
    if jit_retirement_queue_len() != 0 {
        drain_deferred_jit_owners();
        used = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        if used < cap {
            return false;
        }
    }
    // At/over the cap: warn exactly once, then keep refusing silently.
    //
    // The wording is conditional on the sweeper because the operator's next
    // move differs between the two states, and the old single sentence — "new
    // methods will stay in the interpreter. Raise the limit …" — left out the
    // part that matters most when nothing evicts: the condition does not clear.
    // Nothing shrinks `COMMITTED_JIT_CODE_BYTES` except a body being released,
    // and with no sweeper the only releases are invalidation-driven, so a
    // process that reaches this line without one is refusing every future
    // compile for the rest of its life. That is a permanent performance cliff
    // reported as a transient-sounding notice.
    if !JIT_CODE_CACHE_CAP_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        if code_cache_sweep_enabled() {
            eprintln!(
                "[cratonvm-jit] code-cache cap reached: {used} bytes retained >= cap {cap} \
                 bytes; new methods stay in the interpreter until the cold-body sweeper \
                 frees room (CRATONVM_JIT_CODE_CACHE_SWEEP is on). \
                 Raise the limit with CRATONVM_JIT_CODE_CACHE_MAX_MB (0 disables it)."
            );
        } else {
            eprintln!(
                "[cratonvm-jit] code-cache cap reached: {used} bytes retained >= cap {cap} \
                 bytes; new methods will stay in the interpreter. Nothing evicts cold code, \
                 so this is PERMANENT for the life of this process — every method that goes \
                 hot from now on stays interpreted. \
                 Raise the limit with CRATONVM_JIT_CODE_CACHE_MAX_MB (0 disables it)."
            );
        }
    }
    true
}

/// Ring of the most recently unmapped executable buffers.
///
/// Exists so the crash handler can answer the one question a bare
/// `SIGSEGV at pc=X, addr=X` cannot: was `X` inside a code buffer this process
/// had just released? `lookup_jit_method_name` cannot — it answers from the
/// range registry, which a *recycled* address also satisfies, and which is
/// still populated when the fault is taken. Plain atomics, no locks and no
/// allocation, so the lookup is safe from a signal handler.
pub(crate) const RECENT_CODE_FREES: usize = 4096;
pub(crate) static RECENT_FREE_BASE: [std::sync::atomic::AtomicUsize; RECENT_CODE_FREES] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; RECENT_CODE_FREES];
pub(crate) static RECENT_FREE_LEN: [std::sync::atomic::AtomicUsize; RECENT_CODE_FREES] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; RECENT_CODE_FREES];
pub(crate) static RECENT_FREE_ACTIVE: [std::sync::atomic::AtomicUsize; RECENT_CODE_FREES] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; RECENT_CODE_FREES];
/// Bit 0: the buffer belonged to a *published* body, i.e. one a dispatch
/// surface could hand a raw pointer to. Bit 1: the release was authorised by
/// the retirement queue's quiescence proof.
///
/// Both are needed to read `RECENT_FREE_ACTIVE` correctly, and neither used to
/// be recorded. `active_jit_executions` alone cannot distinguish a genuine
/// bypass from (a) a discarded compile attempt, which nothing should point into
/// (round 13 wave 8 found one thing that did: a caller baking the entry of a
/// callee whose `put` was refused, `r13w8-codecache-baked-entry-of-a-refused-callee-FIXED-20260928.md`),
/// or (b) a perfectly legal reclamation whose `munmap` merely happened after
/// some *other* thread entered compiled code — the count is sampled at the
/// unmap, not at the decision. A crash report that names only the count
/// therefore reads as damning when it is not; see
/// `jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`.
pub(crate) static RECENT_FREE_FLAGS: [std::sync::atomic::AtomicUsize; RECENT_CODE_FREES] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; RECENT_CODE_FREES];

/// `RECENT_FREE_FLAGS` bit: the buffer belonged to a published body.
pub const CODE_FREE_PUBLISHED: usize = 1;
/// `RECENT_FREE_FLAGS` bit: the retirement queue authorised the release.
pub const CODE_FREE_AUTHORISED: usize = 2;
/// `RECENT_FREE_FLAGS` bit: the body was ENTERED without being published — an
/// accepted optimizing OSR body or a lambda-adapter thunk
/// (`ExecutableBuffer::note_entered_unpublished`). Such a body is NOT "a
/// discarded compile attempt" even though [`CODE_FREE_PUBLISHED`] is clear:
/// generated code did run it (round 13 wave 8, lane codecache).
pub const CODE_FREE_ENTERED: usize = 4;
pub(crate) static RECENT_FREE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    /// AUDIT: `(depth, scope)`. `depth` is non-zero while this thread is inside
    /// a reclamation the retirement queue authorised (`defer_jit_owner`'s
    /// quiescent fast path, or the drain). `scope` is the buffer start the
    /// innermost authorisation's proof covers, or [`RECLAIM_SCOPE_ANY`].
    ///
    /// Nested drops inherit the depth. Under a process-wide proof (scope ANY)
    /// that is correct: every thread was outside compiled code, so a callee
    /// root or inline-cache owner the reclaimed body held, dropped last here,
    /// is unreachable too. Under per-thread evidence it is NOT: the evidence
    /// was checked against ONE buffer, and a blocked thread's stack may name
    /// the cascaded body. Round 14 wave 1 (lane codecache, `gcd-d10v`): the
    /// drain's evidence arm scopes each release to its own buffer, and
    /// `ExecutableBuffer::drop` sends a published buffer outside the scope back
    /// through the queue ([`reclaim_is_cascaded`]).
    pub(crate) static AUTHORISED_RECLAIM_DEPTH: std::cell::Cell<(u32, usize)> =
        const { std::cell::Cell::new((0, RECLAIM_SCOPE_ANY)) };
}

/// [`AUTHORISED_RECLAIM_DEPTH`]'s scope for a proof that covers every buffer
/// (the process-wide quiescence proof). Never a buffer start.
pub(crate) const RECLAIM_SCOPE_ANY: usize = 0;

/// An authorised reclamation on this thread. Restores the enclosing scope on
/// drop, so a nested unscoped reclamation (the queue's fast path reached from a
/// cascade) neither inherits nor clobbers a scoped one.
pub(crate) struct AuthorisedReclaim {
    pub(crate) prev_scope: usize,
}

impl AuthorisedReclaim {
    /// An authorisation backed by the process-wide proof: covers every buffer.
    pub(crate) fn enter() -> Self {
        Self::enter_scoped(RECLAIM_SCOPE_ANY)
    }

    /// An authorisation backed by per-thread evidence about the buffer at
    /// `buffer_start` only (round 14 wave 1, lane codecache).
    pub(crate) fn enter_for(buffer_start: usize) -> Self {
        Self::enter_scoped(buffer_start)
    }

    pub(crate) fn enter_scoped(scope: usize) -> Self {
        let prev_scope = AUTHORISED_RECLAIM_DEPTH.with(|d| {
            let (depth, prev) = d.get();
            d.set((depth.saturating_add(1), scope));
            prev
        });
        AuthorisedReclaim { prev_scope }
    }
}

impl Drop for AuthorisedReclaim {
    fn drop(&mut self) {
        let prev_scope = self.prev_scope;
        AUTHORISED_RECLAIM_DEPTH.with(|d| {
            let (depth, _) = d.get();
            d.set((depth.saturating_sub(1), prev_scope));
        });
    }
}

pub(crate) fn reclaim_is_authorised() -> bool {
    AUTHORISED_RECLAIM_DEPTH
        .try_with(|d| d.get().0 != 0)
        .unwrap_or(false)
}

/// Whether the buffer at `buffer_start` is being released as a SIDE EFFECT of
/// an authorised reclamation whose proof covered a different buffer (a
/// `CompiledMethod` freed on per-thread evidence dropping the last owner of a
/// callee or inline-cache target). Such a release must earn its own proof:
/// `ExecutableBuffer::drop` queues it (round 14 wave 1, lane codecache,
/// `gcd-d10v-compiled-code-jumps-into-a-retired-code-buffer-under-drop-orphans-CLOSED-20260929.md`).
pub(crate) fn reclaim_is_cascaded(buffer_start: usize) -> bool {
    AUTHORISED_RECLAIM_DEPTH
        .try_with(|d| {
            let (depth, scope) = d.get();
            depth != 0 && scope != RECLAIM_SCOPE_ANY && scope != buffer_start
        })
        .unwrap_or(false)
}


/// AUDIT: published bodies whose mapping was released.
pub(crate) static PUBLISHED_CODE_FREES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// AUDIT: published bodies whose last owner was dropped by a path the
/// retirement queue never saw.
///
/// Each of these used to be a potential use-after-free of executable memory:
/// nothing on that path asked whether a thread was inside the body. Since
/// 2026-09-18 `ExecutableBuffer::drop` RESCUES such a mapping into the queue
/// ([`defer_orphaned_mapping`]) instead of unmapping it, so a non-zero count is
/// no longer a crash waiting to happen — but it still names a holder that
/// should have released through the queue, and it is still meant to be zero.
pub(crate) static UNQUEUED_PUBLISHED_CODE_FREES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Process-wide, append-only intern table for the class names that
/// `checkcast` / `instanceof` sites hand to `jit_checkcast` / `jit_instanceof`
/// as a raw `(ptr, len)` pair.
///
/// # Why these names cannot live on the compiled method
///
/// The type-check helpers in `vm/src/jit/helpers.rs` memoize on that pair:
///
/// * `JIT_TYPECHECK_TARGET_CACHE` — `(vm, ptr, len) -> resolved target ClassId`
/// * `JIT_TYPECHECK_ANSWER_CACHE` — `(vm, ptr, len, receiver ClassId, lenient)
///   -> the check passed`
///
/// Both are *thread-locals*, so they outlive any one compiled method. The
/// names used to be `Box<str>`s owned by `CompiledMethod::_jit_strings`, freed
/// when that method is dropped — tier-up, invalidation, code-cache eviction.
/// The allocator then hands the same block to the next compilation, so a
/// `(ptr, len)` key silently starts naming a DIFFERENT class of the same
/// length, and both caches answer for the old one.
///
/// That is not theoretical: modelling this allocation pattern under `mimalloc`
/// (this VM's allocator) over 8000 name allocations produced 3568 keys that
/// later meant a different class name.
///
/// The reachable failure is a `String` passing `instanceof java/lang/Number`.
/// `java/lang/String` and `java/lang/Number` are both 16 bytes; a warm
/// `x instanceof String` site leaves `(vm, P, 16, String, lenient=false) ->
/// true` behind, and once `P` is recycled for a `java/lang/Number` site the
/// strict `instanceof` answers `true` while the paired `checkcast` — a
/// different site pointer, and `lenient = true`, so a different key — resolves
/// honestly and throws. In `scala.runtime.BoxesRunTime.equals2` those two
/// bytecodes are seven apart:
///
/// ```text
///   1: instanceof java/lang/Number
///   4: ifeq   16
///   8: checkcast java/lang/Number
/// ```
///
/// which is the whole of
/// `ClassCastException: class java.lang.String cannot be cast to class
/// java.lang.Number` inside `Seq.distinct`, seen in Kafka's
/// `CoreUtils.listenerListToEndPoints`.
///
/// Interning by CONTENT makes the pair a permanent, unique identity for one
/// class name, which is what both caches always assumed. It also makes them
/// strictly more effective: two `checkcast`s to the same class — in one method
/// or in two — now share a cache entry instead of evicting each other.
///
/// Entries are never freed. The table is bounded by the number of DISTINCT
/// class names that appear at a type-check site in the program, not by the
/// number of compilations.
/// # Interning by (name, resolved target), not by name
///
/// A type-check site carries only a class NAME to the runtime helper, and the
/// class dictionary is keyed by `(ClassLoaderId, name)` — so a name alone does
/// not name a class. When two loaders each define `Foo`, resolving the site's
/// name at run time picks a winner that may not be the copy the compiling
/// method's own constant pool meant, and the helper then had to fall back to a
/// loader-blind name walk to avoid refusing a cast the interpreter would pass.
///
/// The compiler already knows the answer: `cp_new_resolver` resolves the site's
/// `CONSTANT_Class` entry through the compiling class's loader and hands back
/// the exact `ClassId`. Interning by `(name, that id)` gives each resolved
/// target its own pointer and records the id in [`TYPECHECK_TARGET_BY_SITE`],
/// so the helper can compare identities instead of spelling. `None` — a site
/// whose target was not loaded at compile time — keeps the old name-only
/// identity and the old behaviour.
///
/// # Round 9 wave 8 (`arraylist8`): the key also carries finality
///
/// The key is `(resolved target, target is a final class)`. A site whose
/// compile door PROVED its target a `final`, non-interface class
/// ([`intern_typecheck_target_with_finality`]) gets a pointer of its own, and
/// [`typecheck_target_is_final_class`] answers for exactly that pointer. Keying
/// on the verdict rather than recording it beside a shared pointer is what keeps
/// it per-VM-correct: `ClassId`s are per VM and this table is process-wide, so
/// two VMs can intern the same `(name, id)` for a final class in one and a
/// non-final one in the other. Each gets the pointer its OWN verdict names, and
/// a door that knows nothing about finality (every caller of
/// [`intern_typecheck_target`]) can never be handed a pointer another door
/// declared final.
///
/// # A site nobody could resolve at compile time carries its HOLDER
///
/// A `None` target used to mean one shared name-only pointer for every such
/// site in the process, and the helper then resolved the bare name with no
/// loader at all. That is the loader-blind answer the recorded id exists to
/// avoid, and since 2026-09-23 it is also the COMMON shape for a class a
/// user-defined loader defined: the compiler no longer guesses a target
/// that loader has not initiated (`class_resolved_without_loading` in the
/// VM). [`intern_typecheck_unresolved_in`] keys such a site by the class
/// whose constant pool names it, so the helper can resolve it at run time
/// exactly as the interpreter would -- through that class's own loader.
pub(crate) static TYPECHECK_NAME_INTERN: std::sync::OnceLock<
    parking_lot::Mutex<
        rustc_hash::FxHashMap<TypecheckInternKey, rustc_hash::FxHashSet<&'static str>>,
    >,
> = std::sync::OnceLock::new();

/// The identity a type-check site's interned name pointer stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum TypecheckInternKey {
    /// `(resolved target, the door proved it a final class)`; `(None, false)`
    /// is the legacy name-only identity.
    Target(Option<u32>, bool),
    /// Unresolved at compile time; resolve through this class's loader.
    /// `(holder ClassId, cp index of the site's CONSTANT_Class)`.
    UnresolvedIn(u32, u16),
}

/// What [`TYPECHECK_TARGET_BY_SITE`] records for one interned pointer.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TypecheckSiteIdentity {
    /// `(ClassId, the interning door proved it a final non-interface class)`.
    Target(u32, bool),
    /// `(the ClassId of the class whose constant pool names this site, the
    /// index of that CONSTANT_Class entry)`. The index is what JVMS §5.4.3
    /// keys a recorded resolution failure by (interpreter round i1 wave 12,
    /// L2): the run-time helper consults and records it exactly as the
    /// interpreter's `checkcast` / `instanceof` do.
    ///
    /// The holder is a [`cp_holder_word`] (interpreter round i1 wave 21,
    /// lane L1): the constant-pool stamp of the compile that interned the
    /// pointer rides in its high half, so a site of code compiled before
    /// its holder was redefined keys the record by its translated index
    /// ([`typecheck_holder_word_for_site`]). A later compile that interns
    /// the same `(name, holder, index)` reuses the pointer and its stamp:
    /// its code named the same class at that index, which a redefinition's
    /// translation leaves in place.
    UnresolvedIn(u64, u16),
}

/// `interned name pointer -> the `ClassId` that site's target resolved to at
/// compile time`. Read by `vm::jit::helpers::jit_typecheck_resolve`.
///
/// Entries are never removed, for the same reason the intern table's are not:
/// the pointer is leaked for the life of the process, so a row can never come
/// to describe a different site.
///
/// A `ClassId` is per-VM, and this table is process-wide, so the reader must
/// confirm the recorded id still names the site's class in *its* VM before
/// trusting it — see the check in `jit_typecheck_resolve`.
///
/// The value is `(resolved ClassId, the interning door proved it a final
/// non-interface class)`; see the finality note on [`TYPECHECK_NAME_INTERN`].
pub(crate) static TYPECHECK_TARGET_BY_SITE: std::sync::OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<usize, TypecheckSiteIdentity>>,
> = std::sync::OnceLock::new();

pub(crate) fn typecheck_target_table(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<usize, TypecheckSiteIdentity>> {
    TYPECHECK_TARGET_BY_SITE.get_or_init(|| parking_lot::RwLock::new(Default::default()))
}

/// Intern `name` and return the stable `(ptr, len)` pair for it. Repeated calls
/// with equal contents return the identical pointer, for the life of the
/// process.
///
/// Equivalent to [`intern_typecheck_target`] with no resolved target: the site
/// answers "I could not resolve this name at compile time".
pub fn intern_typecheck_class_name(name: &str) -> (*const u8, usize) {
    intern_typecheck_target(name, None)
}

/// Intern `name` under the identity of the `ClassId` its `CONSTANT_Class` entry
/// resolved to through the compiling class's own loader.
///
/// Two loaders' same-named copies get two distinct pointers, so the per-thread
/// memos keyed on that pointer stay exact, and the runtime helper recovers the
/// resolved id with [`typecheck_target_for_site`].
pub fn intern_typecheck_target(name: &str, target_class_id: Option<u32>) -> (*const u8, usize) {
    intern_typecheck_target_with_finality(name, target_class_id, false)
}

/// [`intern_typecheck_target`], for a door that also knows whether the
/// resolved target is a `final`, non-interface class (round 9 wave 8,
/// `arraylist8`; `NOTES-w5-typecheck5.md` cross-lane request 2).
///
/// `target_is_final_class` is honoured only with a resolved id: a name the door
/// could not resolve has no class to be final. A `true` gets its own pointer,
/// for which [`typecheck_target_is_final_class`] answers `true`; that is what
/// lets the inline `instanceof` answer a definite MISS for a user class the way
/// it already does for the final boot classes (`op_object.rs`,
/// `final_miss_target`). The door's claim must be the class's own
/// `ACC_FINAL && !ACC_INTERFACE`, read from the class the site resolved to.
pub fn intern_typecheck_target_with_finality(
    name: &str,
    target_class_id: Option<u32>,
    target_is_final_class: bool,
) -> (*const u8, usize) {
    let is_final = target_is_final_class && target_class_id.is_some();
    intern_typecheck_site(
        name,
        TypecheckInternKey::Target(target_class_id, is_final),
        target_class_id.map(|id| TypecheckSiteIdentity::Target(id, is_final)),
    )
}

/// Intern a type-check site whose target the compiling door could NOT
/// resolve, under the identity of `holder_class_id` -- the class whose
/// constant pool names it -- and `cp_index`, the index of that
/// `CONSTANT_Class` entry. The run-time helper recovers both with
/// [`typecheck_holder_for_site`], resolves the name through the holder's own
/// loader, and keys a JVMS §5.4.3 resolution failure by the pair. See the
/// holder note on [`TYPECHECK_NAME_INTERN`].
pub fn intern_typecheck_unresolved_in(
    name: &str,
    holder_class_id: u32,
    cp_index: u16,
) -> (*const u8, usize) {
    intern_typecheck_site(
        name,
        TypecheckInternKey::UnresolvedIn(holder_class_id, cp_index),
        Some(TypecheckSiteIdentity::UnresolvedIn(
            cp_holder_word(holder_class_id),
            cp_index,
        )),
    )
}

/// Intern the site pointer for a [`JitNewSite`] answer about the same
/// `CONSTANT_Class` entry: a resolved class gets its id (and `is_final`'s
/// verdict on it), a deferred one its holder, and no answer at all the legacy
/// name-only identity. The one mapping every JIT-crate door shares.
pub fn intern_typecheck_for_new_site(
    name: &str,
    site: Option<JitNewSite>,
    is_final: Option<&dyn Fn(u32) -> bool>,
) -> (*const u8, usize) {
    match site {
        Some(JitNewSite::Resolved { class_id, .. }) => intern_typecheck_target_with_finality(
            name,
            Some(class_id),
            is_final.is_some_and(|f| f(class_id)),
        ),
        Some(JitNewSite::Deferred {
            holder_class_id,
            cp_idx,
        }) => intern_typecheck_unresolved_in(name, holder_class_id, cp_idx),
        None => intern_typecheck_target(name, None),
    }
}

pub(crate) fn intern_typecheck_site(
    name: &str,
    key: TypecheckInternKey,
    identity: Option<TypecheckSiteIdentity>,
) -> (*const u8, usize) {
    let table = TYPECHECK_NAME_INTERN.get_or_init(|| parking_lot::Mutex::new(Default::default()));
    let mut table = table.lock();
    let names = table.entry(key).or_default();
    let interned: &'static str = match names.get(name) {
        Some(existing) => existing,
        None => {
            let leaked: &'static str = Box::leak(String::from(name).into_boxed_str());
            names.insert(leaked);
            if let Some(identity) = identity {
                typecheck_target_table()
                    .write()
                    .insert(leaked.as_ptr() as usize, identity);
            }
            leaked
        }
    };
    (interned.as_ptr(), interned.len())
}

/// `CRATONVM_JIT_TYPECHECK_MISS_MEMO` (JIT r23 w2c close; perf, **default
/// ON**, `=0` kill): an optimizing-tier `instanceof` remembers, per site and
/// per VM, the last two receiver classes it proved a MISS for inline, and
/// answers `0` for them with two compares instead of the receiver's display
/// walk (three dependent loads and the verdict read). See
/// [`typecheck_miss_memo_cell`]. `W2cTypecheckLoops` gate / shapes / iface:
/// the walk was most of each element's miss path. Read per site.
pub(crate) fn typecheck_miss_memo_enabled() -> bool {
    true
}

/// The memo of an `instanceof` against `target` in this compile's VM: the
/// address of a `[AtomicU32; 4]` the VM owns (`SupersDisplays::miss_memo_addr`
/// in `vm/src/vm/realms/jit_realm.rs`, kept for the VM's life beside the
/// displays the memo's facts come from), asked through the well-known resolver
/// ([`ir_lower::WELL_KNOWN_TYPECHECK_MISS_MEMO_TAG`]); every way starts at
/// `target` -- a class id no receiver that reaches the memo can have (the
/// exact compare answered it). Shared by every compile of that target in that
/// VM, so recompiles allocate nothing. `None` when the VM keeps none (no
/// resolver wired, displays off).
///
/// Ways 0 and 1 hold MISSES: a receiver class `X` for which the inline
/// sequence PROVED `X instanceof target == false` -- the receiver's published
/// display lists no `target` (or `target` is final and is not `X`), the
/// display's verdict is hierarchy-only, and the duplicate-class-name latch was
/// down. Ways 2 and 3 hold HITS: a class whose published display lists
/// `target` among its supers or interfaces. None of those facts changes for
/// the class afterwards (a display is published once from fixed edges; class
/// ids are never reused within a VM: `ClassStore::next_id` is the store's
/// length), except the latch, which a miss way's reader checks first (a hit
/// does not depend on it, as the display's own hit does not). Two threads
/// racing on a write each store a proven class, so either value is sound.
///
/// JIT r25 wave 8 (W8-2): the memos lived in a process `static` keyed by
/// `(interned target name, display cell address)`. That was per-VM state in
/// a process global (AGENTS.md), and unsound across VM lifetimes: the display
/// cell is freed with its VM, so a later VM whose cell reused the address
/// inherited the dead VM's proven classes under colliding class ids. Keyed by
/// the class id inside the VM that proved them, the facts die with it.
pub(crate) fn typecheck_miss_memo_cell(helpers: &DirectHelperTable, target: u32) -> Option<usize> {
    let answer = ir_lower::vh_const_ask(
        helpers,
        ir_lower::vh_const_query(ir_lower::WELL_KNOWN_TYPECHECK_MISS_MEMO_TAG, target, 0),
    );
    usize::try_from(answer)
        .ok()
        .filter(|&a| a != 0 && a % std::mem::align_of::<std::sync::atomic::AtomicU32>() == 0)
}


/// Whether the site whose class name lives at `name_ptr` was interned by a
/// door that proved its target a `final`, non-interface class
/// ([`intern_typecheck_target_with_finality`]). `false` for every other
/// pointer, including every pointer [`intern_typecheck_target`] hands out.
#[inline]
pub fn typecheck_target_is_final_class(name_ptr: *const u8) -> bool {
    TYPECHECK_TARGET_BY_SITE
        .get()
        .and_then(|table| match table.read().get(&(name_ptr as usize)) {
            Some(&TypecheckSiteIdentity::Target(_, is_final)) => Some(is_final),
            _ => None,
        })
        .unwrap_or(false)
}

/// Process-wide intern table for compiled local handlers' catch-type names.
///
/// A `JitLocalHandlerSite` holds `&'static str` catch types and outlives no
/// particular compile, so the bytes cannot be owned by the artifact's
/// `_jit_strings` (which is freed on tier-up while a *newer* artifact for the
/// same method may still name the same catch type). A leaked intern is what
/// every other by-name JIT site does — see [`intern_typecheck_target`] — and
/// the population is bounded by the distinct catch types in the program.
pub(crate) static CATCH_TYPE_NAME_INTERN: std::sync::OnceLock<
    parking_lot::Mutex<rustc_hash::FxHashSet<&'static str>>,
> = std::sync::OnceLock::new();

fn intern_static_str(
    cell: &std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashSet<&'static str>>>,
    s: &str,
) -> &'static str {
    let table = cell.get_or_init(|| parking_lot::Mutex::new(Default::default()));
    let mut table = table.lock();
    match table.get(s) {
        Some(existing) => existing,
        None => {
            let leaked: &'static str = Box::leak(String::from(s).into_boxed_str());
            table.insert(leaked);
            leaked
        }
    }
}

/// Intern a catch-type name for a [`JitLocalHandlerSite`].
pub fn intern_catch_type_name(name: &str) -> &'static str {
    intern_static_str(&CATCH_TYPE_NAME_INTERN, name)
}

pub(crate) static JIT_INVOKE_STR_INTERN: std::sync::OnceLock<
    parking_lot::Mutex<rustc_hash::FxHashSet<&'static str>>,
> = std::sync::OnceLock::new();

/// Intern a class/method/descriptor string for [`JitInvokeInfo`].
///
/// Invoke-site metadata pointers may outlive the `CompiledMethod` that
/// originally allocated them (for example across tier-up or OSR transitions),
/// so they must have process-wide `'static` storage.
pub fn intern_jit_invoke_str(s: &str) -> &'static str {
    intern_static_str(&JIT_INVOKE_STR_INTERN, s)
}

/// The `ClassId` the site whose class name lives at `name_ptr` resolved to at
/// compile time, if it resolved at all.
#[inline]
pub fn typecheck_target_for_site(name_ptr: *const u8) -> Option<u32> {
    let table = TYPECHECK_TARGET_BY_SITE.get()?;
    match table.read().get(&(name_ptr as usize)) {
        Some(&TypecheckSiteIdentity::Target(id, _)) => Some(id),
        _ => None,
    }
}

/// `(the class whose constant pool names the site at `name_ptr`, the index of
/// its CONSTANT_Class entry)`, for a site interned by
/// [`intern_typecheck_unresolved_in`] and not yet resolved.
#[inline]
pub fn typecheck_holder_for_site(name_ptr: *const u8) -> Option<(u32, u16)> {
    typecheck_holder_word_for_site(name_ptr)
        .map(|(word, cp_index)| (cp_holder_word_parts(word).0, cp_index))
}

/// [`typecheck_holder_for_site`] with the holder as the [`cp_holder_word`]
/// the pointer was interned with: the constant-pool stamp of the code that
/// interned it rides in the high half, for the run-time helper's index
/// translation (`vm::jit::helpers::cp_site_index`; interpreter round i1 wave
/// 21, lane L1, `interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`).
#[inline]
pub fn typecheck_holder_word_for_site(name_ptr: *const u8) -> Option<(u64, u16)> {
    let table = TYPECHECK_TARGET_BY_SITE.get()?;
    match table.read().get(&(name_ptr as usize)) {
        Some(&TypecheckSiteIdentity::UnresolvedIn(holder_word, cp_index)) => {
            Some((holder_word, cp_index))
        }
        _ => None,
    }
}

/// Record the `ClassId` a holder-keyed site (see
/// [`intern_typecheck_unresolved_in`]) resolved to at run time, so later
/// executions take the recorded-target path. A resolved constant-pool entry
/// never changes (JVMS §5.4.3), so this is written at most once per site.
/// Declines for any pointer not interned as an unresolved site.
pub fn record_typecheck_target_for_site(name_ptr: *const u8, class_id: u32) {
    let mut table = typecheck_target_table().write();
    if let Some(entry) = table.get_mut(&(name_ptr as usize)) {
        if matches!(entry, TypecheckSiteIdentity::UnresolvedIn(..)) {
            *entry = TypecheckSiteIdentity::Target(class_id, false);
        }
    }
}

/// RW->RX flips the OS refused at the end of an otherwise successful compile
/// (`CompiledMethod::try_new*`). Each one is a method that stayed interpreted
/// instead of a process abort. Monotone; diagnostic.
pub(crate) static CODE_BUFFER_PROTECT_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The "JIT unavailable" latch, folded into the HIGH bit of
/// [`CODE_BUFFER_PROTECT_FAILURES`] rather than kept in a static of its own
/// (`jit/tests/process_global_statics_ratchet.rs`). The count cannot reach it:
/// that would take 2^63 refused flips.
///
/// Set once, never cleared, when a refused flip was a process-wide POLICY
/// denial ([`platform::JitError::is_policy_denial`]: SELinux `execmem`, a
/// hardened runtime, Windows ACG). Such a process will refuse every later flip
/// too, so every compile after the first would be generated in full and then
/// thrown away. Process-wide is the right scope here, unlike the per-VM state
/// the ratchet guards: executable-memory policy belongs to the OS process, not
/// to a VM inside it.
pub(crate) const JIT_UNAVAILABLE_BY_POLICY_BIT: u64 = 1 << 63;

/// See [`CODE_BUFFER_PROTECT_FAILURES`].
pub fn code_buffer_protect_failures() -> u64 {
    CODE_BUFFER_PROTECT_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
        & !JIT_UNAVAILABLE_BY_POLICY_BIT
}

/// Whether the OS refused an RW->RX flip by POLICY, which switches every later
/// compile off for the life of the process (see
/// `JIT_UNAVAILABLE_BY_POLICY_BIT`). Consulted by
/// `jit_code_cache_at_capacity`, so both the tiered manager's queueing gate
/// and `compile_gate::admit` decline before any code is generated: a policy
/// denial is an executable-code capacity of zero.
pub fn jit_unavailable_by_policy() -> bool {
    CODE_BUFFER_PROTECT_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
        & JIT_UNAVAILABLE_BY_POLICY_BIT
        != 0
}

/// Whether a refused flip latches [`jit_unavailable_by_policy`]. Never in a
/// unit-test build: the fault-injection tests arm `EACCES` on purpose inside
/// a test process shared with every other compile test, and latching there
/// would refuse all of them.
pub(crate) fn protect_failure_latches_jit_unavailable(e: &platform::JitError) -> bool {
    !cfg!(test) && e.is_policy_denial()
}

/// Count a refused RW->RX flip and report the FIRST one on stderr (a
/// `tracing` event can be filtered away, and a compile tier that silently
/// stops producing code is the failure this must not become). A policy denial
/// also latches [`jit_unavailable_by_policy`], announced once on stderr.
pub(crate) fn note_code_buffer_protect_failure(e: &platform::JitError) {
    let n = CODE_BUFFER_PROTECT_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        & !JIT_UNAVAILABLE_BY_POLICY_BIT;
    if n == 0 {
        eprintln!(
            "[cratonvm-jit] WARNING: making a compiled body executable failed ({e}); \
             the method stays interpreted. Further failures are counted in \
             code_buffer_protect_failures(), not reported."
        );
    }
    if protect_failure_latches_jit_unavailable(e) {
        let before = CODE_BUFFER_PROTECT_FAILURES.fetch_or(
            JIT_UNAVAILABLE_BY_POLICY_BIT,
            std::sync::atomic::Ordering::Relaxed,
        );
        if before & JIT_UNAVAILABLE_BY_POLICY_BIT == 0 {
            eprintln!(
                "[cratonvm-jit] WARNING: the OS refused executable JIT code by policy ({e}); \
                 the JIT is disabled for the rest of this process and every method stays \
                 interpreted. Allow dynamic code (SELinux execmem / Windows ACG) to re-enable it."
            );
        }
    }
}

/// Published bodies unmapped, and how many of those bypassed the retirement
/// queue.
pub fn published_code_free_audit() -> (usize, usize) {
    (
        PUBLISHED_CODE_FREES.load(std::sync::atomic::Ordering::Relaxed),
        UNQUEUED_PUBLISHED_CODE_FREES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `CRATONVM_DBG_JIT_CODE_FREE=1` — name the release site of every executable
/// buffer, and of every published body released outside the retirement queue.
pub(crate) fn dbg_jit_code_free_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| false)
}

pub(crate) fn record_code_free(base: usize, len: usize, active_executions: usize, flags: usize) {
    use std::sync::atomic::Ordering;
    // DIAG (`CRATONVM_DBG_JIT_CODE_FREE=1`): name the release site. Executable
    // buffers are unmapped a handful of times per process, so capturing a
    // backtrace here is free in practice and it is the only way to tell WHICH
    // owner dropped last — the crash it explains reports only an address.
    if dbg_jit_code_free_enabled() {
        eprintln!(
            "[jit-code-free] base={base:#x} len={len:#x} active_jit_executions={active_executions} \
published={} authorised={}\n{}",
            flags & CODE_FREE_PUBLISHED != 0,
            flags & CODE_FREE_AUTHORISED != 0,
            std::backtrace::Backtrace::force_capture()
        );
    }
    let i = RECENT_FREE_SEQ.fetch_add(1, Ordering::Relaxed) % RECENT_CODE_FREES;
    // Publish base last: a reader that sees a non-zero base sees the matching
    // length and count, and a torn slot reads as "empty" rather than as a wrong
    // range.
    RECENT_FREE_BASE[i].store(0, Ordering::Relaxed);
    RECENT_FREE_LEN[i].store(len, Ordering::Relaxed);
    RECENT_FREE_ACTIVE[i].store(active_executions, Ordering::Relaxed);
    RECENT_FREE_FLAGS[i].store(flags, Ordering::Relaxed);
    RECENT_FREE_BASE[i].store(base, Ordering::Release);
}

/// Total executable buffers unmapped by this process. Async-signal-safe.
pub fn code_frees_total() -> usize {
    RECENT_FREE_SEQ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Was `addr` inside one of the last [`RECENT_CODE_FREES`] executable buffers
/// this process unmapped? Returns
/// `(base, len, active_jit_executions_at_free, flags)`, where `flags` carries
/// [`CODE_FREE_PUBLISHED`], [`CODE_FREE_AUTHORISED`] and [`CODE_FREE_ENTERED`].
/// `len` is the emit CAPACITY; the mapping released was
/// [`NOT_ENTRANT_TAIL_RESERVE`] bytes longer, page-rounded by the OS.
///
/// Read the flags before the count. An unpublished buffer is one nothing could
/// point into, and an authorised release already carries a quiescence proof —
/// in both cases a non-zero `active_jit_executions_at_free` says nothing,
/// because the count is sampled at the `munmap` rather than at the decision.
///
/// Newest first. An address range is routinely freed more than once — the
/// allocator hands the same pages to the next compile of a similar size, and
/// the arena recycles blocks by design — and the entries for one range can
/// disagree about everything that matters: an unpublished discard, then a
/// published body released through the queue, then one released outside it.
/// The fault being explained happened AFTER the most recent of those, so that
/// is the one to report. Scanning slot 0 upward answered with whichever entry
/// happened to sit lowest in the ring, which after the first wrap is unrelated
/// to recency.
///
/// Async-signal-safe: atomic loads only.
pub fn recent_code_free_covering(addr: usize) -> Option<(usize, usize, usize, usize)> {
    use std::sync::atomic::Ordering;
    // `RECENT_FREE_SEQ` is the index the NEXT free will take, so the newest
    // record is at `seq - 1`. Wrapping arithmetic is exact modulo
    // `RECENT_CODE_FREES` because that is a power of two (asserted below), so
    // this also walks correctly across a `usize` wrap of the sequence.
    const _: () = assert!(RECENT_CODE_FREES.is_power_of_two());
    let newest = RECENT_FREE_SEQ.load(Ordering::Acquire).wrapping_sub(1);
    for back in 0..RECENT_CODE_FREES {
        let i = newest.wrapping_sub(back) % RECENT_CODE_FREES;
        let base = RECENT_FREE_BASE[i].load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let len = RECENT_FREE_LEN[i].load(Ordering::Relaxed);
        if addr >= base && addr < base.saturating_add(len) {
            return Some((
                base,
                len,
                RECENT_FREE_ACTIVE[i].load(Ordering::Relaxed),
                RECENT_FREE_FLAGS[i].load(Ordering::Relaxed),
            ));
        }
    }
    None
}


// ---------------------------------------------------------------------------
// Compiled method
// ---------------------------------------------------------------------------

/// NEW-12: one entry in a [`CompiledMethod`]'s oop map table.
///
/// Records, for a specific point in the emitted native code, the set of
/// frame-local slots that hold live object references. The GC root
/// walker consults this map at safepoints to enumerate *only* the real
/// oops, avoiding the false positives of a conservative range scan.
///
/// The map is keyed by `native_pc_offset` — the byte offset of the
/// instruction *after* the safepoint, relative to the compiled method's
/// entry pointer. At GC time, the walker retrieves the active frame's
/// return PC (which is always the instruction-after a call) and
/// subtracts the entry pointer to obtain the offset; this matches one
/// of the recorded entries exactly because safepoints are emitted
/// immediately before the call-that-may-trigger-GC.
///
/// `frame_slot_offsets` lists the positive byte distances *below RBP* of each
/// slot holding a live oop. A distance `off` addresses `[rbp - off]`; this is
/// the same convention used by the x64 emitter's `local_offset` and by the
/// relocation walker. Exactly one qword per entry — the frame layout
/// guarantees 8-byte alignment so smaller slots never appear.
///
/// The fields are `Vec<i16>` / `u32` to minimize the map's footprint.
/// A typical method has 1–5 oop maps with ≤ 16 slots each; the total
/// overhead is well under 100 bytes for the vast majority of methods.
#[derive(Debug, Clone)]
pub struct CompiledSiteRecord<'a> {
    pub safepoint_id: Option<u32>,
    pub bci: Option<u32>,
    pub oop_map: Option<&'a OopMapEntry>,
    pub inline_chain: Option<&'a crate::x64::InlineChain>,
    pub inline_chain_exact: bool,
}

#[derive(Debug, Clone)]
pub struct OopMapEntry {
    /// Byte offset into the compiled method's machine code of the
    /// instruction that follows the safepoint call. GC walkers match
    /// on this offset directly (not via ranges) because the safepoint
    /// is emitted *immediately* before the call.
    pub native_pc_offset: u32,
    /// Stage 3 (precise oop maps) — bytecode PC of the safepoint
    /// instruction. The JIT stores this into the frame's safepoint-id
    /// slot before each GC-capable call so the GC root walker can match
    /// it back to the *exact* map for the active safepoint (not the
    /// union-of-all-maps, which is unsafe for relocation). 0 / unused
    /// when the precise gate is off.
    pub bytecode_pc: u32,
    /// Frame slot offsets relative to RBP that hold live oops at this
    /// safepoint. `i16` is sufficient because frame sizes are capped
    /// well below 32 KiB in the current JIT; a larger frame would fail
    /// the compile-time max_locals check before reaching this code.
    pub frame_slot_offsets: Vec<i16>,
    /// True only when the moving-young shadow-stack publication for this
    /// safepoint proved complete enough for relocation under live JIT frames.
    pub moving_young_coverage_complete: bool,
    /// Which GPRs may hold a LIVE object reference at this safepoint, as a
    /// bitmask over `x64::ALL_SPILL_GPRS` positions — the register-file half of
    /// this map, and the one `frame_slot_offsets` never described.
    ///
    /// `None` is "no claim": the compiler could not prove the set, and a
    /// consumer must fall back to scanning the whole blind-spill image. That is
    /// the pre-2026-09-09 behaviour and it is the fail-open direction.
    ///
    /// `Some(mask)` says: **every register outside `mask` holds no live
    /// reference here.** `emit_pre_safepoint_spill` writes the register file
    /// into the frame's `reg_spill` region so a conservative scan can see it,
    /// and that image is blind — it cannot tell a live oop from a dead
    /// leftover, so every register's last value is retained. This mask is what
    /// lets the scan skip the leftovers.
    ///
    /// Bit `i` corresponds to `ALL_SPILL_GPRS[i]`, which is also the slot
    /// `emit_blind_reg_spill` writes it to (`reg_spill_lo + 8 * i`), so a
    /// consumer holding a frame offset inverts it with
    /// [`FrameLayout::spill_image_index`] and needs no register decoding.
    ///
    /// See `docs/feature-designs/jit-ir-optimizations.md`
    /// for what the blind image costs when nothing narrows it: five conservative
    /// words holding 2520 KB out of a G1 collection set, one of them an `r8`
    /// leftover.
    pub reg_oop_mask: Option<u16>,
    /// Exclusive `[rbp - off]` bound of the LIVE part of the frame at this
    /// safepoint: the operand-spill cursor (`next_spill_offset`) at the moment
    /// the safepoint was emitted. Slots at a larger offset are inside the
    /// `max_stack`-sized spill reserve but above the live operand stack and
    /// the staged invoke-argument buffer, so their contents are dead — the
    /// spill cursor reclaims by moving, it does not clear.
    ///
    /// The moving-young frame verifier uses this to stop reporting stale
    /// object pointers in reclaimed spill slots as un-rewritable roots.
    /// `0` means "unknown" and makes the verifier scan the whole region.
    pub live_frame_hi: i32,
    /// DIAGNOSTIC ORACLE -- the forward "must be oop" local dataflow mask that
    /// was in force when this map was built, as a bitmask over JVM local slots
    /// (bit `k` = local `k`, whose canonical home is `[rbp - 8*(k+1)]`).
    ///
    /// `None` means the dataflow never reached this safepoint's bci, i.e. the
    /// map named NO locals at all (`map_incomplete_cause::LOCAL_MASK_UNREACHED`).
    /// That is a different statement from `Some(0)`, which says the dataflow DID
    /// reach here and proved no local holds a reference, and the two must not be
    /// folded together: the first makes a stale word in the locals band
    /// unexplained, the second explains it as dead storage.
    ///
    /// Recorded so a stale-word report can say **live**. `live_frame_hi` is a
    /// spill watermark, not a liveness bound, so "below the watermark and not in
    /// the map" is an upper bound on missed roots and cannot be acted on; this
    /// mask is the in-tree oracle that turns such a word into one of "the
    /// dataflow proves this local is a reference" (a real miss) or "the dataflow
    /// proves it is not" (dead storage). See
    /// `bug-h2-testrandommapops-small-heap-corruption-20260829-RETIRED-20260909.md` §5.
    ///
    /// x86-64 single-pass only. The IR tier allocates frame slots rather than
    /// homing locals at `8*(k+1)`, so it records `None` and the oracle stays
    /// silent there rather than answering about the wrong frame layout.
    pub local_oop_mask: Option<u64>,
    /// Number of JVM locals of the frame `local_oop_mask` describes, so a word
    /// at `[rbp - 8*(k+1)]` with `k >= num_locals` can be told apart from a
    /// local the mask declined to name. `0` when there is no mask.
    pub num_locals: u16,
    /// The same oracle for the locals of each LIVE INLINE SPLICE, as
    /// `(base_off, num_locals, oop_mask)`: spliced callee local `k` is homed at
    /// `[rbp - (base_off + 8*k)]`.
    ///
    /// A splice's locals are allocated out of the operand-SPILL band
    /// (`reserve_spill_slots`), not the java-locals band, so `local_oop_mask`
    /// above cannot speak for them and every such word would otherwise read as
    /// unclassifiable spill. That matters because the frames this oracle exists
    /// for carry splices: "a spliced callee's locals are named by no oop map"
    /// is a defect this repo has already paid for once.
    ///
    /// Only scopes whose mask is known are recorded; a scope that cannot
    /// classify its locals already fails the safepoint closed
    /// (`map_incomplete_cause::INLINE_LOCAL_UNMAPPABLE`) and contributes
    /// nothing here, so a word in its band stays honestly unattributed.
    pub inline_local_scopes: Vec<(i32, u16, u64)>,
    /// Frame-resident OPERAND-STACK slots this safepoint's own stack model
    /// classified as NOT holding a reference.
    ///
    /// The marked ones are already in `frame_slot_offsets`; these are their
    /// complement, and they are what lets a stale word in the operand-spill
    /// band be read as dead storage rather than merely unexplained. Measured
    /// need: on `org.h2.test.store.TestRandomMapOps`, 36 of the 37 stale words
    /// below `live_frame_hi` sit in `region=operand-spill`, where the locals
    /// oracle above is silent.
    ///
    /// Only meaningful when `stack_marks_exact`; a mark vector that nobody
    /// classified was PADDED with "not an oop", which is a default and not a
    /// proof.
    ///
    /// Two readers spend it as a ROOT claim, not only as a diagnostic: the
    /// optimizing tier's prim-colour claim (`conservative_roots::ir_prim_slots`,
    /// default on) and, for a single-pass map, [`SpStackPrimFold`] (gch h4/jm,
    /// opt-in `CRATONVM_GC_SP_STACK_PRIM_ROOTS`). The single-pass producer
    /// pushes it in operand-stack order, which is usually but not always
    /// ascending (`invalidate_callee_saved` re-homes entries to fresh, higher
    /// slots); the fold refuses an unsorted list.
    pub non_oop_stack_slots: Vec<i16>,
    /// Whether the mark vector behind `non_oop_stack_slots` was exact
    /// (`Compiler::stack_oop_marks_exact`). False turns every entry above from
    /// a proof into a guess, so the report must not spend it.
    pub stack_marks_exact: bool,
    /// §25.3's probe — how many oop homes the SHADOW PUSH paired with this
    /// safepoint published, captured just before `emit_shadow_reload` pops
    /// them.
    ///
    /// Shadow publication is CALL-SCOPED (§25.2): `emit_shadow_push` runs
    /// before the call and `emit_shadow_reload` pops right after it returns.
    /// The band verifier's obligation is not scoped that way, so a frame
    /// stopped at a safepoint that published nothing reports every movable word
    /// it holds as un-rewritable — even the ones this map names. That is the
    /// shape §22.2 measured (67 of 93 reported words `in_map=true`) and could
    /// not explain.
    ///
    /// `0` says this safepoint pushed nothing. It does not by itself say
    /// whether that is because the site is poll-shaped, because the gate was
    /// off, or because no oop was live — those are separated by the
    /// `shadow_incomplete_cause` counters.
    pub shadow_pushed: u16,
    /// gcg g3/v (`gcg-g1s-proposal-oop-map-records-the-spill-images-written-set`,
    /// opt-in `CRATONVM_JIT_SPILL_WRITTEN_SET`): which blind-spill image slots
    /// EVERY spill emitted at this safepoint's id wrote, as a bitmask over
    /// `x64::ALL_SPILL_GPRS` positions (the same bit order as
    /// [`Self::reg_oop_mask`]).
    ///
    /// `None` is "no claim" (the switch off, a backend without the `=all`
    /// image, or a spill at this id that could not say). `Some(w)` says: **an
    /// image slot outside `w` was not written at this safepoint**, so it holds
    /// an EARLIER safepoint's register value, which the narrowed selection
    /// proved this site does not need (`x64/safepoint.rs`,
    /// `oop_capable_spill_regs`; `Some(0)` after a metadata-only safepoint,
    /// whose elision proof says no live reference is only in a register). The
    /// band scan skips those slots whatever `reg_oop_mask` says. Unlike a
    /// `Some` register mask, it licenses nothing outside the image (the
    /// outgoing-reserve claim does not read it).
    pub spill_written: Option<u16>,
}

impl OopMapEntry {
    /// Construct an empty oop map for `native_pc_offset`. Callers that
    /// want to build a map programmatically push to
    /// [`Self::frame_slot_offsets`] directly.
    pub fn new(native_pc_offset: u32) -> Self {
        Self {
            native_pc_offset,
            bytecode_pc: 0,
            frame_slot_offsets: Vec::new(),
            moving_young_coverage_complete: false,
            reg_oop_mask: None,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
            spill_written: None,
        }
    }

    /// Return the number of oop slots recorded at this safepoint.
    pub fn slot_count(&self) -> usize {
        self.frame_slot_offsets.len()
    }
}

// gch h4/jm (2026-10-09) -- the producer half of
// `gch-h3yc-proposal-single-pass-non-oop-operand-slots-are-not-roots-REFUTED-20261015`.
//
// A single-pass map lists, beside the operand slots it names as references,
// the frame-resident operand slots its stack model classified as NOT holding a
// reference (`OopMapEntry::non_oop_stack_slots`), a proof only when the mark
// vector was exact (`stack_marks_exact`). The collector's band scan spends the
// optimizing tier's twin of that list (the prim-colour claim, `ir_prim_slots`
// in `vm/src/jit/conservative_roots.rs`) and leaves the single-pass list alone.
// [`SpStackPrimFold`] is the fold the scan runs over the maps of ONE active
// safepoint id to decide whether the single-pass list may be spent the same
// way. It fails closed on every doubt:
//
//   * any map of the id whose marks were not exact refuses the whole id;
//   * the maps of the id must publish the SAME list (a slot one map calls a
//     primitive and another an operand reference stays a root);
//   * a listed slot that its own map ALSO names as a reference (an aliased
//     entry) refuses;
//   * the answer is a borrowed slice the scan binary-searches, so an unsorted
//     list (the producer pushes in stack order, which `invalidate_callee_saved`
//     can permute) refuses rather than risk a miss being read as a proof --
//     a miss only keeps the word, but a refusal says so in one place;
//   * every listed slot must lie in the frame's `operand-spill` region.
//
// The scan side still lets go only of a word it would ROOT (an object start)
// that is not a minted handle (`memory::smuggled_longs`), exactly as for the
// prim colours. Switch (scan side): `CRATONVM_GC_SP_STACK_PRIM_ROOTS`, opt-in.

/// The fold over one single-pass safepoint id's maps; see the section above.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpStackPrimFold<'a> {
    pub(crate) slots: Option<&'a [i16]>,
    pub(crate) refused: bool,
}

impl<'a> SpStackPrimFold<'a> {
    /// An empty fold (no map seen: claims nothing).
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one map of the active id in.
    pub fn add(&mut self, m: &'a OopMapEntry) {
        if self.refused {
            return;
        }
        if !m.stack_marks_exact {
            self.refused = true;
            return;
        }
        let s = m.non_oop_stack_slots.as_slice();
        if s.iter().any(|o| m.frame_slot_offsets.contains(o)) {
            self.refused = true;
            return;
        }
        match self.slots {
            None => self.slots = Some(s),
            Some(first) if first == s => {}
            Some(_) => self.refused = true,
        }
    }

    /// The operand slots the folded maps prove hold no reference of this
    /// activation, ascending and distinct, or `None` (claim nothing).
    pub fn finish(self, layout: &FrameLayout) -> Option<&'a [i16]> {
        if self.refused {
            return None;
        }
        let s = self.slots?;
        if s.is_empty()
            || !s.windows(2).all(|w| w[0] < w[1])
            || !s
                .iter()
                .all(|&o| layout.region_name(i32::from(o)) == "operand-spill")
        {
            return None;
        }
        Some(s)
    }
}


// ---------------------------------------------------------------------------
// Precise frame liveness (gen r5w3/live7, `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`)
// ---------------------------------------------------------------------------
//
// A compiled frame's band is scanned CONSERVATIVELY: every word below the
// safepoint's watermark that looks like an object is a root. HotSpot's oop map
// names only the values live at the safepoint. This table is the emitter's
// half of closing that gap WITHOUT a store in compiled code: for each safepoint
// that makes a claim it says which of a declared set of frame words (`covered`)
// hold a LIVE reference there. A covered word the record does not list is
// proven dead, and the collector's band scan (`vm/src/jit/conservative_roots.rs`,
// `scan_one_frame_filtered` and the pin publication beside it) skips it.
//
// A wrong claim frees a live object, so the table is built to fail closed:
//
//   * a safepoint with no record makes no claim (nothing is skipped);
//   * a record whose `complete` is `false` makes no claim;
//   * when several records share a safepoint id, a word is dead only if EVERY
//     one of them proves it dead (an intersection of dead sets -- a union of
//     live ones);
//   * a word outside `covered` is never proven dead, whatever the records say.
//
// Empty (no claim anywhere) unless the opt-in was set when the body was
// compiled. Only the optimizing tier (`ir_lower.rs`) produces it today.

/// One safepoint's liveness claim over [`FrameLivenessTable::covered`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafepointFrameLiveness {
    /// The safepoint id ([`OopMapEntry::bytecode_pc`]) the claim is for -- the
    /// value the frame's `[rbp - sp_id_slot_off]` word holds while that
    /// safepoint is active.
    pub sp_id: u32,
    /// The covered words (positive `[rbp - off]` offsets) that hold, or may
    /// hold, a LIVE reference at this safepoint. Sorted ascending, distinct.
    pub live: Vec<i16>,
    /// `true` when `live` is complete for EVERY covered word. `false` is "no
    /// claim", which is what a record the emitter could not finish says.
    pub complete: bool,
}

/// The per-body liveness table: the words the claims speak for, and one
/// record per claiming safepoint. See the section comment above.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameLivenessTable {
    /// Every frame word (positive `[rbp - off]` offset) any record of this
    /// body speaks for. Sorted ascending, distinct.
    pub covered: Vec<i16>,
    /// Sorted by `sp_id` (ascending, duplicates adjacent).
    pub records: Vec<SafepointFrameLiveness>,
}

impl FrameLivenessTable {
    /// `true` when no safepoint of this body makes a claim.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() || self.covered.is_empty()
    }

    /// Does the table PROVE the word at `[rbp - off]` dead while safepoint
    /// `sp_id` is active? `false` -- keep scanning it -- whenever it cannot:
    /// an uncovered word, no record for the id, an incomplete record, or a
    /// record that lists the word as live.
    pub fn proves_dead(&self, sp_id: u32, off: i16) -> bool {
        if self.covered.binary_search(&off).is_err() {
            return false;
        }
        let start = self.records.partition_point(|r| r.sp_id < sp_id);
        let mut any = false;
        for r in self.records[start..]
            .iter()
            .take_while(|r| r.sp_id == sp_id)
        {
            if !r.complete || r.live.binary_search(&off).is_ok() {
                return false;
            }
            any = true;
        }
        any
    }
}


// ---------------------------------------------------------------------------
// Stage 5 (precise oop maps) — JIT code-range registry
// ---------------------------------------------------------------------------
//
// Maps each compiled method's native code range `[entry, entry+len)` to an
// Arc-stable `CompiledMethod` pointer. The GC root walker uses it to resolve
// which method a return address belongs to while walking the JIT RBP chain, so
// it can remap EVERY active JIT frame (not just the innermost). Populated at
// `JitCache::put`. Range snapshots are immutable and lock-free; the matching
// `CompiledMethod` stays alive through cache, direct-call, inline-cache, and
// active-reader ownership. Its final Drop unregisters the range before unmap.

/// `(start, end, cm_ptr, owner)`.
///
/// `owner` is what makes [`pin_jit_code_range_owner`] lock-free, and it exists
/// because a *pin* — not a bare `cm_ptr` — is what the JIT-dispatch helper needs
/// before it calls a raw entry. `cm_ptr` is retained beside it for the root
/// walkers, which run under their own quiescence rules and want no refcount
/// traffic per stack word.
pub(crate) type JitCodeRange = (usize, usize, usize, std::sync::Weak<CompiledMethod>);

/// Copy-on-write code-range registry.
///
/// Readers take an atomically reference-counted immutable snapshot and never
/// acquire the writer mutex, so the lookup path stays lock-free and O(log n).
pub(crate) struct JitCodeRangeRegistry {
    pub(crate) snapshot: arc_swap::ArcSwap<JitCodeRangeSet>,
    pub(crate) writer: std::sync::Mutex<()>,
}

impl JitCodeRangeRegistry {
    pub(crate) fn new() -> Self {
        Self {
            snapshot: arc_swap::ArcSwap::from_pointee(JitCodeRangeSet::empty()),
            writer: std::sync::Mutex::new(()),
        }
    }
}

/// One immutable snapshot of the registry, split so a registration does not
/// copy the whole table.
///
/// A single sorted `Vec` cloned per registration made every publication O(n),
/// and N publications O(N^2). Now a registration copies only `recent`, which
/// is capped at [`recent_code_range_capacity`] (about sqrt(n)), and shares
/// `base` by `Arc`. When `recent` is full the two are merged into a new `base`,
/// an O(n) step taken once per sqrt(n) registrations. That is O(sqrt(n))
/// amortised per registration.
///
/// Removals are symmetric (round 11, lane `rt`). A removal from `base` used to
/// rebuild it outright — an O(n) copy that also CLONES every entry's `Weak`
/// owner, i.e. n atomic increments now and n decrements when the old snapshot
/// dies — so reclaiming N bodies out of a table of n was O(N * n). That is
/// exactly what an invalidation does: `JitCache::clear_all` on a JVMTI redefine
/// withdraws the whole cache, and the retirement queue then drops the bodies
/// back to back. A removal now appends the start to `removed` (bounded like
/// `recent`), and only an overflow rebuilds `base`.
///
/// Live ranges are disjoint (each is a live executable buffer, and `Drop`
/// unregisters before unmapping), so an address is covered by at most one LIVE
/// entry. A tombstoned `base` entry may overlap a later registration in
/// `recent` (the allocator reused its address), which is why every reader
/// consults `recent` first and treats a tombstoned `base` hit as a miss: the
/// `base` entries were all live together at the last merge, so no live `base`
/// entry can be hidden behind a tombstoned one.
pub(crate) struct JitCodeRangeSet {
    /// Sorted by start; shared unchanged between snapshots until a merge.
    pub(crate) base: Arc<Vec<JitCodeRange>>,
    /// Sorted by start; the registrations since the last merge.
    pub(crate) recent: Vec<JitCodeRange>,
    /// Sorted, deduplicated starts of `base` entries unregistered since the
    /// last merge. Never names a `recent` entry.
    pub(crate) removed: Vec<usize>,
}

impl JitCodeRangeSet {
    pub(crate) fn empty() -> Self {
        Self {
            base: Arc::new(Vec::new()),
            recent: Vec::new(),
            removed: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        // Every tombstone names a distinct `base` entry, so this never
        // saturates; saturating anyway keeps a broken invariant a wrong count
        // rather than a wrapped one.
        self.base.len().saturating_sub(self.removed.len()) + self.recent.len()
    }

    /// Whether the `base` entry starting at `start` has been unregistered.
    #[inline]
    pub(crate) fn is_removed(&self, start: usize) -> bool {
        !self.removed.is_empty() && self.removed.binary_search(&start).is_ok()
    }

    /// The range covering `addr`, if any.
    pub(crate) fn covering(&self, addr: usize) -> Option<&JitCodeRange> {
        fn search(ranges: &[JitCodeRange], addr: usize) -> Option<&JitCodeRange> {
            let candidate = ranges.partition_point(|(entry, _, _, _)| *entry <= addr);
            let range = ranges.get(candidate.checked_sub(1)?)?;
            (addr >= range.0 && addr < range.1).then_some(range)
        }
        if let Some(range) = search(&self.recent, addr) {
            return Some(range);
        }
        search(&self.base, addr).filter(|range| !self.is_removed(range.0))
    }

    /// Whether some LIVE range starts exactly at `entry`.
    pub(crate) fn starts_at(&self, entry: usize) -> bool {
        self.recent_starts_at(entry) || self.live_base_starts_at(entry)
    }

    /// Whether a `recent` range starts exactly at `entry`.
    pub(crate) fn recent_starts_at(&self, entry: usize) -> bool {
        let at = self
            .recent
            .partition_point(|(start, _, _, _)| *start < entry);
        self.recent.get(at).is_some_and(|range| range.0 == entry)
    }

    /// Whether a `base` range that has not been tombstoned starts at `entry`.
    pub(crate) fn live_base_starts_at(&self, entry: usize) -> bool {
        let at = self.base.partition_point(|(start, _, _, _)| *start < entry);
        self.base.get(at).is_some_and(|range| range.0 == entry) && !self.is_removed(entry)
    }

    /// Visit every LIVE range in ascending start order.
    pub(crate) fn for_each_sorted(&self, mut visit: impl FnMut(&JitCodeRange)) {
        let (base, recent) = (&self.base[..], &self.recent[..]);
        let (mut i, mut j) = (0usize, 0usize);
        while i < base.len() && j < recent.len() {
            if base[i].0 <= recent[j].0 {
                if !self.is_removed(base[i].0) {
                    visit(&base[i]);
                }
                i += 1;
            } else {
                visit(&recent[j]);
                j += 1;
            }
        }
        for range in &base[i..] {
            if !self.is_removed(range.0) {
                visit(range);
            }
        }
        recent[j..].iter().for_each(&mut visit);
    }

    /// Every range except those starting at `skip`, merged into one sorted
    /// `Vec` with spare room for one more.
    pub(crate) fn merged_without(&self, skip: Option<usize>) -> Vec<JitCodeRange> {
        let mut out = Vec::with_capacity(self.len() + 1);
        self.for_each_sorted(|range| {
            if Some(range.0) != skip {
                out.push(range.clone());
            }
        });
        out
    }
}

/// Most registrations `recent` holds before it is merged into `base`.
pub(crate) fn recent_code_range_capacity(base_len: usize) -> usize {
    ((base_len as f64).sqrt() as usize).max(32)
}

pub(crate) static JIT_CODE_RANGES: std::sync::OnceLock<JitCodeRangeRegistry> = std::sync::OnceLock::new();

pub(crate) fn jit_code_ranges() -> &'static JitCodeRangeRegistry {
    JIT_CODE_RANGES.get_or_init(JitCodeRangeRegistry::new)
}

// ---------------------------------------------------------------------------
// Compile-id table — naming the method that owns the innermost RBP
// ---------------------------------------------------------------------------
//
// `conservative_roots::innermost_frame_method` has to answer "which
// `CompiledMethod` owns the frame standing at `exact_rbp`?". Its only evidence
// was the saved return address, from which it decoded the five-byte
// `CALL rel32` that must have created the frame. An **indirect** JIT->JIT call
// — every inline-cache hit ends in `CALL R11`, plus the megamorphic stub and
// the trampolines — encodes no rel32, so the decode failed closed and the whole
// young collection diverted to the non-moving sweep (`FOREIGN_INNERMOST_RBP`,
// plus the `UNBOUNDED_FRAME_BAND` the same failure co-emits from the band
// walk). Measured on dev 892ab4f40 that pair is 419 of 419 of
// `Log4J2LoggingSystemTests`'s fallback cycles under `-XX:+UseGenerationalGC`,
// with nothing else in the set.
//
// The prologue already publishes its RBP into a TLS slot with one
// segment-relative `mov`. Publishing an IDENTITY beside it removes the decode:
// both stores are emitted together at every point that touches the mirror, so
// `(rbp, id)` describe the same frame by construction.
//
// Why a dense u32 and not the `CompiledMethod` pointer: `mov dword
// <seg>:[disp], imm32` needs no scratch register. The post-call republish paths
// run with the callee's return value live in RAX and document that they must
// not disturb it; a 64-bit immediate would need a register and put a push/pop
// back on every JIT->JIT call site.
//
// The id is reserved BEFORE codegen (the immediate is encoded while the
// prologue is emitted) and bound at publication, because the `CompiledMethod`
// does not exist until the buffer it owns has been filled. An id that is still
// unbound — reserved but not published, or released on drop — resolves to
// `None`, which leaves the caller with exactly the decode-and-fail-closed
// behaviour it had before.
//
// # Storage, recycling and exhaustion
//
// The table used to be a `RwLock<Vec<usize>>` that grew by one word per
// compilation and never shrank, and whose lookup took the lock — from a root
// scan that may run while a frozen peer holds it. Now:
//
// * slots live in fixed chunks of `AtomicUsize`, allocated on demand and never
//   freed or moved, so a lookup is two atomic loads and no lock;
// * a released id goes on a FIFO free list stamped with a retirement
//   generation, and is reissued only once that generation is CODE-graced
//   (every running thread has returned to JIT depth 0 since, or every thread
//   was outside compiled code; `ReclamationCounters::code_graced_generation`).
//   A mirror left holding the old id then cannot resolve it to the new body.
//   This is deliberately not the inline-cache grace, which since M8-1 also
//   advances on a miss-handler stamp and at a cooperative stop, where the
//   thread is still inside compiled bodies (round 14 wave 3, lane fixup,
//   RV3-2; `CRATONVM_JIT_COMPILE_ID_CODE_GRACE=0` restores the old rule);
// * the table bounds the ids LIVE at once, not the compilations in a run. When
//   it is full, `reserve_compile_id` still returns 0, but counts it
//   (`compile_id_exhaustions`) and says so once on stderr.

/// Ids per chunk of the compile-id table, as a shift.
pub(crate) const COMPILE_ID_CHUNK_BITS: u32 = 12;
pub(crate) const COMPILE_ID_CHUNK_LEN: usize = 1 << COMPILE_ID_CHUNK_BITS;
/// Chunks the table can grow to: 4096 x 4096 ids live at once.
pub(crate) const COMPILE_ID_CHUNK_COUNT: usize = 4096;

/// A slot reserved by `reserve_compile_id` and not yet bound. Never a
/// `CompiledMethod` address (those are aligned), and resolves to `None`.
pub(crate) const COMPILE_ID_RESERVED: usize = 1;

pub(crate) type CompileIdChunk = [std::sync::atomic::AtomicUsize; COMPILE_ID_CHUNK_LEN];

/// `id -> CompiledMethod address`, with 0 for a free slot and
/// [`COMPILE_ID_RESERVED`] for a reserved one.
pub(crate) static COMPILE_ID_CHUNKS: [std::sync::atomic::AtomicPtr<CompileIdChunk>; COMPILE_ID_CHUNK_COUNT] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; COMPILE_ID_CHUNK_COUNT];

/// Reservations refused because every id was live or not yet graced.
pub(crate) static COMPILE_ID_EXHAUSTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The slot for `id`, if its chunk has been allocated.
pub(crate) fn compile_id_slot(id: u32) -> Option<&'static std::sync::atomic::AtomicUsize> {
    let chunk = COMPILE_ID_CHUNKS
        .get((id >> COMPILE_ID_CHUNK_BITS) as usize)?
        .load(std::sync::atomic::Ordering::Acquire);
    if chunk.is_null() {
        return None;
    }
    // SAFETY: a published chunk is a leaked `Box` that is never freed or moved.
    let chunk: &'static CompileIdChunk = unsafe { &*chunk };
    chunk.get(id as usize & (COMPILE_ID_CHUNK_LEN - 1))
}

/// Which ids may be issued next. Only `reserve_compile_id` and
/// `release_compile_id` touch it, under its lock; lookups never do.
pub(crate) struct CompileIdAllocator {
    /// Next never-issued id. Starts at 1: an unwritten or stale TLS slot reads
    /// as 0 and must not resolve to a real method.
    pub(crate) next: u32,
    /// One past the largest id the table can hold.
    pub(crate) limit: u32,
    /// Released ids, oldest first, each with the retirement generation stamped
    /// when it was released.
    pub(crate) free: std::collections::VecDeque<(u32, u64)>,
}

impl CompileIdAllocator {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            next: 1,
            limit,
            free: std::collections::VecDeque::new(),
        }
    }

    /// The oldest released id whose stamp `graced` accepts, else a fresh id,
    /// else `None`.
    pub(crate) fn reserve(&mut self, graced: impl Fn(u64) -> bool) -> Option<u32> {
        if let Some(&(id, stamp)) = self.free.front() {
            if graced(stamp) {
                self.free.pop_front();
                return Some(id);
            }
        }
        if self.next >= self.limit {
            return None;
        }
        let id = self.next;
        self.next += 1;
        Some(id)
    }

    pub(crate) fn release(&mut self, id: u32, stamp: u64) {
        self.free.push_back((id, stamp));
    }
}

pub(crate) fn compile_id_allocator() -> &'static parking_lot::Mutex<CompileIdAllocator> {
    static ALLOCATOR: OnceLock<parking_lot::Mutex<CompileIdAllocator>> = OnceLock::new();
    ALLOCATOR.get_or_init(|| {
        parking_lot::Mutex::new(CompileIdAllocator::new(
            (COMPILE_ID_CHUNK_COUNT * COMPILE_ID_CHUNK_LEN) as u32,
        ))
    })
}

/// Reserve an identity for a compilation that has not produced its
/// `CompiledMethod` yet. `0` means "no identity" — codegen then emits no
/// publication and the scan keeps its old decode path for that method. It is
/// returned only when every id is live or awaiting grace, and each such refusal
/// is counted in [`compile_id_exhaustions`].
pub fn reserve_compile_id() -> u32 {
    let code_grace =
        true;
    let mut allocator = compile_id_allocator().lock();
    let Some(id) = allocator.reserve(|g| compile_id_generation_is_graced(g, code_grace)) else {
        drop(allocator);
        if COMPILE_ID_EXHAUSTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
            eprintln!(
                "[jit-compile-id] every compile id is live or awaiting grace; compiled \
frames of new methods fall back to the return-address decode in root scans"
            );
        }
        return 0;
    };
    let chunk = &COMPILE_ID_CHUNKS[(id >> COMPILE_ID_CHUNK_BITS) as usize];
    if chunk.load(std::sync::atomic::Ordering::Acquire).is_null() {
        // Allocated under the allocator lock, so no two reservations race to
        // publish the same chunk.
        let fresh: Box<CompileIdChunk> = Box::new(std::array::from_fn(|_| {
            std::sync::atomic::AtomicUsize::new(0)
        }));
        chunk.store(Box::into_raw(fresh), std::sync::atomic::Ordering::Release);
    }
    if let Some(slot) = compile_id_slot(id) {
        slot.store(COMPILE_ID_RESERVED, std::sync::atomic::Ordering::Release);
    }
    id
}

/// A compile id reserved for a compilation still in progress.
///
/// Dropping it releases the id. A compile that bails after reserving (an
/// overflowed buffer, an unallocated slot, any refusal) therefore returns its
/// id instead of leaving the slot reserved for the life of the process. The
/// finalizer moves the id into the `CompiledMethod` with [`Self::hand_off`],
/// whose own `Drop` releases it from then on.
#[derive(Debug)]
pub(crate) struct CompileIdReservation(pub(crate) u32);

impl CompileIdReservation {
    /// Reserve an id; see [`reserve_compile_id`] for when this holds `0`.
    pub(crate) fn reserve() -> Self {
        Self(reserve_compile_id())
    }

    /// No identity: the prologue publishes nothing for this compile.
    pub(crate) fn none() -> Self {
        Self(0)
    }

    /// The reserved id, `0` when there is none.
    pub(crate) fn id(&self) -> u32 {
        self.0
    }

    /// Give the id to the artifact that will own it. This reservation holds
    /// `0` afterwards, so dropping it releases nothing.
    pub(crate) fn hand_off(&mut self) -> u32 {
        std::mem::take(&mut self.0)
    }
}

impl Drop for CompileIdReservation {
    fn drop(&mut self) {
        release_compile_id(self.0);
    }
}

/// Reservations [`reserve_compile_id`] refused because no id was available.
pub fn compile_id_exhaustions() -> u64 {
    COMPILE_ID_EXHAUSTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bind a reserved id to the published artifact. Called once, at publication.
pub fn bind_compile_id(id: u32, cm_ptr: usize) {
    if id == 0 || cm_ptr == 0 {
        return;
    }
    if let Some(slot) = compile_id_slot(id) {
        slot.store(cm_ptr, std::sync::atomic::Ordering::Release);
    }
}

/// Clear a binding when its artifact is dropped and queue the id for reuse. No
/// frame of a dropped method can be live, so a later lookup answering `None` is
/// the correct answer. The id is reissued only after grace (see above).
pub fn release_compile_id(id: u32) {
    if id == 0 {
        return;
    }
    let Some(slot) = compile_id_slot(id) else {
        return;
    };
    let mut allocator = compile_id_allocator().lock();
    // A slot already free was never reserved, or was released twice. Queueing
    // it again would hand one id to two bodies.
    if id >= allocator.next || slot.swap(0, std::sync::atomic::Ordering::AcqRel) == 0 {
        return;
    }
    // Stamped after the slot is cleared, so grace postdates the clear.
    let stamp = bump_retire_generation();
    allocator.release(id, stamp);
}

/// Resolve a published id to its `CompiledMethod` address, or `None` when the
/// id is 0, out of range, reserved or unbound. Lock-free.
pub fn lookup_compile_id(id: u32) -> Option<usize> {
    if id == 0 {
        return None;
    }
    match compile_id_slot(id)?.load(std::sync::atomic::Ordering::Acquire) {
        0 | COMPILE_ID_RESERVED => None,
        ptr => Some(ptr),
    }
}

/// PERF (2026-07-15, RequestMappingMessageConversionIntegrationTests bootstrap
/// slowness, round 2): monotonically-increasing generation counter, bumped
/// whenever the registered code-range set changes. `native_stack_has_jit_frame`
/// (`vm/src/jit/conservative_roots.rs`) used to call `snapshot_code_ranges_into`
/// — a full lock + Vec copy + `sort_unstable()` over the ENTIRE set — on every
/// single call, even though it caches the result in a thread-local buffer.
/// That "cache" was write-only: it got clobbered and fully rebuilt every call
/// regardless of whether the underlying set had changed since the previous
/// call. Since `native_stack_has_jit_frame` runs on the per-native-call
/// root-snapshot path (see the comment on that function), this was an
/// O(n log n) cost paid on EVERY native call, with n = the registered ranges
/// (which then only grew: compiled code was retained, not freed. Today a range
/// is withdrawn when its body is reclaimed, and the generation advances only
/// when the set actually changes). Exposed dramatically by the
/// 2026-07-15 invoke-cache fix (this same file's caller-side history): making
/// JDK-internal bytecode actually tier up to JIT (previously it barely did)
/// multiplied the number of registered code ranges, which multiplied this
/// per-call sort cost right along with it. Callers now compare this counter
/// against a cached "last-seen" value and skip the resnapshot/resort entirely
/// when nothing changed (the overwhelmingly common case within one scan burst).
pub(crate) static JIT_CODE_RANGES_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Current code-range-set generation. Callers that cache a sorted snapshot
/// (e.g. `native_stack_has_jit_frame`'s thread-local buffer) can skip a
/// resnapshot/resort when this hasn't advanced since their last read.
pub fn jit_code_ranges_generation() -> u64 {
    JIT_CODE_RANGES_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Register `[entry, entry+len)` → `cm_ptr` (the `Arc<CompiledMethod>` inner
/// address). No-op for empty/zero ranges. Stage 5.
pub fn register_jit_code_range(entry: usize, len: usize, cm_ptr: usize) {
    register_jit_code_range_inner(entry, len, cm_ptr, std::sync::Weak::new())
}

/// Register a range together with a weak handle on its owner, so
/// [`pin_jit_code_range_owner`] can retain the body without taking a lock.
///
/// This is the form the two publication sites use. The bare
/// [`register_jit_code_range`] remains for synthetic ranges (tests) that have no
/// `Arc` to downgrade; pinning one of those falls back to the locking path.
pub fn register_jit_code_range_owned(entry: usize, len: usize, owner: &Arc<CompiledMethod>) {
    register_jit_code_range_inner(
        entry,
        len,
        Arc::as_ptr(owner) as usize,
        Arc::downgrade(owner),
    )
}

/// Give a body that is entered WITHOUT being published through `JitCache` the
/// same frame identity publication gives every other body: bind its compile id
/// and register its code range, under the gate `JitCache::put` / `put_osr` use.
///
/// Every frame walker resolves a compiled PARENT frame from the return address
/// its callee left on the stack (`lookup_jit_code_range`), and the innermost
/// frame from the compile id its prologue published (`lookup_compile_id`). A
/// body registered in neither is invisible to both: a caller-side walk stops at
/// it as if it were the interpreter, and `innermost_frame_method` reads a return
/// address INTO it as "the ordinary boundary frame" and answers with the chain
/// entry's method for its callee's frame. The accepted optimizing OSR body is
/// such a body (kept in the bridge's `osr_optimizing_accepted` memo, never in
/// the cache), and since it can direct-CALL compiled callees, its callee's frame
/// was reported under the OSR body's name and the OSR body's own frame was
/// dropped from stack traces, the GC's precise root/band scan, the moving-young
/// coverage proof and the remap walk alike
/// (`docs/feature-designs/deopt-osr.md`).
///
/// Both halves are withdrawn by `CompiledMethod::drop`, which unregisters the
/// range and releases the id unconditionally, so no teardown is owed here.
pub fn register_unpublished_body(owner: &Arc<CompiledMethod>) {
    note_body_entered_unpublished(owner);
    owner.note_bound_arc();
    bind_compile_id(owner.compile_id, Arc::as_ptr(owner) as usize);
    // Round 11 wave 19 (lane lhm): the accepted optimizing OSR body is the
    // longest-lived caller there is and is never published through a
    // `JitCache`, so its inline caches are bound here. Binding only makes the
    // slots RECORD what they publish; the supersede walk that acts on the
    // record is gated per cache. The same kill switch is asked here (once per
    // accepted body, so an uncached read), so that `=0` also stops the
    // publish-after-supersede withdrawal a bound slot makes.
    {
        bind_inline_cache_holders(owner);
    }
    if crate::x64::precise_jit_maps_enabled() || xt_jit_root_scan_enabled() {
        register_jit_code_range_owned(owner.entry_ptr() as usize, owner.code_len(), owner);
    }
}

/// Round 13 wave 8 (lane codecache): a body generated code ENTERS without its
/// ever being published gets the published body's release rule — a last-owner
/// drop outside the retirement queue is rescued into it rather than unmapping
/// (`ExecutableBuffer::note_entered_unpublished`). Without it that drop was
/// immediate whatever thread was inside, and the crash ring then labelled the
/// freed range "NEVER PUBLISHED: a discarded compile attempt".
///
/// `CRATONVM_JIT_RESCUE_ENTERED_UNPUBLISHED=0` restores the immediate unmap.
/// Read uncached: once per accepted OSR body or adapter thunk, like the
/// inline-cache switch `register_unpublished_body` reads.
pub(crate) fn note_body_entered_unpublished(owner: &CompiledMethod) {
    {
        owner._buffer.note_entered_unpublished();
    }
}

pub(crate) fn register_jit_code_range_inner(
    entry: usize,
    len: usize,
    cm_ptr: usize,
    owner: std::sync::Weak<CompiledMethod>,
) {
    if entry == 0 || len == 0 || cm_ptr == 0 {
        return;
    }
    let registry = jit_code_ranges();
    // Poison is recovered: skipping a registration after an unrelated panic
    // would hide this body from every root walker.
    let _writer = registry.writer.lock().unwrap_or_else(|e| e.into_inner());
    let current = registry.snapshot.load();
    // INSERT, do not push-then-sort: both halves are already sorted by
    // `start`, so the ordering work is placing ONE element. (A push-then-sort
    // paid O(n log n) per registration; `register_jit_code_range_inner` and
    // its sorts were ~0.7% of the WebClient exchange profile with 155
    // compiled methods.)
    let range = (entry, entry.saturating_add(len), cm_ptr, owner);
    let next = if current.recent.len() < recent_code_range_capacity(current.base.len()) {
        let mut recent = Vec::with_capacity(current.recent.len() + 1);
        recent.extend(current.recent.iter().cloned());
        let at = recent.partition_point(|(start, _, _, _)| *start < entry);
        recent.insert(at, range);
        JitCodeRangeSet {
            base: Arc::clone(&current.base),
            recent,
            removed: current.removed.clone(),
        }
    } else {
        // The merge drops every tombstoned `base` entry, so it starts a fresh
        // (empty) tombstone list.
        let mut base = current.merged_without(None);
        let at = base.partition_point(|(start, _, _, _)| *start < entry);
        base.insert(at, range);
        JitCodeRangeSet {
            base: Arc::new(base),
            recent: Vec::new(),
            removed: Vec::new(),
        }
    };
    registry.snapshot.store(Arc::new(next));
    // Release: any cached snapshot taken with Acquire after this point must see
    // the insertion above (the counter is read outside the lock by cache-check
    // callers).
    JIT_CODE_RANGES_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Remove every range starting at `entry`. Called from `CompiledMethod::drop`
/// before the body's buffer is unmapped, and by tests.
///
/// Every dropped body calls this, including the many that never registered a
/// range (registration is gated, see `JitCache::put`). So the absent case is
/// answered from the lock-free snapshot, and neither copies the table nor
/// advances [`jit_code_ranges_generation`]: advancing it for a no-op made every
/// `native_stack_has_jit_frame` cache resnapshot after each reclamation.
pub fn unregister_jit_code_range(entry: usize) {
    if entry == 0 {
        return;
    }
    let registry = jit_code_ranges();
    // A range for `entry` can only be registered by a body that owns the
    // buffer at `entry`, and that body is the one being dropped. So no
    // registration of `entry` can race this check.
    if !registry.snapshot.load().starts_at(entry) {
        return;
    }
    let _writer = registry.writer.lock().unwrap_or_else(|e| e.into_inner());
    let current = registry.snapshot.load();
    if !current.starts_at(entry) {
        return;
    }
    // EVERY live range starting at `entry` goes, in either half: the same body
    // can be registered twice (a test harness does, beside `put`), and a merge
    // in between leaves one copy in each half. A tombstoned `base` entry is
    // already gone, and is not the same thing as a live `recent` range at the
    // same start (the allocator reused the address) — that one is removed
    // here like any other. `starts_at` above guarantees one half holds a live
    // range.
    let recent: Vec<JitCodeRange> = if current.recent_starts_at(entry) {
        current
            .recent
            .iter()
            .filter(|range| range.0 != entry)
            .cloned()
            .collect()
    } else {
        current.recent.clone()
    };
    let next = if !current.live_base_starts_at(entry) {
        JitCodeRangeSet {
            base: Arc::clone(&current.base),
            recent,
            removed: current.removed.clone(),
        }
    } else if current.removed.len() < recent_code_range_capacity(current.base.len()) {
        // Tombstone it: O(sqrt n), where rebuilding `base` was O(n) plus a
        // `Weak` clone per entry. See `JitCodeRangeSet`.
        let mut removed = current.removed.clone();
        let at = removed.partition_point(|start| *start < entry);
        removed.insert(at, entry);
        JitCodeRangeSet {
            base: Arc::clone(&current.base),
            recent,
            removed,
        }
    } else {
        // Tombstone list full: rebuild `base`, merging `recent` in and dropping
        // every tombstoned entry on the way (which is free). `merged_without`
        // skips every live range starting at `entry`, in both halves.
        JitCodeRangeSet {
            base: Arc::new(current.merged_without(Some(entry))),
            recent: Vec::new(),
            removed: Vec::new(),
        }
    };
    registry.snapshot.store(Arc::new(next));
    JIT_CODE_RANGES_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Number of registered code ranges (Stage 5 diagnostic).
pub fn jit_code_range_count() -> usize {
    jit_code_ranges().snapshot.load().len()
}

/// BUG-03 — whether the cross-thread STW JIT root scan is enabled (env
/// `CRATONVM_XT_JIT_ROOT_SCAN`). Mirrors `cratonvm_vm::jit::xt_root_scan::
/// enabled` (the jit crate cannot depend on the vm crate); kept in sync via
/// the same env var. Cached on first read.
///
/// A4 (fork6-fjp) — default ON (opt-OUT), matching the vm-side gate. The two
/// sides had diverged when the vm side flipped to default-on: this mirror
/// stayed opt-IN, so `CompiledMethodCache::put` never registered JIT code
/// ranges in a default-env process, `jit_code_ranges_snapshot()` was always
/// empty, and the "default-on" takeover classified every suspended peer as
/// "not in JIT" — the whole cross-thread STW JIT root scan (and the
/// helper-window pass) was silently inert unless the env var was set to `1`
/// explicitly. Keep the polarity identical to `xt_root_scan::enabled`.
pub fn xt_jit_root_scan_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        true
    })
}

/// Snapshot the registered JIT code ranges as `(entry, end)` pairs.
///
/// BUG-03 — the cross-thread STW JIT root scan must classify each *suspended*
/// peer's `Rip` as in-JIT-or-not without anything that can block on a lock a
/// frozen peer holds. (Written when `lookup_jit_code_range` took the ranges
/// lock; both it and this now read the lock-free `arc_swap` snapshot, but this
/// COPY allocates, which is why the collector takes it ONCE before suspending
/// anyone.) It then classifies every frozen peer against the returned copy
/// with a lock-free range check. Ranges are added as bodies are
/// published and withdrawn when a body's last owner drops, before its buffer
/// is unmapped.
/// A momentarily-stale
/// snapshot can only mis-classify a brand-new range as "not JIT" (handled
/// conservatively by the snapshot-based mitigation), never the reverse.
pub fn jit_code_ranges_snapshot() -> Vec<(usize, usize)> {
    let ranges = jit_code_ranges().snapshot.load();
    let mut out = Vec::with_capacity(ranges.len());
    ranges.for_each_sorted(|(e, end, _, _)| out.push((*e, *end)));
    out
}

/// Resolve the `CompiledMethod` pointer whose code range contains `addr`, or
/// `None`. The immutable range table is sorted at publication, so this performs
/// a lock-free binary search. Stage 5.
pub fn lookup_jit_code_range(addr: usize) -> Option<usize> {
    let ranges = jit_code_ranges().snapshot.load();
    ranges.covering(addr).map(|(_, _, cm, _)| *cm)
}

/// The registered body's existing diagnostic label, with its owner retained
/// while the label is copied. Unlike the optional method-name census this
/// works with `CRATONVM_DBG_JIT_NAMES` disabled and adds no publication work.
///
/// For normal-context diagnostics only: copying the string allocates. The
/// allocation-free Unix signal handler must not call this function. Synthetic
/// ranges without a weak owner, withdrawn bodies, and unnamed artifacts return
/// `None`; no bare `CompiledMethod` pointer is dereferenced and no fallback
/// owner-table lock is taken.
#[cold]
pub fn lookup_jit_code_owner_label(addr: usize) -> Option<String> {
    let body = {
        let ranges = jit_code_ranges().snapshot.load();
        let (_, _, _, owner) = ranges.covering(addr)?;
        owner.upgrade()?
    };
    (!body.method_label.is_empty()).then(|| body.method_label.clone())
}

/// One snapshot of the registered JIT code ranges, held for a whole stack
/// walk (JIT round 16 wave 15, lane throwfill).
///
/// [`lookup_jit_code_range`] loads the `arc_swap` snapshot, searches it and
/// drops the guard for EVERY return address; a throwable's compiled-frame walk
/// asks once per activation, so a 24-deep compiled stack paid 24 loads and 24
/// releases. A view loads once and answers every address against that
/// snapshot with the same search (`JitCodeRangeSet::covering`).
///
/// The answers are [`lookup_jit_code_range`]'s for every address a walk of
/// the CALLING thread's own stack asks about: such an address is a return
/// address into a body with a live frame on this thread, which was registered
/// before that frame was entered (so before the view was taken) and cannot be
/// unregistered while the frame is live (the JIT cache owns the body until its
/// frames are gone). A range registered or withdrawn after the view was taken
/// belongs to no frame of this walk. Do not keep a view across a call that can
/// run Java or publish code; it is a walk-scoped value.
pub struct JitCodeRangeView {
    pub(crate) ranges: arc_swap::Guard<std::sync::Arc<JitCodeRangeSet>>,
}

impl JitCodeRangeView {
    /// [`lookup_jit_code_range`] against this view's snapshot.
    #[inline]
    pub fn lookup(&self, addr: usize) -> Option<usize> {
        self.ranges.covering(addr).map(|(_, _, cm, _)| *cm)
    }
}

/// Take a [`JitCodeRangeView`]: one `arc_swap` load.
pub fn jit_code_range_view() -> JitCodeRangeView {
    JitCodeRangeView {
        ranges: jit_code_ranges().snapshot.load(),
    }
}

/// `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR=0` -- stop rewriting an elidable
/// `invokespecial C.<init>()V` site's callee class to `java/lang/Object`.
///
/// The rewrite is what lets the single-pass `0xb7` codegen drop the per-object
/// `jit_invoke_dispatch` for a constructor whose whole body is
/// `aload_0; invokespecial Object.<init>; return`. It also DISCARDS the site's
/// real callee identity, which every consumer downstream of the rewrite then
/// sees as `Object.<init>` -- so this is the bisect lever for anything that
/// appears only when an elidable constructor is in the method.
///
/// Default ON. `CRATONVM_DBG_JIT_ELIDE_CTOR=<caller substring>` names each
/// rewrite as it happens.
pub fn elide_trivial_ctor_enabled() -> bool {
    true
}

/// `CRATONVM_DBG_JIT_DIRECT_BINDS=<caller-method substring>` -- print every raw
/// JIT-to-JIT direct call this compile BAKES: the caller, the bytecode pc, the
/// callee triple the site resolved to, and the entry address bound.
///
/// A baked `CALL` carries no identity at runtime -- `JitCalleeGuard` is only
/// constructed by `jit_invoke_dispatch`, which a raw call bypasses -- so the
/// punned-store report names the door and not the site. This is the same
/// question asked where the answer is still attributable, and it is the one
/// place that sees both the pc and the address.
///
/// Cross-check against `CRATONVM_DBG=jitc`'s
/// `full-compile <method> entry=0x...`: a bind whose entry belongs to a method
/// other than the one named here is a site bound to the wrong body.
pub fn dbg_direct_binds() -> Option<&'static str> {
    static F: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    F.get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok())
        .as_deref()
}

/// Emit one `[jit-direct-bind]` line when the CALLER matches the filter.
pub fn note_direct_bind(
    caller: &str,
    pc: usize,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    entry: usize,
) {
    let Some(filter) = dbg_direct_binds() else {
        return;
    };
    if !caller.contains(filter) {
        return;
    }
    eprintln!(
        "[jit-direct-bind] caller={caller} pc={pc} callee={callee_class}.{callee_method}{callee_desc} entry={entry:#x}"
    );
}

/// DIAGNOSTIC: name the compiled body whose code range contains `addr`.
///
/// [`pin_jit_code_range_owner`] cannot answer for a body registered through the
/// non-owning [`register_jit_code_range`] — its `Weak` never upgrades. Both
/// production publication sites (`JitCache::put` / `put_osr`) now register the
/// OWNED form, so that is only the synthetic ranges tests install; but a
/// diagnostic must not depend on which door registered the range, and a pin
/// would also RETAIN the body it names, which a crash-path diagnostic has no
/// business doing. This reads the method key straight off the artifact the
/// range table already points at.
///
/// Only for reports taken from INSIDE the body (a helper called by it): the
/// range table's `cm` is the address of a live `CompiledMethod` for exactly as
/// long as that body can be executing, which is the only window this is called
/// in. Do not call it about an arbitrary address.
pub fn jit_code_range_method_key(addr: usize) -> Option<String> {
    let cm = lookup_jit_code_range(addr)?;
    // SAFETY: see the contract above — `cm` is the address of the
    // `CompiledMethod` whose code is executing at `addr`.
    let compiled: &CompiledMethod = unsafe { &*(cm as *const CompiledMethod) };
    compiled.method_key_for_report()
}

impl CompiledMethod {
    /// The method key this artifact's deopt metadata names (the outermost scope
    /// of an inlined chain), for reports; `None` when it carries no deopt point.
    pub(crate) fn method_key_for_report(&self) -> Option<String> {
        self.deopt_points
            .iter()
            .map(|d| d.frame_state.method_key.clone())
            .find(|k| !k.is_empty())
            .or_else(|| {
                self._deopt_point_boxes
                    .iter()
                    // The compiling method is the OUTERMOST scope of an inlined
                    // chain (round 13 wave 3); a flat frame is its own outermost.
                    .map(|d| deopt::outermost_scope(&d.frame_state).method_key.clone())
                    .find(|k| !k.is_empty())
            })
    }
}

/// Resolve `addr` to a *retained* owner of the compiled body containing it.
///
/// [`lookup_jit_code_range`] returns a bare `usize` — the address of an
/// `Arc<CompiledMethod>`'s inner value with no keep-alive. Between that call
/// returning and the caller dereferencing it, the last strong reference can be
/// dropped and the artifact (its oop maps, its deopt tables, its inline-cache
/// slot boxes, and its executable buffer) freed. Today that window is closed
/// only indirectly, by `defer_jit_owner` declining to reclaim while any thread
/// is inside compiled code — which holds for a root scan of a *running* JIT
/// frame but is not an invariant the signature states or a caller can check.
///
/// This is the same lookup with the keep-alive attached: the returned `Arc`
/// makes the metadata and the code buffer valid for as long as it is held.
/// `None` means "no live body covers this address", which is the safe answer.
///
/// Costs one `Weak::upgrade` — an atomic increment — over the lock-free range
/// search, so it is affordable on the per-dispatch path that calls a raw entry,
/// not just on the per-frame ones. It is NOT for the per-stack-word
/// conservative sweep; that one wants [`snapshot_code_ranges_into`], which
/// hands out no owner at all.
pub fn pin_jit_code_range_owner(addr: usize) -> Option<Arc<CompiledMethod>> {
    let entry = {
        let ranges = jit_code_ranges().snapshot.load();
        let (entry, _, _, owner) = ranges.covering(addr)?;
        // Fast path: the range was registered by `put`/`put_osr`, which carry
        // the owner. A live body upgrades with one atomic increment.
        if let Some(pinned) = owner.upgrade() {
            return Some(pinned);
        }
        *entry
    };
    // Either the body is gone (the honest `None`), or this range was registered
    // without an owner — the synthetic ranges unit tests install. Both are rare
    // enough to pay `jit_entry_owners`'s lock.
    resolve_jit_entry_owner(entry)
}

/// The live body that owns the deopt point at `point_addr`
/// ([`CompiledMethod::owns_deopt_point`]), pinned, found through the
/// code-range registry: every registered range whose owner still upgrades is
/// asked. Interpreter round i2 wave 59, lane L2
/// (`docs/feature-designs/profile-guided-inlining.md`).
///
/// For a sink that must name the trapping artifact of a stashed chain frame
/// after the JIT cache stopped publishing it: a body withdrawn by a
/// redefinition or an interpreter-only request is no longer what
/// `JitCache::get` answers, yet a running activation of it -- a callee a
/// compiled caller still pins through its direct-callee keep-alive -- leaves
/// through a forced post-call chain exit and stashes a point it owns. The
/// registry keeps such a body's range until it is reclaimed, so the walk
/// finds it; a body already dropped answers `None` (its point can then be
/// vouched for by nothing). Linear in the registered ranges, each asked
/// with one weak upgrade: cold, only on a chain trap whose body the cache no
/// longer holds.
#[cold]
#[inline(never)]
pub fn pin_jit_body_owning_deopt_point(point_addr: usize) -> Option<Arc<CompiledMethod>> {
    if point_addr == 0 {
        return None;
    }
    let ranges = jit_code_ranges().snapshot.load();
    let mut found: Option<Arc<CompiledMethod>> = None;
    ranges.for_each_sorted(|(_, _, _, owner)| {
        if found.is_some() {
            return;
        }
        if let Some(body) = owner.upgrade() {
            if body.owns_deopt_point(point_addr) {
                found = Some(body);
            }
        }
    });
    found
}

/// Widen the envelope of every address an executable buffer ever covered
/// (`RECLAMATION.mapped_lo/hi`), for [`pin_jit_code_range_owner_checked`]'s
/// lock-free `NotJit` answer (round 14 wave 2, lane codecache). Called by the
/// two sites that register a mapping in the PROCESS code-region table
/// (`ExecutableBuffer::new`, the arena's `new_in`), not by
/// `JitCodeRegion::register`, which unit tests also drive with fake addresses.
/// Monotonic: a freed buffer stays inside it.
pub(crate) fn note_code_mapping(start: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    RECLAMATION.mapped_lo.fetch_min(start, Relaxed);
    RECLAMATION
        .mapped_hi
        .fetch_max(start.saturating_add(len), Relaxed);
}

/// [`pin_jit_code_range_owner`]'s answer with its `None` split in two (round 14
/// wave 1, lane codecache, `r14w1-codecache-unpinned-raw-entry-call-patch`).
pub enum JitEntryPin {
    /// A live owner of the compiled body at the address, held.
    Live(Arc<CompiledMethod>),
    /// Compiled code whose owner is gone: a registered range with a dead
    /// owner, an orphaned mapping waiting in the retirement queue, or a buffer
    /// this process unmapped and nothing has mapped since. Must not be entered.
    Dead,
    /// Not (known) compiled code: a native trampoline, a Rust function.
    NotJit,
}

/// Resolve `addr` like [`pin_jit_code_range_owner`], and on a miss say whether
/// the address is compiled code that lost its owner ([`JitEntryPin::Dead`]) or
/// not compiled code at all ([`JitEntryPin::NotJit`]).
///
/// Cost by case (round 14 wave 2, lane codecache). Every executable buffer
/// this crate maps is an `ExecutableBuffer`, registered in the code-region
/// table for its whole mapped life, so an address is one of:
///
/// * inside a registered range: the lock-free range snapshot answers (a dead
///   owner still pays `jit_entry_owners`' lock, as `pin_jit_code_range_owner`);
/// * outside the envelope of every buffer ever registered
///   (`RECLAMATION.mapped_lo/hi`, monotonic, so freed buffers stay inside it):
///   the common `NotJit` case (a Rust function), answered by two relaxed loads,
///   with no lock and no ring walk;
/// * inside the envelope and a LIVE registered buffer (a lambda-adapter thunk,
///   an orphaned mapping, an OSR trampoline): `validate_code_ptr`, then the
///   owner map, then -- only while bodies are queued -- the queue;
/// * inside the envelope but in no live buffer (a native trampoline mapped
///   between JIT buffers, or a freed body): `validate_code_ptr`'s locked miss
///   and the freed-buffer ring.
pub fn pin_jit_code_range_owner_checked(addr: usize) -> JitEntryPin {
    use std::sync::atomic::Ordering::Relaxed;
    let covered = {
        let ranges = jit_code_ranges().snapshot.load();
        match ranges.covering(addr) {
            Some((entry, _, _, owner)) => {
                if let Some(pinned) = owner.upgrade() {
                    return JitEntryPin::Live(pinned);
                }
                Some(*entry)
            }
            None => None,
        }
    };
    if let Some(entry) = covered {
        return match resolve_jit_entry_owner(entry) {
            Some(pinned) => JitEntryPin::Live(pinned),
            None => JitEntryPin::Dead,
        };
    }
    if addr < RECLAMATION.mapped_lo.load(Relaxed) || addr >= RECLAMATION.mapped_hi.load(Relaxed) {
        // Never inside any executable buffer this process mapped.
        return JitEntryPin::NotJit;
    }
    if validate_code_ptr(addr as *const u8).is_ok() {
        // A live registered buffer outside every range. Adapter thunks own
        // themselves through `jit_entry_owners`.
        if let Some(pinned) = resolve_jit_entry_owner(addr) {
            return JitEntryPin::Live(pinned);
        }
        // An orphaned mapping (its last owner dropped outside the queue) is
        // still mapped and registered, but its side tables are gone.
        if RECLAMATION.queued_owners.load(Relaxed) != 0
            && lock_retire_queue()
                .iter()
                .any(|body| body.buffer_start == addr)
        {
            return JitEntryPin::Dead;
        }
        // An OSR trampoline or another ownerless stub: callable, as before.
        return JitEntryPin::NotJit;
    }
    // In no live JIT buffer, but inside the envelope. A compiled entry is its
    // buffer's FIRST byte, so only an address equal to a freed buffer's base
    // is a dead entry. The edge: a mapping made by code
    // outside this crate (a libffi closure page, for one) that the OS placed
    // exactly at a recently freed buffer's base reads `Dead` until that ring
    // slot is overwritten (4096 frees later). That costs such a call its
    // interpreter bail (a correct, slower answer), never a wrong call; this
    // crate's own trampolines are `ExecutableBuffer`s and take the live-region
    // arm above instead.
    if recent_code_free_covering(addr).is_some_and(|(base, ..)| base == addr) {
        return JitEntryPin::Dead;
    }
    JitEntryPin::NotJit
}

/// Copy the registered code ranges into `buf` as `(start, end)` pairs sorted by
/// `start`, taking the table lock exactly ONCE.
///
/// PERF (TC0622 startup): the GC's per-native-call native-stack scan
/// (`native_stack_has_jit_frame`) tests up to ~1M stack words against the code
/// ranges. Calling `lookup_jit_code_range` per word locked this `Mutex` a
/// million times per scan — ~66% of all CPU during a Tomcat `start()`. Callers
/// snapshot once into a reusable buffer and binary-search it lock-free instead;
/// the result is identical because the ranges are disjoint. The lock is NOT held
/// across the (long) word scan, so it never blocks the background compiler's
/// `register_jit_code_range`.
pub fn snapshot_code_ranges_into(buf: &mut Vec<(usize, usize)>) {
    buf.clear();
    let ranges = jit_code_ranges().snapshot.load();
    buf.reserve(ranges.len());
    ranges.for_each_sorted(|(e, end, _, _)| buf.push((*e, *end)));
}

/// DBG (spring-bug-11): code-range → method-name table for naming a JIT frame in
/// a crash report. Populated by `JitCache::put` ONLY when `CRATONVM_DBG_JIT_NAMES`
/// is set (so the default path keeps zero overhead and no unbounded growth).
pub(crate) static JIT_NAME_RANGES: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, usize, String)>>> =
    std::sync::OnceLock::new();

pub(crate) fn jit_name_ranges() -> &'static std::sync::Mutex<Vec<(usize, usize, String)>> {
    JIT_NAME_RANGES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Whether to record JIT method-name ranges (`CRATONVM_DBG_JIT_NAMES`). Cached.
pub fn jit_names_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// Verdict bit of the safepoint slow path (`HelperFnSafepointSlowPath`): the
/// thread's INNERMOST interpreter frame must run interpreted now. Only an OSR
/// body runs inside that frame's activation, so only an OSR body's back-edge
/// poll acts on it (`x64::safepoint::emit_safepoint_poll_leaving_to`, which
/// tests this bit alone there since wave 17, as the optimizing tier's OSR
/// poll does, `ir_lower::Lowerer::emit_poll_mode_exit`; the whole register
/// before). Interpreter round i1 wave 12.
pub const SAFEPOINT_VERDICT_INNERMOST_FRAME: i64 = 1;

/// Verdict bit of the safepoint slow path: the compiled body that POLLED must
/// run interpreted now. A method-entry body's back-edge and self-tail polls
/// test this bit alone (`x64::safepoint::emit_safepoint_poll_leaving_to`;
/// since wave 18 an optimizing one's unconditional back-edge polls too,
/// `ir_lower::IrPollModeExits::MethodEntry`).
///
/// When the poll names its body (the helper's second argument, the body's
/// compile id, confirmed against the thread's compile-id mirror), the answer
/// is about that body's method: a JVMTI interpreter-only event, or a JDWP
/// request concerning the method (a breakpoint in it included), while the
/// method's class has not been redefined. An unnamed body gets the answer
/// that is about every compiled frame (a JVMTI interpreter-only event, or a
/// JDWP request concerning every method, while no class of the process has
/// been redefined), always together with
/// [`SAFEPOINT_VERDICT_INNERMOST_FRAME`]. Interpreter round i1 wave 15, lane
/// L3 (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, then named
/// `SAFEPOINT_VERDICT_EVERY_FRAME`); per body since wave 17, lane L1
/// (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`).
pub const SAFEPOINT_VERDICT_POLLING_BODY: i64 = 2;

/// Verdict bit of the safepoint slow path: the compiled body the poll NAMED
/// (a compile id the VM confirmed and looked up) must run interpreted now,
/// and no compiled caller has bound that body for a caller-held synchronized
/// `CALL` (`CompiledMethod::is_bound_by_a_caller_held_site`). Unlike
/// [`SAFEPOINT_VERDICT_POLLING_BODY`] it is never set for a body the poll did
/// not name (the VM-wide answer), so a body whose binding is unknown is never
/// told to leave on it.
///
/// Only the back-edge polls of an optimizing METHOD-ENTRY body of an
/// `ACC_SYNCHRONIZED` method whose graph can neither trap nor call test it
/// (`ir_lower::IrPollModeExits::MethodEntryNamedOnly`, under
/// `CRATONVM_JIT_IR_SYNC_LEAF_NAMED_POLL_EXITS`): the one body shape a
/// caller-held sync-direct site binds, whose cold side treats every sentinel
/// as the callee's exception. An unbound such body leaves like every other
/// synchronized method-entry body; a bound one keeps a verdict-blind poll.
/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of
/// `docs/feature-designs/jit-ir-optimizations.md`).
pub const SAFEPOINT_VERDICT_NAMED_BODY: i64 = 4;

/// Tag bit of the safepoint slow path's SECOND argument (the body id): the
/// caller is a forced post-call exit site, not a poll
/// (`CompiledMethod::post_call_exit_sites`; interpreter round i1 wave 27,
/// lane L6). The helper then answers the verdict for the body the low 32
/// bits name WITHOUT parking: the site's out-of-line stub preserves every
/// register instead of publishing the frame's references, so the thread must
/// not stop for a collection there (the VM's `jit_safepoint_slow_path_body`);
/// it parks at the body's next poll, or leaves through its exit. A compile id
/// is a `u32`, so the bit never collides with one.
pub const POST_CALL_EXIT_VERDICT_ONLY: i64 = 1 << 62;

/// deopt-osr: master gate for *real* deopt-exit / OSR-exit resume
/// (`CRATONVM_DEOPT_REAL`, **default-ON** as of the 2026-06-22 flip). Read-once
/// cached. When ON, a guard/loop bail reconstructs a precise interpreter frame
/// at the trapping bci and RESUMES there (avoiding the `i64::MIN` whole-method
/// re-run and its side-effect double-execution). The interpreter deopt sinks and
/// the OSR-exit sink consult this together with the per-method
/// `can_deopt_resume` / `can_osr_exit` flags, so deopt-exit and OSR-exit can
/// never run half-on (see `docs/feature-designs/deopt-osr.md`).
///
/// **Opt out** with `CRATONVM_DEOPT_REAL=0` (also `false`/`off`/`no`) to restore
/// the whole-method re-run — the safety net. The flip rides on the
/// correctness-verified non-moving young-gen + conservative-OSR-backstop GC
/// foundation (precise-jit-maps default-on); it does NOT enable the OSR-exit
/// in-place transfer (`CRATONVM_OSR_EXIT_TRANSFER`), which stays default-off —
/// so OSR-exit uses the safe reject path.
pub fn deopt_real_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            // Explicit opt-out values disable; any other value (and unset) → ON.
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        },
    )
}

/// Let receiver-type speculation consult the per-bci de-spec registry
/// (`CRATONVM_JIT_RECEIVER_DESPEC`, **default-ON; `=0` restores the old
/// route**). Read-once cached.
///
/// With this off -- the state this VM shipped in until 2026-08-24 -- a
/// call-site intrinsic guarded on one receiver class (`guard_class_id != 0`:
/// the `java/lang/CharSequence` String family, CRC32, the atomic-field family)
/// is re-emitted with the identical guard on every recompile, even at a bci the
/// de-spec registry has already recorded as a proven-failing speculation. A
/// site whose receiver is never that class therefore deopts on every call until
/// `recommend_action` bars the method from compilation for good.
///
/// With it on, such a bci declines the intrinsic and takes the ordinary
/// MIC/PIC dispatch, and the `MakeNotCompilable` escalation is withheld from
/// the method (`DeoptimizationLog::recommend_action_at_bci`). The engagement
/// counter is `[cratonvm] receiver despec: sites-declined=` under
/// `CRATONVM_DBG=jit-method-stats` -- a zero there means this changed nothing,
/// whatever the clock says.
///
/// `docs/feature-designs/jit-local-exception-handlers.md` covers the class it was written for: 1.67
/// `ReceiverTypeChanged` deopts per loop iteration on a `CharSequence.length()`
/// reached only with `AsciiString`.
pub fn receiver_despec_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

/// Enter a compiled method's OWN `catch` block from compiled code
/// (`CRATONVM_JIT_LOCAL_HANDLERS`, **default-ON; `=0` restores the old
/// route**). Read-once cached.
///
/// With this off — the state this VM shipped in until 2026-08-20 — no `catch`
/// block anywhere runs in compiled code. A caught exception leaves the
/// artifact: reason-9 deopt, exceptional-frame reconstruction, an interpreted
/// handler, and inside an OSR'd loop a re-entry at the next hot back edge.
/// `probes/OsrExcRateProbe.java` prices that round trip at ~2 900 ns per catch
/// against HotSpot's 6.7-16.
///
/// With it on, a throwing site inside one of this method's own protected
/// ranges branches to a stub that asks
/// [`JitLocalHandlerSite`] which handler applies and jumps straight into the
/// compiled handler block, staying in the same frame — so the locals need no
/// reconstruction because they were never left behind.
///
/// It shipped OFF for one day, which was long enough to measure it. What the
/// flip rests on:
///
/// * `probes/OsrExcRateProbe`, one binary, two interleaved rounds: **1246-1377
///   -> 60-120 ns per caught exception**, ~12x, with `sink` and `caught`
///   byte-identical in every arm (HotSpot on the same host: 4.6-8);
/// * `probes/LocalHandlerShapeProbe`, eight handler shapes including two typed
///   handlers over one range, a strict-subclass catch, a propagating throw, a
///   `finally`, a nested `try`, a rethrowing handler and a locals-survive
///   arm: the same digest under HotSpot, under `--nojit`, and with this off
///   and on;
/// * the whole netty `codec-http` suite, 103 classes: **identical result
///   sets**;
/// * `regression-suite/run.sh`, 64 vectors: identical, 63 pass and the one
///   pre-existing `RImmutableFactoryTypes` failure that is also there on
///   unmodified `dev`.
///
/// `=0` (also `false`/`off`/`no`) restores the old route, so one binary still
/// has two arms — which is how the two OSR-admission defects this feature
/// exposed were found. `docs/feature-designs/jit-local-exception-handlers.md` covers the exception/OSR path it was written for.
///
/// The prose above sat on [`receiver_despec_enabled`] until r10 (lane `drv`):
/// the two doc blocks had been concatenated, so this function was undocumented
/// and the next one carried two default-ON stories, one of them about a flag it
/// does not read.
pub fn local_handlers_enabled() -> bool {
    local_handler_flags().0
}

/// Let an IMPLICIT exception raised by compiled code -- a bounds check, an
/// inline null check or a zero-divisor guard -- enter this method's own
/// compiled `catch` block, instead of returning the deopt sentinel and having
/// the interpreter build the throwable, find the handler and run it there.
///
/// **Default ON**; `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS=0` is the kill switch
/// and restores the pre-2026-09-20 route byte for byte on the miss edge.
///
/// This is the second half of [`local_handlers_enabled`]. The first half
/// (2026-08-20) covered the fallible sites that hand back a *throwable*: a
/// dispatched callee's exception and an allocation's `OutOfMemoryError`. The
/// three implicit traps were left out because each of them signals a REQUEST
/// for a throwable rather than a throwable, and nothing in compiled code could
/// grant one. `jit_local_handler_lookup` can now materialise the request
/// through the same body the interpreter's drain uses, so the request is no
/// longer a reason to leave the frame.
///
/// Why it matters:
/// `docs/feature-designs/jit-ir-optimizations.md`
/// measures the round trip a caught implicit exception pays -- exception-exit
/// TRANSFER, an interpreted handler and, inside an OSR'd loop, a fresh OSR
/// entry at the next back edge -- at about 1.4 us of a 3.0-3.3 us total,
/// against HotSpot's 0.4-0.7 us for the whole thing.
pub fn implicit_local_handlers_enabled() -> bool {
    local_handler_flags().1
}

/// `(local_handlers_enabled, implicit_local_handlers_enabled)`, read from the
/// environment once.
///
/// The two flags share one cell rather than one each because the second is a
/// sub-switch of the first -- it cannot be on with the first off, so the pair
/// is one decision -- and because `jit/tests/process_global_statics_ratchet.rs`
/// counts `static` declaration lines and a second `OnceLock` here would be a
/// second one for no gain. Reading the environment uncached is not an option:
/// [`implicit_local_handlers_enabled`] is consulted on the throw path in
/// `jit_local_handler_lookup`, which is the path this flag exists to make
/// cheap.
pub(crate) fn local_handler_flags() -> (bool, bool) {
    static CACHE: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        fn on(key: &str) -> bool {
            match cratonvm_types::flags::runtime_var(key) {
                Ok(v) => !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                ),
                Err(_) => true,
            }
        }
        let base = on("CRATONVM_JIT_LOCAL_HANDLERS");
        (base, base && on("CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS"))
    })
}

thread_local! {
    /// Set for the duration of one compile that armed local handlers, so the
    /// compile door can tell "this method refused for some unrelated reason"
    /// from "this method refused with handler bodies newly in the image".
    pub(crate) static LOCAL_HANDLERS_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Did the compile that just ran on this thread put handler bodies in the
/// image? Read by the compile door to decide whether a refusal is worth
/// retrying without them.
pub fn local_handlers_were_armed() -> bool {
    LOCAL_HANDLERS_ARMED.with(std::cell::Cell::get)
}

/// Record whether the compile now starting armed local handlers.
pub fn note_local_handlers_armed(armed: bool) {
    LOCAL_HANDLERS_ARMED.with(|c| c.set(armed));
}

/// Clears the local-handlers-armed flag on creation and again on drop, so the
/// flag the compile door reads always belongs to the compile inside the scope,
/// however that compile returned: normally, early, or by unwinding.
pub(crate) struct LocalHandlersArmedScope(pub(crate) ());

impl LocalHandlersArmedScope {
    pub(crate) fn enter() -> Self {
        note_local_handlers_armed(false);
        LocalHandlersArmedScope(())
    }
}

impl Drop for LocalHandlersArmedScope {
    fn drop(&mut self) {
        note_local_handlers_armed(false);
    }
}


/// activate-ir-optimizer Front 3.2: guard-surviving scalar replacement
/// (`CRATONVM_SCALAR_DEOPT`, read-once).
///
/// **SUPERSEDED 2026-09-12.** It used to be the opt-in half of the gate that
/// let the IR lowerer emit a `FrameValue::VirtualObject` for a scalar-replaced
/// object live at a deopt point. That recipe is now emitted whenever precise
/// resume is on — see [`scalar_deopt_descriptor_available`] and
/// `allocation-elision-never-fires-by-default-FIXED-20260912.md` — so setting
/// this flag changes nothing. It is still declared and still read (by that
/// function) so the flag registries and a run that sets it stay consistent.
pub fn scalar_deopt_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// Will a scalar-replaced object that a REAL deopt point names get a
/// `FrameValue::VirtualObject` recipe (so its allocation may be removed)?
///
/// Exactly [`deopt_real_enabled`] — the precise-resume switch, default ON. The
/// recipe is only consumed by a precise resume, so it is emitted exactly when
/// one can happen. With precise resume OFF the answer is `false`, and the rule
/// `plan_scalar_replacement` applies is the conservative one: an allocation a
/// consultable deopt snapshot names KEEPS its allocation. It never relies on the
/// whole-method re-run.
///
/// The one place the rule and the recipe can still disagree — the lowerer
/// failing to describe an elided object at a point it did elide for
/// (`MaterializationRequired`) — is caught after lowering by
/// [`ir_artifact_names_an_undescribable_elided_object`].
///
/// This was `scalar_deopt_enabled() && deopt_real_enabled()`;
/// `CRATONVM_SCALAR_DEOPT` is read here only so it keeps its read site.
pub(crate) fn scalar_deopt_descriptor_available() -> bool {
    let _superseded = scalar_deopt_enabled();
    deopt_real_enabled()
}

/// deopt-osr: CI/test gate for the eager-deopt differential verifier
/// (`CRATONVM_DEOPT_VERIFY`, default-OFF). Read-once cached. When ON, every
/// eligible guard/loop boundary deopts, reconstructs the interpreter frame, and
/// compares the reconstructed-interpreter result against the JIT result — the
/// mandatory check before any guard/loop family is flipped onto `deopt_real`.
pub fn deopt_verify_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// deopt-osr: the through-JIT BCE-deopt differential trigger (`CRATONVM_DEOPT_EAGER`,
/// default-OFF, read-once). When ON *and* `deopt_real_enabled()`, the single-pass
/// backend emits one synthetic UNCONDITIONAL branch to the BCE-guard deopt stub
/// (reason 2) right after the loop-header snapshot, so a JIT'd speculative-BCE loop
/// deopts at the loop bci *even when the bounds check passes* — reconstructing the
/// loop-header frame (its locals, incl. `long`/`double`/`float` accumulators) and
/// resuming in the interpreter. This is the long-missing end-to-end exercise of the
/// deopt-EXIT resume: run a program with the gate vs without and compare outputs (a
/// SEPARATE-PROCESS differential — the read-once gates can't diff in-process); equal
/// outputs prove the reconstruction + resume are correct. OFF ⇒ no branch ⇒
/// byte-identical production code.
pub fn deopt_eager_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// Phase B (real-frame-deopt x64 backport) e2e trigger: `CRATONVM_DEOPT_EAGER_BCI=<n>`
/// (default unset, read-once). When set *and* `deopt_real_enabled()`, the eager
/// deopt-EXIT branch fires at the chosen bytecode bci `n` instead of the first
/// loop header. Loop headers can never hold a live scalar-replaced object (it
/// would escape across the back-edge), so this points the trigger at a
/// straight-line bci where a scalar object IS live in a local — the only way to
/// exercise the `VirtualObject` materialization path through the live JIT. A
/// SEPARATE-PROCESS differential, like `CRATONVM_DEOPT_EAGER`. Unset ⇒ unchanged.
pub fn deopt_eager_bci_override() -> Option<usize> {
    static CACHE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
    })
}

/// deopt-osr Step 8 (test trigger): `CRATONVM_OSR_EXIT_TEST` (default-OFF,
/// read-once). When ON *and* `deopt_real_enabled()`, the single-pass backend
/// emits one synthetic unconditional OSR-exit branch at a loop header so a JIT'd
/// loop bails to the interpreter at a loop bci and resumes the loop body — the
/// deliberate "instrument a rare branch" trigger that exercises the OSR-exit
/// resume end-to-end pending a real speculation trigger. OFF ⇒ no trigger
/// emitted ⇒ byte-identical code (the production path).
pub fn osr_exit_test_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// deopt-osr Step 8 follow-up (P4): `CRATONVM_OSR_EXIT_AFTER=N` (default-OFF,
/// read-once) — the COUNTER-GATED OSR-exit trigger. When set to a positive `N`
/// *and* `deopt_real_enabled()`, the single-pass backend emits, at a loop header,
/// a per-site counter that bails to the OSR-exit stub only on the `N`-th reach —
/// so the JIT runs ~`N` loop iterations (advancing the loop-carried locals /
/// accumulator and committing their side effects) BEFORE the exit. This makes the
/// reconstructed frame carry genuinely JIT-advanced state, which the interpreter's
/// `transfer_osr_exit_into_live_frame` (gated `CRATONVM_OSR_EXIT_TRANSFER`) writes
/// back into the live frame — the realistic trigger the handoff calls for, vs the
/// unconditional-at-header `CRATONVM_OSR_EXIT_TEST` trigger that bails at iteration
/// 0. `None` / 0 ⇒ no counter emitted ⇒ byte-identical production code. Test-only:
/// run it WITH `CRATONVM_OSR_EXIT_TRANSFER=1`; with the transfer gate off the
/// interpreter safe-rejects and re-runs the committed iterations (double-executing
/// their side effects) — which is exactly the gap the transfer closes.
pub fn osr_exit_after() -> Option<usize> {
    static CACHE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
    })
}

/// Record `[entry, entry+len)` → `name` for crash-time symbolization. No-op
/// unless `CRATONVM_DBG_JIT_NAMES` is set.
pub fn register_jit_method_name(entry: usize, len: usize, name: String) {
    if entry == 0 || len == 0 {
        return;
    }
    jit_name_ranges()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((entry, entry + len, name));
}

/// Withdraw every name range starting at `entry`.
///
/// Called from [`CompiledMethod::drop`], BEFORE the buffer is unmapped, for the
/// same reason [`unregister_jit_code_range`] is. Without it the table both grows
/// without bound and, worse, keeps naming an address the OS is free to hand to
/// the next `mmap`: a crash inside a *new* body at a recycled address would be
/// reported under the *dead* body's name. A crash report that names the wrong
/// method is worse than one that names none, because it is believed.
pub fn unregister_jit_method_name(entry: usize) {
    if entry == 0 {
        return;
    }
    // Only when the registry was ever populated — `jit_names_enabled()` is the
    // usual reason it was not, and `get()` avoids creating it on the drop path.
    if let Some(lock) = JIT_NAME_RANGES.get() {
        // Poison is recovered: a skipped withdrawal would name a recycled
        // address after the dead body.
        let mut v = lock.lock().unwrap_or_else(|e| e.into_inner());
        // DIAG: remember the last few thousand withdrawn names so a crash at a
        // page-aligned (freed) entry can say WHICH body it was. Bounded; only
        // populated when the registry is (`CRATONVM_DBG_JIT_NAMES`).
        let mut dead = JIT_DEAD_NAMES
            .get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (e, end, name) in v.iter().filter(|(e, _, _)| *e == entry) {
            if dead.len() >= 8192 {
                dead.pop_front();
            }
            dead.push_back((*e, *end, name.clone()));
        }
        v.retain(|(e, _, _)| *e != entry);
    }
}

pub(crate) static JIT_DEAD_NAMES: OnceLock<Mutex<std::collections::VecDeque<(usize, usize, String)>>> =
    OnceLock::new();

/// The most recently withdrawn body whose range covered `addr` (crash-time
/// diagnosis of a call into freed code; `try_lock`, may allocate).
pub fn lookup_dead_jit_method_name(addr: usize) -> Option<String> {
    let d = JIT_DEAD_NAMES.get()?.try_lock().ok()?;
    d.iter()
        .rev()
        .find(|(e, end, _)| addr >= *e && addr < *end)
        .map(|(_, _, n)| n.clone())
}

/// Number of registered JIT method-name ranges (diagnostic; also the assertion
/// hook for "the registry does not grow without bound").
pub fn jit_method_name_range_count() -> usize {
    JIT_NAME_RANGES
        .get()
        .and_then(|lock| lock.try_lock().ok().map(|v| v.len()))
        .unwrap_or(0)
}

/// Resolve the method name whose code range contains `addr`. Uses `try_lock` so
/// it never BLOCKS on a held lock — but it is not async-signal-safe: a hit
/// clones the name, which allocates. A signal handler may call it only when the
/// fault cannot be inside the allocator, which is why `crash_handler.rs` gates
/// the call on the opt-in that populates the table (`CRATONVM_DBG_JIT_NAMES`)
/// rather than treating it as free.
///
/// Searches from the END: entries are appended in publication order, so the
/// most recent registration for an address wins. That is the belt to
/// [`unregister_jit_method_name`]'s braces — if a withdrawal is ever missed, a
/// recycled address still symbolizes as the body currently mapped there rather
/// than as the dead one that was mapped there first.
pub fn lookup_jit_method_name(addr: usize) -> Option<String> {
    let v = jit_name_ranges().try_lock().ok()?;
    v.iter()
        .rev()
        .find(|(e, end, _)| addr >= *e && addr < *end)
        .map(|(_, _, name)| name.clone())
}

/// Outcome of a crash-time JIT name lookup.
///
/// [`lookup_jit_method_name`] collapses "the registry covers no such address"
/// and "the registry mutex happened to be held" into the same `None`. Those are
/// not the same fact, and a whole investigation
/// (`jit-wild-jump-page-aligned-pc-20260728`) rested on reading a missing
/// `jit pc :` line as the former.
pub enum JitNameLookup {
    /// The registry mutex was held. The absence of a name says nothing.
    Locked,
    /// The registry was read and no published body covers the address.
    NotFound,
    Found(String),
}

/// Like [`lookup_jit_method_name`], but distinguishes "no match" from "could
/// not read the registry". `try_lock`, so it never blocks; a hit allocates, so
/// the same signal-safety caveat applies.
pub fn lookup_jit_method_name_detailed(addr: usize) -> JitNameLookup {
    let Ok(v) = jit_name_ranges().try_lock() else {
        return JitNameLookup::Locked;
    };
    match v
        .iter()
        .rev()
        .find(|(e, end, _)| addr >= *e && addr < *end)
        .map(|(_, _, name)| name.clone())
    {
        Some(name) => JitNameLookup::Found(name),
        None => JitNameLookup::NotFound,
    }
}

/// Outcome of a crash-time live-code-region lookup. Same reasoning as
/// [`JitNameLookup`]: a lock that could not be taken is not a negative.
pub enum JitRegionLookup {
    Locked,
    NotFound,
    /// `(base, capacity)` of the live executable buffer covering the address.
    Found(usize, usize),
}

/// Which LIVE executable buffer covers `addr`?
///
/// Complements [`recent_code_free_covering`] (buffers already unmapped) and
/// [`lookup_jit_method_name`] (bodies `JitCache::put` published). Every
/// [`ExecutableBuffer`] registers here, including the ones no `put` ever names:
/// OSR trampolines, and bodies a compiler thread is still emitting — which on
/// Linux are mapped `rw-` and so fault on instruction fetch exactly like an
/// unmapped page. `try_lock`, so it is safe from a crash handler.
pub fn jit_code_region_covering(addr: usize) -> JitRegionLookup {
    let regions = match jit_code_regions().try_lock() {
        Ok(regions) => regions,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return JitRegionLookup::Locked,
    };
    let idx = regions.regions.partition_point(|&(s, _)| s <= addr);
    if idx == 0 {
        return JitRegionLookup::NotFound;
    }
    let (start, end) = regions.regions[idx - 1];
    if addr >= start && addr < end {
        JitRegionLookup::Found(start, end - start)
    } else {
        JitRegionLookup::NotFound
    }
}

/// Where each storage class lives in a compiled frame, as positive
/// `[rbp - off]` byte offsets. Every range is `lo..hi` (exclusive `hi`), and an
/// empty range is `0..0`.
///
/// The GC needs this partition because the classes are NOT interchangeable
/// under a relocating young generation:
///
///   * `java_locals`, `operand_spill`, `ref_hoist` and `scalar_fields` are
///     genuine, independent oop storage — a reference there is the only copy,
///     so the collector must be able to REWRITE it or must not move at all.
///   * `arith_hoist` and `arith_scratch` hold `int`/`long` results of hoisted
///     integer arithmetic. Never references.
///   * `callee_saved`, `xmm_saved` and `reg_spill` are IMAGES of registers.
///     `callee_saved`/`xmm_saved` hold the CALLER's values for the epilogue to
///     restore; `reg_spill` is the write-only SB-CRASH-04 blind spill that
///     exists to make register-resident values visible to a CONSERVATIVE scan.
///     Nothing resumes from any of them in a way the owning frame's own
///     published roots do not already cover.
///
/// See `moving-young-gen-drops-jit-held-oops-FIXED.md`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    /// Java locals: slot `i` at `[rbp - (i + 1) * 8]`.
    pub java_locals_hi: i32,
    /// LICM `aaload` hoist slots. Always reference-typed (`aaload` loads an
    /// element of a reference array).
    pub ref_hoist_lo: i32,
    pub ref_hoist_hi: i32,
    /// V6 (round 12 wave 4): the backend names every LIVE ref-hoist slot in
    /// the oop map of each safepoint inside its loop (x64
    /// `Compiler::live_ref_hoist_slots`), so a word in
    /// `ref_hoist_lo..ref_hoist_hi` that a resolved map does not name is dead
    /// there. `false` (the default) keeps the region unmodelled: always
    /// verified, never excused by map absence.
    pub ref_hoist_mapped: bool,
    /// LICM integer-arithmetic hoist slots and their shared evaluation
    /// scratch. Always primitive.
    pub arith_lo: i32,
    pub arith_hi: i32,
    /// Scalar-replacement field slots (all fields of every eliminated object).
    pub scalar_lo: i32,
    pub scalar_hi: i32,
    /// End of the whole reserved-locals region (locals + heap slot + hoists +
    /// scalar fields + the reserved tail slots: sp-id, stack floor, cached
    /// JIT thread, shadow bookkeeping).
    pub locals_hi: i32,
    /// Canonical operand-stack spill slots.
    pub spill_lo: i32,
    pub spill_hi: i32,
    /// Prologue save area for the caller's callee-saved GPRs.
    pub callee_saved_lo: i32,
    pub callee_saved_hi: i32,
    /// Is that save area at the SHALLOW end of the frame (nearest the frame
    /// pointer) rather than the deep end?
    ///
    /// x86-64 puts it deepest, which lets the band verifier treat
    /// `callee_saved_lo` as a half-line -- everything at or beyond it is a
    /// register image or past the frame. AArch64's prologue puts the
    /// callee-saved GPRs immediately below the frame pointer (the saved FP/LR
    /// frame record is AT it) and the spill area BELOW them, so that half-line would exclude
    /// the entire spill area -- exactly where the oop maps point, leaving the
    /// verifier unable to see the words it exists to check.
    ///
    /// `false` (the derived default) is the x86-64 geometry, so no existing
    /// producer changes. A backend that sets it gets the RANGE exclusion
    /// (`is_register_image`) and not the half-line.
    pub callee_saved_shallow: bool,
    /// Prologue save area for the caller's callee-saved XMMs.
    pub xmm_saved_lo: i32,
    pub xmm_saved_hi: i32,
    /// Per-safepoint blind GPR spill (`emit_pre_safepoint_spill`).
    pub reg_spill_lo: i32,
    pub reg_spill_hi: i32,
    /// Start of the OUTGOING area — the ABI shadow space plus the in-frame
    /// stack-argument reserve — as an `[rbp - off]` offset, or `0` when the
    /// producer does not distinguish it.
    ///
    /// `region_name` reports everything past `reg_spill_hi` as
    /// `outgoing-args-or-deopt-regs`, which is two regions with different
    /// contents: the frame-deopt `SavedRegisters` block (a register image the
    /// deopt stub reads back) sits between the blind spill and the outgoing
    /// area. A consumer that wants to stop scanning the outgoing reserve must
    /// not thereby stop scanning `SavedRegisters`, so the boundary is published
    /// rather than guessed.
    ///
    /// `0` means "not published" and must be read as "assume the whole region
    /// is `SavedRegisters`" — the conservative direction.
    pub outgoing_lo: i32,
    /// The frame-deopt `SavedRegisters` GPR image, as a half-open `[lo, hi)`
    /// range of `[rbp - off]` offsets, or `(0, 0)` when the frame reserved no
    /// such region.
    ///
    /// Sixteen GPR slots, the SHALLOW half of the 256-byte `SavedRegisters`
    /// region the deopt stub spills into (`emit_deopt_stubs`:
    /// `gpr[r] -> [rbp - (deopt_regs_base - r*8)]`). The XMM half is
    /// deliberately NOT included: a `double` whose bits happen to equal a live
    /// object's base would be rewritten by a remap that admitted it, and an
    /// XMM cannot hold a reference in the first place.
    ///
    /// # Why this needs its own range rather than `outgoing_lo`
    ///
    /// `region_name` buckets everything past `reg_spill_hi` as
    /// `outgoing-args-or-deopt-regs`, and the two halves of that bucket want
    /// OPPOSITE treatment. The outgoing reserve is uninitialised memory a
    /// consumer may stop scanning; `SavedRegisters` is a register image the
    /// deopt stub READS BACK, so a moved reference in it must be rewritten --
    /// and once it is rewritten the word is movable rather than unrewritable,
    /// which is what takes its object out of G1's pin set.
    /// `register_image_remap_admits` is the consumer.
    pub deopt_gpr_lo: i32,
    pub deopt_gpr_hi: i32,
    /// The frame-deopt `SavedRegisters` XMM image, as a half-open `[lo, hi)`
    /// range of `[rbp - off]` offsets, or `(0, 0)` when the frame reserved no
    /// such region.
    ///
    /// The DEEP half of the same 256-byte region as [`Self::deopt_gpr_lo`]
    /// (`xmm[n] -> [rbp - (deopt_regs_base - 128 - n*8)]`), and it gets the
    /// opposite treatment for a reason that can be checked in one function:
    /// `deopt::try_resolve_value` reads `regs.xmm` from exactly two arms,
    /// `FrameValue::XmmFloat` and `FrameValue::XmmDouble`, and tags them
    /// `Float` and `Double`. `FrameValue::RegisterRef` -- the only arm that
    /// yields an `Object` from the spilled register file -- reads `regs.gpr`.
    /// **No reference is ever recovered from this half**, so a word here needs
    /// no rewrite after a move, which is what lets the pin decision let go of
    /// its object.
    pub deopt_xmm_lo: i32,
    pub deopt_xmm_hi: i32,
    /// Total frame size (`SUB RSP, frame_size`); the band is `[rbp - size, rbp)`.
    pub frame_size: i32,
}

/// Compiles whose frame layout was published, and how many of them gave each
/// nominated storage class a non-empty extent. Read by
/// [`region_extent_census`].
pub(crate) static FRAMES_PUBLISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static FRAMES_WITH_SCALAR_SPAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static FRAMES_WITH_REF_HOIST_SPAN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static FRAMES_WITH_ARITH_SPAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(frames_published, with_scalar_span, with_ref_hoist_span, with_arith_span)`.
///
/// The denominator a stale-word census by `FrameLayout::region_name` needs
/// before any of its per-region zeros can be read as evidence.
pub fn region_extent_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        FRAMES_PUBLISHED.load(Relaxed),
        FRAMES_WITH_SCALAR_SPAN.load(Relaxed),
        FRAMES_WITH_REF_HOIST_SPAN.load(Relaxed),
        FRAMES_WITH_ARITH_SPAN.load(Relaxed),
    )
}

impl FrameLayout {
    /// Whether `off` lands in the frame-deopt `SavedRegisters` GPR image.
    ///
    /// See [`Self::deopt_gpr_lo`]. `(0, 0)` -- no region reserved -- answers
    /// `false`, which keeps every consumer on its pre-publication behaviour.
    #[inline]
    pub fn is_deopt_saved_gpr_image(&self, off: i32) -> bool {
        self.deopt_gpr_hi > self.deopt_gpr_lo && off >= self.deopt_gpr_lo && off < self.deopt_gpr_hi
    }

    /// Whether `off` lands in the frame-deopt `SavedRegisters` XMM image.
    /// See [`Self::deopt_xmm_lo`] for why this half is the one that can be let
    /// go of rather than rewritten.
    #[inline]
    pub fn is_deopt_saved_xmm_image(&self, off: i32) -> bool {
        self.deopt_xmm_hi > self.deopt_xmm_lo && off >= self.deopt_xmm_lo && off < self.deopt_xmm_hi
    }

    /// Whether `off` names a slot that is only ever an IMAGE of a register.
    #[inline]
    pub fn is_register_image(&self, off: i32) -> bool {
        (self.callee_saved_hi > self.callee_saved_lo
            && off >= self.callee_saved_lo
            && off < self.callee_saved_hi)
            || (self.xmm_saved_hi > self.xmm_saved_lo
                && off >= self.xmm_saved_lo
                && off < self.xmm_saved_hi)
            || (self.reg_spill_hi > self.reg_spill_lo
                && off >= self.reg_spill_lo
                && off < self.reg_spill_hi)
    }

    /// Whether each of the three storage classes
    /// `moving-young-corruption-rootcause.md` nominates actually HAS an extent
    /// in this frame: `(scalar-replaced-field, licm-ref-hoist, licm-arith)`.
    ///
    /// The reason this is a method and not left to the reader of a census:
    /// [`Self::region_name`]'s ladder gates every one of those names behind
    /// `hi > lo`, and `frame_layout()` builds all three spans from collections
    /// that are EMPTY when the optimisation produced no slots. So a stale-word
    /// census reporting zero words in `scalar-replaced-field` says nothing
    /// about scalar replacement unless some frame it walked had a non-empty
    /// scalar span — the region has to exist before a count of words inside it
    /// can mean anything. That is the trap §10.9 of
    /// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
    /// records, one level below the one §10.8 fell into.
    #[inline]
    pub fn candidate_region_extents(&self) -> (bool, bool, bool) {
        (
            self.scalar_hi > self.scalar_lo,
            self.ref_hoist_hi > self.ref_hoist_lo,
            self.arith_hi > self.arith_lo,
        )
    }

    /// Fold this layout into the process-wide extent census. Called once per
    /// published compile; see [`region_extent_census`].
    pub fn record_region_extent_census(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let (scalar, hoist, arith) = self.candidate_region_extents();
        FRAMES_PUBLISHED.fetch_add(1, Relaxed);
        if scalar {
            FRAMES_WITH_SCALAR_SPAN.fetch_add(1, Relaxed);
        }
        if hoist {
            FRAMES_WITH_REF_HOIST_SPAN.fetch_add(1, Relaxed);
        }
        if arith {
            FRAMES_WITH_ARITH_SPAN.fetch_add(1, Relaxed);
        }
    }

    /// The `x64::ALL_SPILL_GPRS` index whose image `off` is, when `off` lands in
    /// the per-safepoint blind spill.
    ///
    /// The inverse of `emit_blind_reg_spill`'s `reg_spill_lo + 8 * index`, and
    /// the same index `OopMapEntry::reg_oop_mask` is a bitmask over -- so a
    /// consumer with a frame offset and a map can answer "may this slot hold a
    /// live oop" without decoding a register name.
    pub fn spill_image_index(&self, off: i32) -> Option<u32> {
        if self.reg_spill_hi <= self.reg_spill_lo
            || off < self.reg_spill_lo
            || off >= self.reg_spill_hi
        {
            return None;
        }
        // Cast: the span is 14 slots, so this is far inside u32.
        Some(((off - self.reg_spill_lo) / 8) as u32)
    }

    /// Name the x86-64 GPR whose image `off` is, when `off` lands in the
    /// per-safepoint blind spill (`safepoint-gpr-spill-image`).
    ///
    /// The spill is emitted as one store per entry of `x64::ALL_SPILL_GPRS`, in
    /// table order, at `reg_spill_lo + 8 * index` (`emit_blind_reg_spill`), so
    /// the index inverts exactly. Diagnostics only, and the table below must
    /// stay in step with that constant -- a wrong name here misattributes a
    /// retained object to the wrong register, which is worse than no name.
    pub fn spill_image_register(&self, off: i32) -> Option<&'static str> {
        const NAMES: [&str; 14] = [
            "rax", "rcx", "rdx", "rbx", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
            "r14", "r15",
        ];
        if self.reg_spill_hi <= self.reg_spill_lo
            || off < self.reg_spill_lo
            || off >= self.reg_spill_hi
        {
            return None;
        }
        // Cast: the span is 14 slots, so the index is in range by construction.
        NAMES.get(((off - self.reg_spill_lo) / 8) as usize).copied()
    }

    /// Whether `off` is in the reserved frame tail that cannot contain Java
    /// object references. This covers ABI bookkeeping and padding between the
    /// real Java locals and the operand spill area, while excluding the typed
    /// LICM and scalar-replacement ranges that may occupy the same interval.
    ///
    /// Conservative root scans must not treat reused stack bytes in this tail
    /// as Java references: no generated Java body reads them as locals, and
    /// every caller frame is scanned independently for its actual oop slots.
    #[inline]
    pub fn is_non_oop_reserved_tail_slot(&self, off: i32) -> bool {
        if off < self.java_locals_hi || off >= self.locals_hi {
            return false;
        }
        let hit = |lo: i32, hi: i32| hi > lo && off >= lo && off < hi;
        !hit(self.ref_hoist_lo, self.ref_hoist_hi)
            && !hit(self.arith_lo, self.arith_hi)
            && !hit(self.scalar_lo, self.scalar_hi)
    }

    /// Name the region `off` falls in. Diagnostics only.
    pub fn region_name(&self, off: i32) -> &'static str {
        let hit = |lo: i32, hi: i32| hi > lo && off >= lo && off < hi;
        if hit(self.ref_hoist_lo, self.ref_hoist_hi) {
            "licm-ref-hoist"
        } else if hit(self.arith_lo, self.arith_hi) {
            "licm-arith"
        } else if hit(self.scalar_lo, self.scalar_hi) {
            "scalar-replaced-field"
        } else if self.java_locals_hi > 0 && off < self.java_locals_hi {
            "java-local"
        } else if self.locals_hi > 0 && off < self.locals_hi {
            "reserved-locals-tail"
        } else if hit(self.spill_lo, self.spill_hi) {
            "operand-spill"
        } else if hit(self.callee_saved_lo, self.callee_saved_hi) {
            "callee-saved-gpr-image"
        } else if hit(self.xmm_saved_lo, self.xmm_saved_hi) {
            "callee-saved-xmm-image"
        } else if hit(self.reg_spill_lo, self.reg_spill_hi) {
            "safepoint-gpr-spill-image"
        } else if hit(self.deopt_gpr_lo, self.deopt_gpr_hi) {
            "deopt-saved-gpr-image"
        } else if hit(self.deopt_xmm_lo, self.deopt_xmm_hi) {
            "deopt-saved-xmm-image"
        } else if self.reg_spill_hi > 0 && off >= self.reg_spill_hi {
            "outgoing-args-or-deopt-regs"
        } else {
            "unclassified"
        }
    }
}

/// The bytecode one spliced body was read from, kept on an optimizing artifact
/// that records chain snapshots ([`CompiledMethod::splice_scope_sources`]).
///
/// Round 14 wave 3 (lane chain; proposal R14DP-5 of
/// `jit-r14-deopt-proposals-RETIRED-20260930.md`, page
/// `r13w5-resume-chain-inner-scope-of-a-redefined-class`): a deopt chain whose
/// SPLICED callee's class was redefined after the compile rebuilds that scope's
/// frame from THIS code (the obsolete method, as JVMTI specifies) instead of
/// refusing the chain. The code is the callee's own, byte for byte: the planner
/// appends `InlineSite::callee_code` to the combined buffer unmodified (branch
/// offsets are relative, constant-pool operands index the callee's own pool),
/// and a callee with an exception table carries it in [`Self::exception_table`]
/// (the original rows; the spliced copy may have its handlers stripped or
/// masked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpliceScopeSource {
    /// `"class.name:descriptor"`, the key a chain scope names
    /// (`ir::IrInlineSite::method_key`).
    pub method_key: String,
    /// The class id the planner resolved the body under
    /// (`ir::IrInlineSite::class_id`, never 0 here).
    pub class_id: u32,
    /// The body's bytecode, padded with the two zero sentinel bytes every
    /// method's code carries.
    pub code: Arc<[u8]>,
    /// The body's `max_locals`.
    pub max_locals: u16,
    /// Whether the body is a static method.
    pub is_static: bool,
    /// The method's own exception table, `(start_pc, end_pc, handler_pc,
    /// catch_type)` as the class file has it (`catch_type` a constant-pool
    /// index of the compile's generation, 0 for catch-all); empty for a body
    /// without one. [`Self::code`] is then the original bytes, so a resumed
    /// frame catches its own exception.
    pub exception_table: Vec<(u16, u16, u16, u16)>,
}

/// `CRATONVM_JIT_CALLER_HELD_CALLEE_RESUME`, **default ON**: shape 2 (c) of
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// the stages 1-3 of
/// `docs/feature-designs/jit-ir-optimizations.md`
/// that wave 53 left unbuilt. A caller-held synchronized `CALL` (the
/// single-pass `sync_direct_site`, the optimizing `emit_direct_cross_call`
/// with `sync_exit`) asks the callee-deopt service on its sentinel edge
/// BEFORE it releases the monitor, and a resumed callee's value takes the
/// normal edge (whose release is the one release); the VM's service resumes a
/// synchronized callee's frame that names no hold of its own when this thread
/// holds the method's monitor -- the caller does -- releasing nothing of it
/// (`deopt_resume::caller_holds_method_monitor`); and the safepoint verdict
/// stops withholding `SAFEPOINT_VERDICT_NAMED_BODY` from an activation such a
/// site made, so a bound synchronized leaf loop leaves for an agent or a
/// debugger as HotSpot's deoptimized frames do. `=0`: none of the three, as
/// before. Read once per caller-held site compile, and on a bound body's
/// verdict.
pub fn caller_held_callee_resume_enabled() -> bool {
    true
}

/// `CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE`, **default ON** (round 14 wave 3,
/// lane chain, R14DP-5): an optimizing artifact that records chain snapshots
/// keeps its spliced bodies' bytecode ([`SpliceScopeSource`]) and the VM's
/// doors resume a chain whose spliced callee's class was redefined since the
/// compile in that bytecode. `=0` keeps nothing and restores the refusal. Read
/// once per compile that records chain snapshots, and on such a trap.
pub fn chain_inner_own_source_enabled() -> bool {
    true
}

/// The spliced bodies of `sites` as [`SpliceScopeSource`] rows, sorted by key,
/// one row per key; a key whose rows disagree (two classes, two bodies) is
/// left out, so the VM keeps its refusal for it. `combined` is the planner's
/// combined buffer, in which each site's own body is `[base, base + code_len)`.
///
/// Round rx27 (lane matcher; page
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// the row is the bytecode the INTERPRETER runs, because the VM rebuilds a
/// redefined scope's frame from it (`deopt_resume::chain_inner_scope_own_sources`).
/// A body the resolver changed (`originals`, [`ir::IrInlineTables::original_body_codes`])
/// contributes its original bytes, not the masked copy; a body whose original
/// carried an exception table (a key of `with_tables`,
/// [`ir::IrInlineTables::original_body_handler_rows`]) contributes its
/// original bytes AND that table, so the template the VM builds from the row
/// catches its own exception; such a body always has an original (every route
/// that admits a table strips or masks the copy), and one without is left out.
pub(crate) fn splice_scope_sources(
    sites: &HashMap<usize, ir::IrInlineSite>,
    combined: &[u8],
    originals: &HashMap<usize, Vec<u8>>,
    with_tables: &HashMap<usize, Vec<(usize, usize, usize, u16)>>,
) -> Vec<SpliceScopeSource> {
    let mut rows: Vec<SpliceScopeSource> = Vec::with_capacity(sites.len());
    for site in sites.values() {
        if site.class_id == 0 || site.method_key.is_empty() {
            continue;
        }
        let Some(end) = site.base.checked_add(site.code_len) else {
            continue;
        };
        let table = with_tables.get(&site.base);
        let body = match originals.get(&site.base) {
            Some(original) => original.as_slice(),
            None if table.is_some() => continue,
            None => match combined.get(site.base..end) {
                Some(body) => body,
                None => continue,
            },
        };
        let Ok(max_locals) = u16::try_from(site.max_locals) else {
            continue;
        };
        let Some(exception_table) = table.map_or(Some(Vec::new()), |rows| {
            rows.iter()
                .map(|&(s, e, h, c)| {
                    Some((
                        u16::try_from(s).ok()?,
                        u16::try_from(e).ok()?,
                        u16::try_from(h).ok()?,
                        c,
                    ))
                })
                .collect()
        }) else {
            continue;
        };
        let mut padded = Vec::with_capacity(body.len() + 2);
        padded.extend_from_slice(body);
        padded.extend_from_slice(&[0, 0]);
        rows.push(SpliceScopeSource {
            method_key: site.method_key.clone(),
            class_id: site.class_id,
            code: Arc::from(padded.into_boxed_slice()),
            max_locals,
            is_static: !site.receiver_is_arg0,
            exception_table,
        });
    }
    one_splice_scope_source_per_key(rows)
}

/// t1 ARGFIX item 2 (`ir_lower::ir_keep_set_splice_local_liveness_enabled`):
/// each spliced method's interpreter bytecode for the keep set's per-scope
/// local liveness, one row per method key. The body is the UNMASKED copy
/// (`original_body_codes`) when the planner masked its handlers; a body with
/// handler rows and no unmasked copy is left out (its relocated bytes hide
/// the handler code, so its liveness would miss a handler's reads), and so is
/// a key whose sites disagree on the bytes. Parameter slots come from the
/// key's descriptor, receiver included.
pub(crate) fn spliced_body_codes(tables: &ir::IrInlineTables, combined: &[u8]) -> Vec<ir_lower::SplicedBodyCode> {
    let mut by_key: HashMap<&str, Option<ir_lower::SplicedBodyCode>> = HashMap::new();
    for site in tables.sites.values() {
        if site.method_key.is_empty() {
            continue;
        }
        let rows = tables.splice_handler_rows.get(&site.base);
        // Round rx27 (lane matcher): a body whose OWN code has an exception
        // table is read whole, with that table -- the interpreter's own
        // liveness. A stripped fence `finally` (nio20) is cut from the copy,
        // and its handler reads the fenced locals; without its row they were
        // dead on every path that only throws.
        let own_rows = tables.original_body_handler_rows.get(&site.base);
        let code = match tables.original_body_codes.get(&site.base) {
            Some(original) if own_rows.is_some() && original.len() >= site.code_len => {
                Some(original.clone())
            }
            Some(_) if own_rows.is_some() => None,
            Some(original) if original.len() == site.code_len => Some(original.clone()),
            Some(_) => None,
            None if rows.is_some_and(|r| !r.is_empty()) => None,
            None => site
                .base
                .checked_add(site.code_len)
                .and_then(|end| combined.get(site.base..end))
                .map(<[u8]>::to_vec),
        };
        let num_params = site
            .method_key
            .rsplit_once(':')
            .and_then(|(_, desc)| descriptor_param_slots(desc))
            .map(|n| n + usize::from(site.receiver_is_arg0));
        let row = match (code, num_params) {
            (Some(code), Some(num_params)) => Some(ir_lower::SplicedBodyCode {
                method_key: site.method_key.clone(),
                code,
                num_params,
                num_locals: site.max_locals,
                handlers: match own_rows {
                    Some(own) => own.iter().map(|&(s, e, h, _)| (s, e, h)).collect(),
                    None => rows
                        .into_iter()
                        .flatten()
                        .filter_map(|r| {
                            Some((
                                r.start_pc.checked_sub(site.base)?,
                                r.end_pc.checked_sub(site.base)?,
                                r.handler_pc.checked_sub(site.base)?,
                            ))
                        })
                        .collect(),
                },
            }),
            _ => None,
        };
        match by_key.entry(site.method_key.as_str()) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(row);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if o.get().as_ref() != row.as_ref() {
                    o.insert(None);
                }
            }
        }
    }
    let mut out: Vec<ir_lower::SplicedBodyCode> = by_key.into_values().flatten().collect();
    out.sort_by(|a, b| a.method_key.cmp(&b.method_key));
    out
}

/// The local slots a method descriptor's parameters take (`J` / `D` two),
/// receiver excluded; `None` for a malformed descriptor.
pub(crate) fn descriptor_param_slots(desc: &str) -> Option<usize> {
    let params = desc.strip_prefix('(')?.split_once(')')?.0.as_bytes();
    let mut slots = 0usize;
    let mut i = 0usize;
    while i < params.len() {
        match params[i] {
            b'J' | b'D' => slots += 2,
            b'B' | b'C' | b'F' | b'I' | b'S' | b'Z' => slots += 1,
            b'L' => {
                i += params[i..].iter().position(|&c| c == b';')?;
                slots += 1;
            }
            b'[' => {
                while params.get(i) == Some(&b'[') {
                    i += 1;
                }
                if params.get(i) == Some(&b'L') {
                    i += params[i..].iter().position(|&c| c == b';')?;
                } else if params.get(i).is_none() {
                    return None;
                }
                slots += 1;
            }
            _ => return None,
        }
        i += 1;
    }
    Some(slots)
}


/// `rows` sorted by key, one row per key; a key whose rows disagree (two
/// classes, two bodies) is left out. Shared by the optimizing tier's
/// [`splice_scope_sources`] and the single-pass tier's splice post-call
/// sites (interpreter round i2 wave 59, lane L2; `x64/driver.rs`).
pub(crate) fn one_splice_scope_source_per_key(
    mut rows: Vec<SpliceScopeSource>,
) -> Vec<SpliceScopeSource> {
    rows.sort_by(|a, b| a.method_key.cmp(&b.method_key));
    let mut out: Vec<SpliceScopeSource> = Vec::with_capacity(rows.len());
    let mut poisoned: Option<String> = None;
    for row in rows {
        if poisoned.as_deref() == Some(row.method_key.as_str()) {
            continue;
        }
        if let Some(last) = out.last() {
            if last.method_key == row.method_key {
                if *last != row {
                    poisoned = Some(row.method_key.clone());
                    out.pop();
                }
                continue;
            }
        }
        out.push(row);
    }
    out
}

/// Did every resolver read of the compilation open on this thread happen at
/// its constant-pool stamp -- i.e. has no class redefinition begun since the
/// compile scope opened ([`open_compile_epoch_witness`])? The redefinition
/// count advances BEFORE a class's bytecode and pool are swapped (under the
/// class manager's write lock, which every resolver read excludes), so an
/// unchanged count after the reads proves each read saw the generation the
/// artifact's [`CompiledMethod::compile_cp_stamp`] names; a moved one may
/// pair new bytecode with the old stamp, and the planner then keeps no
/// [`SpliceScopeSource`]. `false` with no compile scope open or no registered
/// source.
pub(crate) fn splice_sources_read_at_compile_stamp() -> bool {
    let Some(stamp) = COMPILE_CP_STAMP
        .try_with(|c| c.try_borrow().ok().and_then(|scope| scope.cp_stamp))
        .ok()
        .flatten()
    else {
        return false;
    };
    CP_STAMP_SOURCE
        .get()
        .is_some_and(|source| source() == stamp)
}

/// JIT round 19 proposal wave 6, lane sync6 (SYNCDOOR618-5): which monitor a
/// SELF-LOCKING body ([`CompiledMethod::self_locks_monitor`]) holds at every
/// pc between its prologue enter and its exits, and where its frame keeps it,
/// so a stack walk can attribute the hold to this activation's depth for
/// `ThreadInfo.getLockedMonitors` / JVMTI `GetOwnedMonitorStackDepthInfo`
/// (HotSpot's `javaVFrame::locked_monitors` names the compiled frame).
///
/// Metadata only: no emitted code reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfLockHold {
    /// An instance method: the receiver. `obj_rbp_off` is the positive
    /// offset of the frame word that holds it (`[rbp - off]`, the word the
    /// exits release from: local 0's home in a single-pass body), or `None`
    /// when the backend publishes none (the optimizing tier keeps the
    /// receiver in its own allocation).
    Instance { obj_rbp_off: Option<i32> },
    /// A `static synchronized` method: the class mirror of
    /// [`CompiledMethod::owner_class_id`], which a reader can name without
    /// the frame. `mirror_word_rbp_off` is the frame word the single-pass body
    /// refreshes from the mirror slot (`0` until the prologue enter wrote it),
    /// or `None`.
    Static { mirror_word_rbp_off: Option<i32> },
}

/// One type-check cell an optimizing-tier body owns
/// (`CompiledMethod::_jit_typecheck_seen_cells`), whose address the lowering
/// bakes. MHIFACE19-1's seen-class ways use words 0 and 1. The lambda-proxy
/// ways (`ir_lower::ir_instanceof_proxy_ways_enabled`) use word 0 (a proxy the
/// helper answered `1` for), word 1 (one it answered `0` for) and, under
/// round rx27's `CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS`, word 2 (the previous
/// word-0 proxy). Word 3 is padding. Every word starts at a value no proxy id
/// equals.
pub type TypecheckSeenCell = [std::sync::atomic::AtomicU32; 4];
