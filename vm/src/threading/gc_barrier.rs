// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Stop-the-world GC barrier for multi-threaded execution.
//!
//! When garbage collection is needed, the triggering thread requests a
//! stop-the-world (STW) pause. All other threads, at their next safepoint
//! (allocation site or backward branch), deposit their root ObjectRefs and
//! wait for GC to complete. After collection, each thread applies the
//! pointer map to update its own frame references.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Condvar, Mutex};

use crate::threading::jvm_thread::ThreadId;
use crate::threading::thread_state::{self, ThreadExecState};

/// The `stw_requested` byte, in a cache line of its own, in a page of its own,
/// taken from the same OS allocator the JIT code cache comes from.
///
/// `stw_requested` is the single hottest load in the VM: every interpreter
/// thread reads it **once per bytecode** at the top of the dispatch loop, and
/// compiled code polls the very same byte through
/// [`GcBarrier::stw_requested_flag_addr`]. Its three neighbours in `GcBarrier`
/// are all written by other threads — `gc_generation` every collection,
/// `threads_blocked` on every `enter_blocked`/`leave_blocked` (so every
/// blocking native op: `Object.wait`, `Thread.sleep`, `LockSupport.park`,
/// selector `select`, …), and `inner`, whose `parking_lot::Mutex` word is
/// written on every lock and unlock. All four sat inside the first 64 bytes:
/// `AtomicBool` at offset 0, 7 bytes of alignment padding, then the two
/// `AtomicU64`s at 8 and 16 and the mutex at 24.
///
/// So a write to any of them invalidated, on every interpreting core, the line
/// carrying the flag those cores read every bytecode.
///
/// # What it is worth, bounded rather than guessed
///
/// This cannot be A/B'd the way everything else on its branch was: a struct
/// layout is not a runtime toggle, so there is no kill switch to write. It was
/// therefore bounded arithmetically, from a measured write rate.
///
/// `probes/SharedLine.java` runs compute threads in tight interpreted loops
/// (reading this flag once per bytecode) alongside an **untimed** wait/notify
/// ping-pong, whose every hop crosses `enter_blocked`/`leave_blocked`. Untimed
/// deliberately — a timed wait on Windows is tick-quantized to ~15 ms, which
/// would cap the churn at ~66/s and make the probe vacuous. Measured
/// 2026-09-05: **116,025 handoffs in 4 s ≈ 29,000/s**, so ~58,000 writes/s to
/// this line, from a synthetic ping-pong doing nothing else. Real code does
/// not exceed that by orders of magnitude.
///
/// At 8 interpreting threads and a ~70 ns single-socket coherence miss, that
/// is `58_000 * 8 * 70ns` ≈ **32 ms of aggregate CPU per second across 8
/// cores — about 0.4%**, which is below what the harness resolves.
///
/// The cost scales as `writers x readers x miss_latency`, so it grows with
/// core count and again across sockets — but not dramatically: 64 readers at a
/// ~200 ns cross-socket miss is still only ~1%. **This is a small effect, and
/// the honest claim is a bound, not a win.**
///
/// It is kept because it costs one cache line once per process and cannot
/// regress anything, and because the tree already owns the idiom for exactly
/// this reason (`gc/src/zgc/census.rs` and `gc/src/collector.rs` both carry
/// `#[repr(align(64))]` so unrelated counters cannot share a line). Anyone
/// with a many-core box can put a number on it with the probe; that is the
/// only way this one gets measured rather than bounded.
///
/// 64 rather than 128: the two in-tree precedents use 64 and 128 respectively,
/// and the destructive-interference size on x86-64 is 64.
///
/// # Why it is not just `#[repr(align(64))] AtomicBool`
///
/// Isolation was the whole story until 2026-09-10, and `#[repr(align(64))]` on
/// an inline field buys it. What an inline field cannot buy is an ADDRESS, and
/// this byte is one of the few in the VM whose address is compiled INTO
/// machine code.
///
/// `jit/src/x64/safepoint.rs::emit_safepoint_poll` wants
/// `TEST BYTE [rip+disp32], 0xFF` — the entire poll in **one 7-byte
/// instruction and no register** — on the back edge of every compiled loop and
/// at every method entry. `disp32` reaches ±2 GB, so it needs the flag within
/// 2 GB of the code polling it. When it is not, the emitter falls back to
/// `MOV R11, imm64 ; TEST BYTE [R11], 0xFF`: 15 bytes and a clobbered
/// register, at every one of those sites.
///
/// A `GcBarrier` field is reachable only through `Arc<SharedVm>`, i.e. from
/// the Rust global allocator, which in a shipping build is mimalloc
/// (`vm-cli/src/main.rs`). Mimalloc reserves its arenas nowhere near where an
/// anonymous `mmap` / `VirtualAlloc(NULL, ...)` — the primitive
/// `cratonvm_jit::platform` hands the code cache — lands. Measured on Linux
/// x86-64 with `CRATONVM_DBG_JIT_DISASM=*`: code buffer at `0x7DE4D7F9E000`,
/// flag at `0x2000CD6E2C0`, **123.9 TB apart**, so every compiled poll in the
/// process took the long form. Nothing failed and no test noticed, because the
/// fallback is CORRECT — it reads the same byte and branches the same way, it
/// is just longer.
///
/// So the byte moves out of the struct and into a cell from
/// [`cratonvm_jit::platform::alloc_code_adjacent_cell`], which allocates from
/// the code cache's own primitive precisely so the two land in the same region
/// of the address space. The field becomes a `&'static AtomicBool` into that
/// never-unmapped cell; every reader and writer goes through the same four
/// methods and is unaffected.
///
/// Placement stays a HINT — the OS picks the address and may pick badly, and
/// `emit_safepoint_poll` keeps its fallback for exactly that. Verify on a real
/// run with `CRATONVM_DBG_JIT_DISASM=*` and read any loop body:
/// `test byte [rel ...]` means in reach, `mov r11, <imm64>` means it is not.
///
/// The 64-byte cell is never freed, so a process that constructs `GcBarrier`
/// N times keeps 64N bytes forever. That is a real leak and it is bounded by
/// how many VMs a process creates; see `alloc_code_adjacent_cell`'s lifetime
/// note.
pub struct CacheLineFlag(&'static AtomicBool);

impl CacheLineFlag {
    /// Take a cell for this flag.
    ///
    /// Not `const` any more (it allocates), which is why `GcBarrier::new` is
    /// the only construction site — there is no `static CacheLineFlag`.
    ///
    /// The `Box::leak` arm is the correctness floor: if the OS refuses the
    /// mapping the flag still exists, still works, and merely gives up the
    /// short encoding — the state every build was in before 2026-09-10.
    fn new(v: bool) -> Self {
        let cell: &'static AtomicBool = match cratonvm_jit::platform::alloc_code_adjacent_cell() {
            // SAFETY: `alloc_code_adjacent_cell` hands back a freshly
            // carved, zero-filled, 64-byte-aligned cell that no other
            // reference names and that is never unmapped or recycled, so
            // the `'static` and the exclusivity of this initializing write
            // both hold. `AtomicBool` is one byte with alignment 1.
            Some(p) => unsafe {
                let p = p.cast::<AtomicBool>();
                p.write(AtomicBool::new(v));
                &*p
            },
            None => Box::leak(Box::new(AtomicBool::new(v))),
        };
        Self(cell)
    }
    /// The flag itself, for the ordinary atomic API.
    #[inline(always)]
    pub fn flag(&self) -> &AtomicBool {
        self.0
    }
    #[inline(always)]
    pub fn load(&self, order: Ordering) -> bool {
        self.0.load(order)
    }
    #[inline(always)]
    pub fn store(&self, v: bool, order: Ordering) {
        self.0.store(v, order)
    }
    #[inline(always)]
    pub fn swap(&self, v: bool, order: Ordering) -> bool {
        self.0.swap(v, order)
    }
}

/// Coordinates stop-the-world pauses for garbage collection.
///
/// The barrier uses a cheap `AtomicBool` flag (`stw_requested`) that threads
/// poll at safepoints. When set, threads deposit their roots and wait for
/// the GC initiator to finish collection.
pub struct GcBarrier {
    /// Cheap flag polled at every safepoint. Only requires an atomic load.
    ///
    /// Not stored here: [`CacheLineFlag`] is a reference into a cell that
    /// lives outside this struct entirely, on its own cache line, in its own
    /// mapping — see its doc for both reasons (false sharing, and putting the
    /// byte within `disp32` reach of the code that polls it). Its position
    /// among these fields therefore no longer means anything; it used to have
    /// to be FIRST.
    pub stw_requested: CacheLineFlag,
    /// GC generation counter — incremented after each collection.
    /// Threads compare their local generation to detect missed GCs.
    pub gc_generation: AtomicU64,
    /// T19.H1 — count of threads currently parked in a *blocking* native
    /// operation (`Object.wait`, `Thread.sleep`, `LockSupport.park`,
    /// `Thread.join`, `ReferenceQueue.remove`, selector `select`, …).
    ///
    /// Such a thread is GC-safe: before blocking it deposits its frame
    /// roots into the thread-registry snapshot (`deposit_root_snapshot`),
    /// and on wake it applies the GC pointer map (`check_post_block_gc`).
    /// While parked it executes no code and cannot reach an interpreter
    /// safepoint to call `arrive_and_wait`.
    ///
    /// The stop-the-world barrier must therefore NOT include parked
    /// threads in `expected` — otherwise `wait_for_all` deadlocks waiting
    /// for a thread that is (correctly) blocked indefinitely, e.g. the
    /// Reference Handler parked in `ReferenceQueue.remove`. This is the
    /// JVM `_thread_blocked` state, scoped precisely to genuine blocking
    /// operations (NOT every native call — a *running* native still
    /// holds raw `ObjectRef`s in Rust locals and must be waited for so
    /// the copying collector does not relocate objects under it).
    ///
    /// Maintained by `enter_blocked` / `leave_blocked`, called from
    /// `deposit_root_snapshot` / `check_post_block_gc`.
    threads_blocked: AtomicU64,
    /// Protected coordination state.
    inner: Mutex<GcBarrierInner>,
    /// Signaled when all expected threads have arrived at the barrier.
    all_arrived: Condvar,
    /// Signaled when GC is complete and threads can resume.
    gc_complete: Condvar,
}

struct GcBarrierInner {
    /// Thread that initiated the current STW pause.
    initiator: Option<ThreadId>,
    /// Number of active (non-blocked) threads expected to arrive.
    expected: u32,
    /// Number of threads that have arrived at the barrier.
    arrived: u32,
    /// Pointer map from the last GC, shared with threads for frame updates.
    pointer_map: cratonvm_types::PointerMap,
    /// The generation `pointer_map` belongs to — the value `gc_generation`
    /// took when `complete_gc` stored it.
    ///
    /// A waiter releases on "the generation moved past the one I arrived for"
    /// and then clones whatever map is stored. Those are two different facts,
    /// and if they ever disagree the thread applies ANOTHER collection's
    /// relocations to its frames: every slot its own pause moved is left
    /// stale, which is a live object reachable only through an address the
    /// collector vacated. `CRATONVM_DBG_MAPGEN=1` reports the disagreement at
    /// the barrier instead of leaving it to surface as a wrong-class receiver
    /// an unbounded number of collections later.
    map_generation: u64,
    /// GCAUDIT-0711-FIX (finding 1a): identities (`ThreadId.0`) that THIS
    /// pause's `request_stw_counted_with_live_blocked` census read as
    /// `in_blocked_region == true` and therefore excluded from `expected`.
    ///
    /// Whether a given arrival should count toward the barrier's quota is
    /// otherwise UNDECIDABLE at the arrival site: `in_blocked_region`
    /// (`ThreadRegistry`) is set by `deposit_root_snapshot` and read by the
    /// census under a *different* lock (`ThreadRegistry::threads`) than the
    /// one guarding `arrived`/`expected` (`GcBarrier::inner`) — two plain
    /// racy atomics with no ordering relationship to each other. A caller
    /// that infers "was I excluded?" from its own last-known
    /// `in_blocked_region` value (or from `stw_requested` alone) can guess
    /// wrong in either direction: guessing "excluded" when the census
    /// actually counted it hangs `wait_for_all` forever; guessing
    /// "participating" when the census excluded it inflates `arrived` and
    /// releases the initiator while a real counted mutator is still
    /// running — a moving collector then evacuates live, still-mutating
    /// frames (the MTChurn lost-increment / BinaryTrees wrong-total / ES
    /// IVF-KNN Lucene-merge-thread family). Recording the census's actual
    /// per-pause decision here, under the SAME lock every arrival reads it
    /// through (`arrive_and_wait_auto`), makes the check race-free by
    /// construction instead of inferred.
    excluded_blocked: HashSet<u64>,
}

impl GcBarrier {
    /// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`) —
    /// stable address of the raw byte backing [`Self::stw_requested`], for
    /// baking into JIT-compiled code as an absolute poll target.
    ///
    /// `AtomicBool` has the same in-memory representation as `bool` (a
    /// single byte, `0` = false / nonzero = true), so a poll can read this
    /// address with a plain non-atomic byte load (`TEST byte ptr [addr],
    /// 0xFF`) and branch on nonzero — no atomic instruction is required at
    /// the poll site. A false-negative race (the poll observes `false` a
    /// few cycles before a concurrent `store(true, Release)` becomes
    /// globally visible) merely defers the thread noticing the request
    /// until its NEXT poll, the same latency bound the interpreter's own
    /// `stw_requested` poll (`vm/src/runtime/interpreter.rs`) already
    /// accepts.
    ///
    /// **Stability contract:** the returned address is valid for the rest of
    /// the process. It does not point into this `GcBarrier`, nor into the
    /// `Arc<SharedVm>` that owns it: [`CacheLineFlag`] holds a `&'static`
    /// into a cell that is never unmapped and never recycled, so a caller may
    /// bake it as an immediate into generated code without tracking the VM's
    /// lifetime at all.
    ///
    /// That is a widening of the old contract ("valid as long as this
    /// `GcBarrier` is alive; must not outlive the `Arc<SharedVm>` it came
    /// from"), which held only because `SharedVm` is `Arc`-allocated once by
    /// `vm/src/vm/vm_init.rs::Vm::new` and never moved thereafter. Under that
    /// contract a compiled poll surviving VM teardown read freed heap; it now
    /// reads a mapped byte that simply stops changing.
    ///
    /// It is not a licence to SHARE the address across VMs — each `GcBarrier`
    /// takes its own cell, and polling another VM's flag would be wrong for
    /// the ordinary reason, not an unsafe one.
    pub fn stw_requested_flag_addr(&self) -> *const u8 {
        self.stw_requested.flag() as *const AtomicBool as *const u8
    }

    /// Create a new GC barrier with no active STW.
    pub fn new() -> Self {
        Self {
            stw_requested: CacheLineFlag::new(false),
            gc_generation: AtomicU64::new(0),
            threads_blocked: AtomicU64::new(0),
            inner: Mutex::new(GcBarrierInner {
                initiator: None,
                expected: 0,
                arrived: 0,
                pointer_map: cratonvm_types::PointerMap::default(),
                map_generation: 0,
                excluded_blocked: HashSet::new(),
            }),
            all_arrived: Condvar::new(),
            gc_complete: Condvar::new(),
        }
    }

    /// Request a stop-the-world pause. Called by the GC-initiating thread.
    ///
    /// `alive_count` is the total number of alive threads (including the initiator).
    /// Returns `true` if the request was accepted, `false` if another STW is in progress.
    pub fn request_stw(&self, initiator: ThreadId, alive_count: u32) -> bool {
        self.request_stw_counted(initiator, || alive_count)
    }

    /// Request a stop-the-world pause, computing `alive_count` while the
    /// barrier transition lock is held.
    ///
    /// Legacy callers provide only the total alive-thread count, so this keeps
    /// the historical behaviour and subtracts the barrier's global blocked
    /// count as-is. Production GC paths should prefer
    /// [`Self::request_stw_counted_with_live_blocked`], which uses the registry
    /// publication made when a thread deposits its blocked-region roots. That
    /// prevents both stale global slots from lowering `expected` too far and
    /// newly blocked, already-snapshotted threads from being waited on before
    /// `enter_blocked` increments the anonymous global counter.
    ///
    /// This is the production entry point for VM GC initiators. It serializes
    /// the alive-thread snapshot with blocked-region transitions such as thread
    /// termination, avoiding mixed observations like "thread already dead" plus
    /// "same thread still counted blocked".
    pub fn request_stw_counted<F>(&self, initiator: ThreadId, alive_count: F) -> bool
    where
        F: FnOnce() -> u32,
    {
        let mut inner = self.inner.lock();
        let alive_count = alive_count();
        self.request_stw_counted_locked(&mut inner, initiator, alive_count, None, None)
    }

    /// Request a stop-the-world pause with both the alive-thread count and the
    /// subset of those alive threads that are currently published as blocked.
    ///
    /// `live_blocked_count` is the authoritative exclusion count for these
    /// callers: `in_blocked_region` is set only after the thread's root snapshot
    /// has been deposited, and it can become visible before `enter_blocked`
    /// increments the anonymous `threads_blocked` counter. The global counter is
    /// still useful for legacy callers and diagnostics, but production GC must
    /// key the expected mutator quota off the same registry state that owns the
    /// blocked root/fixup publication.
    ///
    /// `counts` also returns the IDENTITIES (`ThreadId.0`) of the alive
    /// threads currently published `in_blocked_region == true` — the exact
    /// set this request excludes from `expected`. Recorded in
    /// `excluded_blocked` so every arrival for this pause can look up its
    /// own exclusion status under the same lock (see
    /// [`Self::arrive_and_wait_auto`]) instead of guessing from a stale
    /// local flag.
    pub fn request_stw_counted_with_live_blocked<F>(&self, initiator: ThreadId, counts: F) -> bool
    where
        F: FnOnce() -> (u32, u32, Vec<u64>),
    {
        let mut inner = self.inner.lock();
        let (alive_count, live_blocked_count, blocked_tids) = counts();
        self.request_stw_counted_locked(
            &mut inner,
            initiator,
            alive_count,
            Some(live_blocked_count),
            Some(blocked_tids),
        )
    }

    fn request_stw_counted_locked(
        &self,
        inner: &mut GcBarrierInner,
        initiator: ThreadId,
        alive_count: u32,
        live_blocked_count: Option<u32>,
        excluded_blocked: Option<Vec<u64>>,
    ) -> bool {
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        inner.initiator = Some(initiator);
        // GCAUDIT-0711-FIX (finding 1a): publish the exact excluded-thread
        // set for THIS pause atomically with `expected`, under the same
        // lock every `arrive_and_wait_auto` call reads it through.
        inner.excluded_blocked = excluded_blocked
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        // T19.H1: exclude threads currently parked in a blocking native
        // from the set we wait for. They are GC-safe (roots already
        // deposited via `deposit_root_snapshot`) and execute no code, so
        // they will not reach an interpreter safepoint. Without this, a
        // STW initiated while e.g. the Reference Handler thread is parked
        // in `ReferenceQueue.remove` deadlocks `wait_for_all` forever.
        //
        // Race analysis (the count may change after this read):
        //  * blocked->running after the read: the waking thread runs
        //    `check_post_block_gc`, sees `stw_requested`, and waits the
        //    pause out. B2 fix: it does so via `arrive_and_wait_excluded`
        //    (or, once it has left the blocked region and become a counted
        //    mutator, via the participating `arrive_and_wait`). An excluded
        //    caller never bumps `arrived`, so it can no longer satisfy the
        //    `arrived >= expected` quota early and release `wait_for_all`
        //    while a counted mutator is still running.
        //  * running->blocked after the read: handled by `enter_blocked`,
        //    which, if a STW is already active, makes the thread
        //    arrive at the barrier *before* it parks, so the initiator
        //    is not left waiting for a thread that counted in `expected`
        //    and then vanished into a block.
        let blocked = self.threads_blocked.load(Ordering::Acquire);
        let blocked_u32 = u32::try_from(blocked).unwrap_or(u32::MAX);
        let live_blocked_for_log = live_blocked_count.unwrap_or(blocked_u32);
        let effective_blocked = live_blocked_count.unwrap_or(blocked_u32);
        inner.expected = alive_count
            .saturating_sub(1)
            .saturating_sub(effective_blocked);
        inner.arrived = 0;
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_CENSUS").is_some() {
            eprintln!(
                "[stw-request] initiator={} alive={} blocked={} live_blocked={} effective_blocked={} expected={}",
                initiator.0,
                alive_count,
                blocked_u32,
                live_blocked_for_log,
                effective_blocked,
                inner.expected
            );
        }
        // NOTE: do NOT clear `pointer_map` here. With generation-keyed waiting
        // (see `arrive_and_wait_inner`), a thread that arrived for the previous
        // generation may not read its remap map until after THIS `request_stw`
        // runs; clearing it would hand that thread an empty map and strand its
        // frame pointers at pre-GC (relocated) addresses. The map is overwritten
        // wholesale by the matching `complete_gc`, so a stale map never leaks
        // into the wrong generation.
        //
        // DO clear the cross-thread JIT coverage ledger, and do it here rather
        // than only in `begin_moving_young_coverage_cycle`. Every peer deposit
        // happens inside the window this store opens (a peer parks because it
        // observed `stw_requested`), so this is the one place in the process
        // that is guaranteed to run before the first deposit of a pause and
        // after the last deposit of the previous one — and it runs under the
        // same lock that publishes `expected`, so no peer can be mid-deposit
        // across it. `begin_moving_young_coverage_cycle` also clears it; the
        // duplication is deliberate, because only SOME collection paths call
        // that one and an over-count is the ledger's only unsound state.
        cratonvm_gc::gc_quiescence::reset_peer_proven_jit_depth();
        self.stw_requested.store(true, Ordering::Release);
        true
    }

    /// Run `f` only if no STW request is active, serialized with
    /// `request_stw_counted`'s expected-count snapshot.
    ///
    /// Startup threads use this to become STW-countable only when a
    /// concurrent request cannot have just excluded them from `expected`.
    pub fn run_if_no_stw_requested<F>(&self, f: F) -> bool
    where
        F: FnOnce(),
    {
        let _inner = self.inner.lock();
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        f();
        true
    }

    /// T19.H1 — mark the calling thread as entering a blocking native
    /// operation (about to park in `wait`/`park`/`sleep`/`select`/…) and
    /// return a [`BlockedGuard`] that clears the mark on drop.
    ///
    /// The guard makes the in-blocked accounting **leak-proof**: even if
    /// the blocking call returns `Err` via `?` or unwinds, `Drop` still
    /// decrements the counter, so `request_stw`'s `expected` can never
    /// drift permanently low (which would make GC stop waiting for live
    /// mutators).
    ///
    /// `pre_stw` on the returned guard is `true` if a stop-the-world
    /// pause was already in progress at the transition: the caller
    /// should then `arrive_and_wait` *before* parking, because
    /// `request_stw` may have counted this thread in `expected` before
    /// it became blocked.
    pub fn enter_blocked(&self) -> BlockedGuard<'_> {
        // Serialize the transition against `request_stw` (which computes
        // `expected` under the same lock): either our increment lands
        // BEFORE the count read (we are excluded AND `pre_stw` reads the
        // pre-request value `false`, so we park without arriving) or AFTER
        // (we are counted and `pre_stw=true` makes the caller arrive —
        // exactly once). Without the lock the two could interleave so that
        // an EXCLUDED thread arrives anyway; `arrived` is a plain counter,
        // so that spurious arrival releases `wait_for_all` while a counted
        // mutator is still running — a moving GC racing live frames.
        let inner = self.inner.lock();
        self.threads_blocked.fetch_add(1, Ordering::AcqRel);
        let pre_stw = self.stw_requested.load(Ordering::Acquire);
        drop(inner);
        // P1 shadow record. Deliberately after the lock drop: the shadow
        // registry's own lock must never be taken inside this critical
        // section's remaining span. See `threading::thread_state`.
        thread_state::record_transition(
            ThreadExecState::NativeBlocked,
            "gc_barrier::enter_blocked",
        );
        BlockedGuard {
            barrier: self,
            pre_stw,
        }
    }

    /// T19.H1 — non-guard variant of `enter_blocked` for native methods
    /// that open a blocking region via `NativeContext::begin_blocking_region`
    /// (e.g. `ReferenceQueue.remove`'s poll loop). Must be balanced by
    /// exactly one `mark_blocked_region_leave`.
    ///
    /// Returns `true` if a stop-the-world pause is already in progress
    /// (caller should `arrive_and_wait`).
    pub fn mark_blocked_region_enter(&self) -> bool {
        // Serialized against `request_stw` — see `enter_blocked` for the
        // exact-counting rationale.
        let inner = self.inner.lock();
        self.threads_blocked.fetch_add(1, Ordering::AcqRel);
        let pre_stw = self.stw_requested.load(Ordering::Acquire);
        drop(inner);
        // P1 shadow record. NOTE: unlike every `NativeContextImpl` caller,
        // `jni::host_thread_enter_native` (`vm/src/native/jni.rs:623`) reaches
        // here WITHOUT a preceding `deposit_root_snapshot`, so it raises the
        // anonymous counter without raising `in_blocked_region`. The shadow
        // state follows this counter, which is the superset — see
        // `docs/threading/thread-transition-states.md`, §"Unsound or
        // unmodelled transitions", item 1.
        thread_state::record_transition(
            ThreadExecState::NativeBlocked,
            "gc_barrier::mark_blocked_region_enter",
        );
        pre_stw
    }

    /// T19.H1 — end a region opened by `mark_blocked_region_enter`.
    ///
    /// Exact-counting contract (see `enter_blocked`): a thread may NOT
    /// transition blocked→running while a stop-the-world pause is active —
    /// it was EXCLUDED from that pause's `expected`, so the initiator will
    /// not wait for it, and arriving would inflate `arrived` for someone
    /// else's quota. Instead we wait the pause out while still counted as
    /// blocked (the GC maintains our roots via
    /// `fold_pointer_map_into_blocked`), and only then decrement — any
    /// LATER pause counts us in `expected` and we arrive exactly once via
    /// `check_post_block_gc`.
    pub fn mark_blocked_region_leave(&self) {
        self.mark_blocked_region_leave_after(|| {});
    }

    /// End a region opened by `mark_blocked_region_enter` after running `f`
    /// while holding the barrier transition lock.
    ///
    /// Use this when leaving the blocked population is coupled to another
    /// scheduler-visible state change, such as marking a thread dead. The two
    /// changes then serialize as one observation against `request_stw_counted`.
    pub fn mark_blocked_region_leave_after<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        let mut inner = self.inner.lock();
        Self::wait_out_pause_locked(
            self.stw_requested.flag(),
            &self.gc_generation,
            &self.gc_complete,
            &mut inner,
        );

        struct LeaveOnDrop<'a>(&'a GcBarrier);

        impl Drop for LeaveOnDrop<'_> {
            fn drop(&mut self) {
                self.0.threads_blocked.fetch_sub(1, Ordering::AcqRel);
            }
        }

        let leave = LeaveOnDrop(self);
        f();
        drop(leave);
    }

    /// Wait out the CURRENTLY-active stop-the-world pause (if any), keyed on the
    /// GC generation rather than the shared `stw_requested` flag.
    ///
    /// CRIT (multi-thread STW deadlock): a plain `while stw_requested` loop here
    /// would stall a thread across DISTINCT pauses — when the pause it entered on
    /// completes and a new one begins before it wakes, it observes `stw_requested`
    /// set again and waits forever, even though its own pause is long over. (At
    /// Churn shutdown this stranded the main thread in `BlockedGuard::drop` while
    /// the last worker's `System.gc` initiator waited on a thread that never
    /// arrived.) We instead capture the active pause's generation and return the
    /// instant it advances. If no pause is active we return immediately (the
    /// generation would otherwise never change → its own deadlock).
    #[inline]
    fn wait_out_pause_locked(
        stw_requested: &AtomicBool,
        gc_generation: &AtomicU64,
        gc_complete: &Condvar,
        inner: &mut parking_lot::MutexGuard<'_, GcBarrierInner>,
    ) {
        if !stw_requested.load(Ordering::Acquire) {
            return;
        }
        // P1 shadow record: this is a genuine parked window — the caller is
        // still counted blocked, executes no code, and its frames are
        // maintained by `fold_pointer_map_into_blocked`. Restore whatever the
        // caller was in afterwards rather than guessing, so the recorder never
        // invents a transition the code does not perform.
        let resume_state = thread_state::current_state();
        thread_state::record_transition(
            ThreadExecState::SafepointParked,
            "gc_barrier::wait_out_pause_locked",
        );
        let gen = gc_generation.load(Ordering::Acquire);
        while gc_generation.load(Ordering::Acquire) == gen {
            gc_complete.wait(inner);
        }
        thread_state::record_transition(resume_state, "gc_barrier::wait_out_pause_locked:resume");
    }

    /// Number of threads currently parked in a blocking native. Used by
    /// diagnostics and by the watchdog's hang report.
    pub fn blocked_count(&self) -> u64 {
        self.threads_blocked.load(Ordering::Acquire)
    }

    /// Finding 1(a/c) — atomically leave the blocked-region FLAG state:
    /// drain every active stop-the-world pause, then clear the caller's
    /// `in_blocked_region` flag under the SAME barrier-lock hold that
    /// confirmed no pause is active.
    ///
    /// Why this must be one atomic step: every pause requested while the flag
    /// was up EXCLUDED the caller from `expected` (identity census), so the
    /// caller may not resume bytecode while such a pause is mid-collection —
    /// but with a plain `store(false)` after a drain loop, a pause requested
    /// in the window between the drain's last check and the store both
    /// excludes the thread AND lets it run: the collector relocates objects
    /// under its live frames (finding 1's corruption family). Clearing under
    /// the lock closes the window exactly: a census serialized before the
    /// clear sees the flag up (excluded — we wait it out below), one
    /// serialized after sees it down (counted — the pause waits for our next
    /// safepoint arrival, so the caller's post-clear fixup application can
    /// never race a fold).
    ///
    /// While a pause is active we arrive with the census-aware (`auto`)
    /// participation rule: identity-census pauses excluded us (flag up) — we
    /// only wait; a legacy anonymous-count pause counted us (our
    /// `threads_blocked` slot was already released by the guard drop) — we
    /// arrive exactly once for it. Waiting is generation-keyed per pause
    /// (see `wait_out_pause_locked`'s rationale), and the re-check after each
    /// pause happens under the reacquired lock, so back-to-back pauses are
    /// each classified exactly.
    ///
    /// Starvation note: under continuous GC pressure this can wait out
    /// several consecutive pauses (each finite), but cannot livelock — a new
    /// pause can only be requested by a mutator holding this same lock, and
    /// parking_lot's eventual fairness bounds how long a woken waiter can be
    /// barged past. The parked WIP variant additionally applied the blocked
    /// fixup BEFORE this synchronization (racing the next pause's fold),
    /// which is the corruption this ordering eliminates.
    pub fn leave_blocked_region_flagged(
        &self,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
    ) {
        // P1 shadow record: this entry is always self-called for the caller's
        // own `tid` (the `initiator == Some(tid)` short-circuit below only
        // makes sense that way), so it is a safe binding point for a thread
        // that has not yet named itself to the recorder.
        thread_state::bind_current_thread(tid.0);
        let mut inner = self.inner.lock();
        loop {
            if !self.stw_requested.load(Ordering::Acquire) || inner.initiator == Some(tid) {
                flag.store(false, Ordering::Release);
                // This is the authoritative blocked -> running edge: the
                // identity flag drops under the same lock hold that proved no
                // pause is active. Recorded as `JavaRunning`; a caller that is
                // really resuming inside a native (`end_blocking_region_refs`)
                // records `NativeRunning` at its own next transition, and
                // `JavaRunning -> NativeRunning` is itself a tabled edge, so
                // the approximation cannot cascade into a false violation.
                thread_state::record_transition(
                    ThreadExecState::JavaRunning,
                    "gc_barrier::leave_blocked_region_flagged",
                );
                return;
            }
            // A pause is active. Arrive for it exactly once if its census
            // counted us (auto rule); either way wait its generation out.
            let participating = !inner.excluded_blocked.contains(&tid.0);
            let arrival_gen = self.gc_generation.load(Ordering::Acquire);
            if participating {
                inner.arrived += 1;
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_CENSUS").is_some() {
                    eprintln!(
                        "[stw-arrive] tid={} gen={} arrived={} expected={} (leave-blocked drain)",
                        tid.0, arrival_gen, inner.arrived, inner.expected
                    );
                }
                if inner.arrived >= inner.expected {
                    self.all_arrived.notify_all();
                }
            }
            // P1 shadow record: still flagged blocked, but genuinely parked
            // for the duration of this pause.
            thread_state::record_transition(
                ThreadExecState::SafepointParked,
                "gc_barrier::leave_blocked_region_flagged:drain",
            );
            while self.gc_generation.load(Ordering::Acquire) == arrival_gen {
                self.gc_complete.wait(&mut inner);
            }
            thread_state::record_transition(
                ThreadExecState::NativeBlocked,
                "gc_barrier::leave_blocked_region_flagged:drained",
            );
            // Lock reacquired by the condvar — reclassify from the top.
        }
    }

    /// Wait for all expected threads to arrive at the barrier.
    /// Called by the GC initiator after `request_stw`.
    pub fn wait_for_all(&self) {
        let mut inner = self.inner.lock();
        while inner.arrived < inner.expected {
            self.all_arrived.wait(&mut inner);
        }
    }

    /// BUG-03 — wait for all expected threads to arrive, but give up after
    /// `dur`. Returns `true` if the barrier was satisfied (`arrived >=
    /// expected`), `false` on timeout.
    ///
    /// Used by the cross-thread STW JIT root scan: the initiator forcibly
    /// stops in-JIT peers (which never arrive cooperatively) and excludes
    /// them via [`reduce_expected`], then loops on this bounded wait to pick
    /// up any thread that entered JIT *after* a previous take-over pass.
    pub fn wait_for_all_timeout(&self, dur: std::time::Duration) -> bool {
        let mut inner = self.inner.lock();
        if inner.arrived >= inner.expected {
            return true;
        }
        // parking_lot's `wait_for` may wake spuriously; one bounded wait is
        // enough because the caller loops. Re-check the predicate after waking.
        let _ = self.all_arrived.wait_for(&mut inner, dur);
        inner.arrived >= inner.expected
    }

    /// BUG-03 — remove `n` threads from the set the initiator is waiting for.
    ///
    /// Called when the initiator has taken over `n` in-JIT peers via OS
    /// suspension (see [`crate::jit::xt_root_scan`]): those threads are now
    /// frozen and conservatively scanned, so they will never arrive at the
    /// barrier and must not be counted in `expected` — otherwise
    /// `wait_for_all` would block on them forever. Saturates at zero and
    /// re-fires `all_arrived` in case the reduced quota is already met.
    pub fn reduce_expected(&self, n: u32) {
        if n == 0 {
            return;
        }
        let mut inner = self.inner.lock();
        inner.expected = inner.expected.saturating_sub(n);
        if inner.arrived >= inner.expected {
            self.all_arrived.notify_all();
        }
    }

    /// Signal that GC is complete and threads can resume.
    /// Called by the GC initiator after running collection.
    ///
    /// Stores the pointer map so threads can update their own frames.
    pub fn complete_gc(&self, pointer_map: cratonvm_types::PointerMap) {
        let mut inner = self.inner.lock();
        inner.pointer_map = pointer_map;
        inner.initiator = None;
        inner.excluded_blocked.clear();
        // Stamp the map with the generation it belongs to, under the same lock
        // that publishes it. See `Inner::map_generation`.
        inner.map_generation = self.gc_generation.load(Ordering::Acquire) + 1;
        self.gc_generation.fetch_add(1, Ordering::Release);
        self.stw_requested.store(false, Ordering::Release);
        self.gc_complete.notify_all();
    }

    /// Called by non-initiator threads at safepoints when STW is active.
    ///
    /// The thread signals its arrival, then waits for GC to complete.
    /// Returns the pointer map for updating this thread's frame references.
    ///
    /// This is the **participating** (counted) entry: the caller is a
    /// running mutator that `request_stw` included in `expected` (a genuine
    /// interpreter safepoint, or a thread whose `enter_blocked` reported
    /// `pre_stw == true` — it became blocked *after* `expected` was
    /// computed, so it was counted). Use [`arrive_and_wait_excluded`] for a
    /// thread that was NOT in `expected` (parked in a blocking native and
    /// excluded by `request_stw`); counting such a thread can satisfy the
    /// `arrived >= expected` quota early and release `wait_for_all` while a
    /// real mutator is still running.
    pub fn arrive_and_wait(&self, tid: ThreadId) -> cratonvm_types::PointerMap {
        self.arrive_and_wait_inner(tid, Some(true))
    }

    /// Like [`arrive_and_wait`], but for a thread that is **excluded** from
    /// the current STW's `expected` count (it was parked in a blocking
    /// native when `request_stw` ran, so it must not be counted toward
    /// `arrived`).
    ///
    /// B2 fix — the excluded thread still has to *wait out* the pause (so
    /// the moving collector never relocates objects under its live Rust
    /// `ObjectRef`s once it resumes), but it must NOT bump `arrived` or fire
    /// `all_arrived`: doing so can push `arrived` to `expected` while a
    /// counted mutator has not yet reached its safepoint, releasing the
    /// initiator's `wait_for_all` early and letting GC run under a live
    /// mutator.
    pub fn arrive_and_wait_excluded(&self, tid: ThreadId) -> cratonvm_types::PointerMap {
        self.arrive_and_wait_inner(tid, Some(false))
    }

    /// GCAUDIT-0711-FIX (finding 1a) — arrival for a caller whose
    /// participation status is NOT locally decidable: any site that may run
    /// either before OR after this thread's own `in_blocked_region` flag
    /// went up (the ambiguity described on [`GcBarrierInner::excluded_blocked`])
    /// must use this instead of guessing via `arrive_and_wait`/
    /// `arrive_and_wait_excluded`.
    ///
    /// Looks up `tid` in the CURRENT pause's `excluded_blocked` snapshot —
    /// populated by `request_stw_counted_locked` atomically with `expected`,
    /// under the same `inner` lock this read takes — so the decision exactly
    /// matches what the census actually did for this pause, race-free. Safe
    /// to use unconditionally in place of `arrive_and_wait`: a caller never
    /// in `excluded_blocked` gets identical (participating) behavior.
    pub fn arrive_and_wait_auto(&self, tid: ThreadId) -> cratonvm_types::PointerMap {
        self.arrive_and_wait_inner(tid, None)
    }

    /// Shared core for [`arrive_and_wait`] / [`arrive_and_wait_excluded`] /
    /// [`arrive_and_wait_auto`].
    ///
    /// `mode = Some(true)`/`Some(false)` forces participating/excluded
    /// unconditionally (used only by callers that can PROVE their status
    /// without consulting the pause's exclusion snapshot — see each
    /// wrapper's doc). `mode = None` decides from `excluded_blocked`,
    /// read under the same lock `request_stw_counted_locked` populated it
    /// under, which is race-free by construction.
    fn arrive_and_wait_inner(
        &self,
        tid: ThreadId,
        mode: Option<bool>,
    ) -> cratonvm_types::PointerMap {
        // P1 shadow record: always self-called for the caller's own `tid`
        // (see the initiator short-circuit below), so this is the primary
        // binding point for the recorder.
        thread_state::bind_current_thread(tid.0);
        let mut inner = self.inner.lock();
        // If this is the initiator or STW is not active, return immediately
        if !self.stw_requested.load(Ordering::Acquire) || inner.initiator == Some(tid) {
            return cratonvm_types::PointerMap::default();
        }
        // The state to restore when this pause ends. The barrier cannot know
        // whether the caller reached here from the interpreter poll, a
        // blocking-region entry, a startup retry loop or compiled code, so it
        // records the parked window and puts the caller back where it was
        // instead of inventing an edge.
        let resume_state = thread_state::current_state();
        let participating = mode.unwrap_or_else(|| !inner.excluded_blocked.contains(&tid.0));
        // Capture the generation of the STW we are arriving for (under the lock,
        // so `complete_gc` — which bumps the generation under the same lock —
        // cannot race between this and the `stw_requested` check above). We wait
        // until THIS generation completes, NOT until `stw_requested` clears.
        //
        // CRIT (multi-thread STW deadlock): `stw_requested` is a single shared
        // flag reused across STW cycles. If a thread arrives for generation G,
        // parks in the wait loop, and generation G completes AND a new
        // generation G+1 starts before this thread wakes, a `while stw_requested`
        // loop observes the flag set again (for G+1) and keeps waiting — yet this
        // thread never `arrive`d for G+1, so G+1's initiator's `wait_for_all`
        // blocks on it forever. Keying on the generation makes the thread return
        // the instant ITS pause ends; it then re-arrives for G+1 at its next
        // safepoint. (Reproduces with 6 threads each looping `System.gc()` —
        // scratch_churn/Churn.java.)
        let arrival_gen = self.gc_generation.load(Ordering::Acquire);
        // Signal arrival — but only for threads the initiator is actually
        // waiting for. An excluded (blocked) thread that wakes mid-STW must
        // not inflate `arrived`: it was never in `expected`, so counting it
        // could satisfy `arrived >= expected` before a counted mutator has
        // arrived and prematurely release `wait_for_all`.
        if participating {
            inner.arrived += 1;
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_CENSUS").is_some() {
                eprintln!(
                    "[stw-arrive] tid={} gen={} arrived={} expected={}",
                    tid.0, arrival_gen, inner.arrived, inner.expected
                );
            }
            if inner.arrived >= inner.expected {
                self.all_arrived.notify_all();
            }
        }
        thread_state::record_transition(
            ThreadExecState::SafepointParked,
            if participating {
                "gc_barrier::arrive_and_wait_inner:participating"
            } else {
                "gc_barrier::arrive_and_wait_inner:excluded"
            },
        );
        // Wait until THIS pause completes (its generation is published by
        // `complete_gc`), not merely until `stw_requested` clears — see above.
        while self.gc_generation.load(Ordering::Acquire) == arrival_gen {
            self.gc_complete.wait(&mut inner);
        }
        thread_state::record_transition(resume_state, "gc_barrier::arrive_and_wait_inner:resume");
        // The map this thread is about to apply must be the map of the pause it
        // arrived for. The release condition ("the generation moved") and the
        // map it then reads are two different facts — see `Inner::map_generation`.
        if inner.map_generation != arrival_gen + 1
            && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MAPGEN").is_some()
        {
            eprintln!(
                "[mapgen] tid={} arrived_for_gen={} but the stored map is gen={}                  (map_len={}) — this thread's own pause's relocations are NOT in it",
                tid.0,
                arrival_gen + 1,
                inner.map_generation,
                inner.pointer_map.len(),
            );
        }
        inner.pointer_map.clone()
    }

    /// Get the number of expected threads still outstanding.
    /// Used for diagnostics/testing.
    pub fn pending_count(&self) -> u32 {
        let inner = self.inner.lock();
        inner.expected.saturating_sub(inner.arrived)
    }

    /// Request a brief STW pause for concurrent GC phases (initial mark / remark).
    ///
    /// Unlike a full `request_stw` + `complete_gc`, this is designed for
    /// short pauses where the initiator runs a quick marking phase and then
    /// immediately releases all threads.
    ///
    /// Returns `true` if the STW was successfully acquired.
    pub fn brief_stw<F>(&self, initiator: ThreadId, alive_count: u32, work: F) -> bool
    where
        F: FnOnce(),
    {
        self.brief_stw_counted(initiator, || alive_count, work)
    }

    /// [`brief_stw`] variant that computes the alive-thread count under the
    /// barrier transition lock.
    pub fn brief_stw_counted<C, F>(&self, initiator: ThreadId, alive_count: C, work: F) -> bool
    where
        C: FnOnce() -> u32,
        F: FnOnce(),
    {
        if !self.request_stw_counted(initiator, alive_count) {
            return false;
        }
        self.wait_for_all();
        work();
        self.complete_gc(cratonvm_types::PointerMap::default());
        true
    }

    /// [`brief_stw_counted`] variant that uses the registry's published live
    /// blocked-thread count. See
    /// [`Self::request_stw_counted_with_live_blocked`].
    pub fn brief_stw_counted_with_live_blocked<C, F>(
        &self,
        initiator: ThreadId,
        counts: C,
        work: F,
    ) -> bool
    where
        C: FnOnce() -> (u32, u32, Vec<u64>),
        F: FnOnce(),
    {
        if !self.request_stw_counted_with_live_blocked(initiator, counts) {
            return false;
        }
        self.wait_for_all();
        work();
        self.complete_gc(cratonvm_types::PointerMap::default());
        true
    }
}

impl Default for GcBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// T19.H1 — RAII guard returned by [`GcBarrier::enter_blocked`].
///
/// Holding the guard means "this thread is parked in a blocking native
/// and is GC-safe". Dropping it (normal return, `?` early-return, or
/// unwind) decrements the barrier's blocked-thread count, so the
/// accounting can never leak.
#[must_use = "dropping the guard immediately ends the blocked state"]
pub struct BlockedGuard<'a> {
    barrier: &'a GcBarrier,
    /// `true` if a stop-the-world pause was already active when the
    /// thread entered the blocked state. The caller should arrive at the
    /// barrier before parking — see `GcBarrier::enter_blocked`.
    pub pre_stw: bool,
}

impl<'a> BlockedGuard<'a> {
    /// Finish this blocked region after running `f` while holding the same
    /// barrier transition lock used by `request_stw_counted`.
    ///
    /// This is for transitions that must be observed atomically with leaving
    /// the blocked population. Thread termination uses it to flip
    /// `alive=false` and decrement `threads_blocked` as one state change; a GC
    /// initiator can no longer see a dead thread still included in the blocked
    /// count and under-estimate `expected`.
    pub fn finish_after<F>(self, f: F)
    where
        F: FnOnce(),
    {
        let this = std::mem::ManuallyDrop::new(self);
        this.barrier.mark_blocked_region_leave_after(f);
    }
}

impl Drop for BlockedGuard<'_> {
    fn drop(&mut self) {
        // Checked leave — identical contract to `mark_blocked_region_leave`:
        // wait out the active stop-the-world pause we were excluded from
        // (arriving or running would both be wrong) before re-entering the
        // mutator population. Keyed on the GC generation, NOT `stw_requested`,
        // so a back-to-back new pause cannot strand this thread forever — see
        // `GcBarrier::wait_out_pause_locked`.
        let mut inner = self.barrier.inner.lock();
        GcBarrier::wait_out_pause_locked(
            self.barrier.stw_requested.flag(),
            &self.barrier.gc_generation,
            &self.barrier.gc_complete,
            &mut inner,
        );
        self.barrier.threads_blocked.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for GcBarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcBarrier")
            .field("stw_active", &self.stw_requested.load(Ordering::Relaxed))
            .field("generation", &self.gc_generation.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn barrier_no_stw_by_default() {
        let barrier = GcBarrier::new();
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 0);
    }

    /// Cooperative JIT safepoint polling — the flag must sit within `disp32`
    /// reach of the JIT code cache, so `emit_safepoint_poll` can use its
    /// one-instruction `TEST BYTE [rip+disp32], 0xFF` form.
    ///
    /// The sibling of `platform::tests::a_cell_is_within_disp32_of_a_code_buffer`,
    /// which asserts the same thing about a bare cell. This one asserts it
    /// about the byte that is actually polled, reached the way the JIT reaches
    /// it (`stw_requested_flag_addr`), so it also fails if `CacheLineFlag` ever
    /// goes back to being an inline field of this `Arc`-allocated struct — the
    /// state that made every compiled poll in the process 15 bytes and a
    /// clobbered R11 instead of 7 bytes and none.
    ///
    /// x86-64 only; `disp32` reach is what makes the distance matter.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn stw_requested_flag_is_within_disp32_of_the_code_cache() {
        use cratonvm_jit::platform::{alloc_executable, free_executable};

        let barrier = GcBarrier::new();
        let flag = barrier.stw_requested_flag_addr() as usize;
        let code = alloc_executable(4096).expect("alloc_executable failed");
        // Widening: usize -> i128, so the subtraction cannot wrap.
        let delta = (flag as i128) - (code as usize as i128);
        let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
        free_executable(code, 4096);
        assert!(
            in_reach,
            "stw_requested flag {flag:#x} is {:.1} GB from a code buffer at \
             {:#x}; every compiled safepoint poll falls back to \
             MOV R11, imm64 ; TEST BYTE [R11], 0xFF",
            (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
            code as usize,
        );
    }

    /// Cooperative JIT safepoint polling — the raw byte address must alias
    /// `stw_requested` exactly: a plain byte read through the pointer must
    /// track every transition the atomic makes.
    #[test]
    fn stw_requested_flag_addr_aliases_the_atomic_byte() {
        let barrier = GcBarrier::new();
        let addr = barrier.stw_requested_flag_addr();
        assert!(!addr.is_null());
        // SAFETY: `addr` points at `barrier.stw_requested`, which is alive
        // for the whole scope of this test.
        assert_eq!(unsafe { *addr }, 0, "expected false (0) initially");

        assert!(barrier.request_stw(ThreadId(0), 1));
        // SAFETY: same as above.
        assert_ne!(unsafe { *addr }, 0, "expected nonzero after request_stw");

        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        // SAFETY: same as above.
        assert_eq!(unsafe { *addr }, 0, "expected false (0) after complete_gc");
    }

    #[test]
    fn barrier_request_and_complete_single_thread() {
        let barrier = GcBarrier::new();
        // Single alive thread (the initiator) → expected = 0
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(barrier.stw_requested.load(Ordering::Relaxed));

        // No other threads to wait for
        barrier.wait_for_all();

        // Complete with empty pointer map
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn barrier_two_threads() {
        let barrier = Arc::new(GcBarrier::new());

        // Request STW with 2 alive threads
        assert!(barrier.request_stw(ThreadId(0), 2));

        let barrier2 = barrier.clone();
        let handle = std::thread::spawn(move || {
            // Non-initiator thread arrives at safepoint
            barrier2.arrive_and_wait(ThreadId(1))
        });

        // Initiator waits for the other thread
        barrier.wait_for_all();

        // Complete GC with a pointer remap
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        barrier.complete_gc(pm);

        // Non-initiator thread should receive the pointer map
        let result_map = handle.join().unwrap();
        assert_eq!(result_map.get(&0x1000), Some(&0x2000));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn barrier_duplicate_request_rejected() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Second request while first is active should fail
        assert!(!barrier.request_stw(ThreadId(1), 2));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn barrier_initiator_not_blocked() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Initiator calling arrive_and_wait should return immediately
        let map = barrier.arrive_and_wait(ThreadId(0));
        assert!(map.is_empty());
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// B2 — an EXCLUDED (blocked) thread that wakes mid-STW and calls
    /// `arrive_and_wait_excluded` must NOT count toward `arrived`, so it
    /// cannot satisfy the quota and release the initiator's `wait_for_all`
    /// while the genuine counted mutator has not yet arrived.
    ///
    /// Scenario: 3 alive threads — initiator I, counted mutator M, blocked
    /// (excluded) thread B. `request_stw` is told 1 thread is blocked, so
    /// `expected = 3 - 1 (initiator) - 1 (blocked) = 1` (just M).
    #[test]
    fn barrier_excluded_thread_does_not_release_early() {
        let barrier = Arc::new(GcBarrier::new());
        // Pretend B is already parked in a blocking native.
        barrier.threads_blocked.store(1, Ordering::Release);
        assert!(barrier.request_stw(ThreadId(0), 3));
        // expected counts only the running mutator M.
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }

        // B (excluded) wakes mid-STW and drains via the excluded entry.
        let b = barrier.clone();
        let hb = std::thread::spawn(move || b.arrive_and_wait_excluded(ThreadId(2)));

        // Give B time to run its (non-counting) arrival.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // The excluded arrival must NOT have satisfied the quota: M (the
        // only counted thread) has not arrived, so `arrived` stays 0 and
        // `wait_for_all` would still block. `pending_count` proves it.
        assert_eq!(
            barrier.pending_count(),
            1,
            "excluded thread must not be counted toward arrived",
        );

        // Now the counted mutator M arrives — quota satisfied.
        let m = barrier.clone();
        let hm = std::thread::spawn(move || m.arrive_and_wait(ThreadId(1)));

        // The initiator can now proceed.
        barrier.wait_for_all();
        assert_eq!(barrier.pending_count(), 0);

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = hm.join();
        let _ = hb.join();
    }

    /// B2 — sanity: with no excluded threads, the participating entry still
    /// counts and releases exactly as before.
    #[test]
    fn barrier_participating_entry_counts() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b2 = barrier.clone();
        let h = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(1)));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join();
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    /// BUG-03 — cross-thread JIT takeover may discover an in-JIT mutator only
    /// after a bounded wait has already timed out. Reducing `expected` after
    /// such a timeout must release the initiator once all non-cooperative peers
    /// have been removed from the quota.
    #[test]
    fn barrier_late_reduce_expected_can_satisfy_bounded_wait() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 3));

        assert!(
            !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "two non-initiator mutators are still pending"
        );

        barrier.reduce_expected(1);
        assert!(
            !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "one mutator is still pending after the first takeover"
        );

        barrier.reduce_expected(1);
        assert!(
            barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "late takeover should satisfy the reduced barrier quota"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn blocked_dead_transition_is_observed_atomically() {
        let barrier = GcBarrier::new();
        let guard = barrier.enter_blocked();
        let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

        guard.finish_after(|| {
            alive = 2; // the terminating thread is now dead
        });

        assert!(barrier.request_stw_counted(ThreadId(0), || alive));
        {
            let inner = barrier.inner.lock();
            assert_eq!(
                inner.expected, 1,
                "dead blocked thread must not be subtracted from the live mutator quota",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn manual_blocked_dead_transition_is_observed_atomically() {
        let barrier = GcBarrier::new();
        let _pre_stw = barrier.mark_blocked_region_enter();
        let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

        barrier.mark_blocked_region_leave_after(|| {
            alive = 2; // the terminating blocked thread is now dead
        });

        assert!(barrier.request_stw_counted(ThreadId(0), || alive));
        {
            let inner = barrier.inner.lock();
            assert_eq!(
                inner.expected, 1,
                "manual dead blocked thread must not be subtracted from the live mutator quota",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
    #[test]
    fn counted_request_ignores_stale_global_blocked_slots() {
        let barrier = GcBarrier::new();
        // Simulate a leaked/stale blocked slot not represented by any live
        // blocked registry entry. A plain global subtraction would set
        // expected=0 for two alive threads and let GC proceed without waiting
        // for the one live peer.
        barrier.threads_blocked.store(1, Ordering::Release);

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn counted_request_uses_live_blocked_before_global_counter_increment() {
        let barrier = GcBarrier::new();
        // A blocking thread deposits roots and sets its registry blocked flag
        // before `enter_blocked` increments the anonymous global count. Once the
        // root snapshot is published, STW must exclude it even if the global
        // counter still reads zero.
        barrier.threads_blocked.store(0, Ordering::Release);

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 2, vec![2, 3])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// GCAUDIT-0711-FIX (finding 1a) — `arrive_and_wait_auto` must resolve
    /// EXACTLY like `arrive_and_wait_excluded` for a thread the pause's
    /// census actually excluded, even though the caller itself cannot prove
    /// that locally (see `GcBarrierInner::excluded_blocked`'s doc for why
    /// `pre_stw`/`in_blocked_region` alone are ambiguous at the call site).
    #[test]
    fn auto_arrival_excluded_thread_does_not_release_early() {
        let barrier = Arc::new(GcBarrier::new());
        // 3 alive: initiator I(0), counted mutator M(1), blocked B(2).
        // The census reports B's identity as excluded.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 1, vec![2])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1, "only M should be counted");
        }

        let b = barrier.clone();
        let hb = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(2)));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            barrier.pending_count(),
            1,
            "auto arrival for an excluded tid must not satisfy the quota",
        );

        let m = barrier.clone();
        let hm = std::thread::spawn(move || m.arrive_and_wait_auto(ThreadId(1)));
        barrier.wait_for_all();
        assert_eq!(barrier.pending_count(), 0);

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = hm.join();
        let _ = hb.join();
    }

    /// GCAUDIT-0711-FIX (finding 1a) — sanity: with no excluded threads,
    /// `arrive_and_wait_auto` counts exactly like `arrive_and_wait`.
    #[test]
    fn auto_arrival_participating_by_default() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        let b2 = barrier.clone();
        let h = std::thread::spawn(move || b2.arrive_and_wait_auto(ThreadId(1)));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join();
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    /// GCAUDIT-0711-FIX (finding 1a) — `excluded_blocked` must not leak
    /// across generations: a tid excluded by pause G must be treated as
    /// participating by default in pause G+1 unless that pause's own
    /// census excludes it again.
    #[test]
    fn excluded_blocked_does_not_leak_across_generations() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![1])));
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        {
            let inner = barrier.inner.lock();
            assert!(
                !inner.excluded_blocked.contains(&1),
                "stale exclusion from a completed pause must not survive complete_gc",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Finding 1(c) — `leave_blocked_region_flagged` must not clear the
    /// caller's `in_blocked_region` flag while a pause that excluded the
    /// caller is still active; the clear lands only once no pause is active,
    /// with no start-and-complete window in between.
    #[test]
    fn leave_blocked_region_flagged_waits_out_active_pause() {
        use std::sync::atomic::AtomicBool;

        let barrier = Arc::new(GcBarrier::new());
        let flag = Arc::new(AtomicBool::new(true));

        // Pause active; thread 2 is excluded by the identity census.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));

        let b = barrier.clone();
        let f = flag.clone();
        let h = std::thread::spawn(move || b.leave_blocked_region_flagged(ThreadId(2), &f));

        // While the pause is active the flag must stay up (the thread is
        // waiting the pause out, not resuming).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            flag.load(Ordering::Acquire),
            "flag must not clear while an excluding pause is active",
        );
        // The excluded leave must not have satisfied any quota either.
        assert_eq!(
            barrier.pending_count(),
            0,
            "expected was 0 (only initiator + excluded)"
        );

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        h.join().unwrap();
        assert!(
            !flag.load(Ordering::Acquire),
            "flag clears once no pause is active",
        );
    }

    /// `leave_blocked_region_flagged` with no active pause clears the flag
    /// immediately and never blocks.
    #[test]
    fn leave_blocked_region_flagged_immediate_when_idle() {
        use std::sync::atomic::AtomicBool;
        let barrier = GcBarrier::new();
        let flag = AtomicBool::new(true);
        barrier.leave_blocked_region_flagged(ThreadId(7), &flag);
        assert!(!flag.load(Ordering::Acquire));
    }
}
